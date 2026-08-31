//! The `efd` daemon core (M3): acquire the index, keep it live via the M2 watch loop, and
//! serve searches/status over the named pipe.
//!
//! ## Concurrency
//! The whole [`Watched`] (index + journal cursor) lives behind one `Arc<RwLock<Watched>>`.
//! Client handlers take the **read** lock for `search`/`path`/`stats`; readers run
//! concurrently. The single watch thread takes the **write** lock only to apply journal
//! events (and to swap in a rebuilt index on recovery). A snapshot save takes the read lock
//! (it only serializes), so it blocks the next apply but not searches. An event burst holds
//! the write lock for the apply (~36 ms for 10k events, M2): the accepted worst-case stall.
//! No arc-swap / double-buffering until a measurement demands it.
//!
//! `Meta` (behind a `Mutex`) holds status-only fields the watch thread refreshes each poll.
//! Lock order is always **`Watched` then `Meta`**, and the watch thread never holds `Watched`
//! while locking `Meta`, so the two locks cannot deadlock.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::index::flags;
use crate::ipc::acl::AclMode;
use crate::ipc::pipe::PipeStream;
use crate::ipc::server::PipeServer;
use crate::ipc::{
    read_request, write_response, DuRowWire, ErrCode, Request, Response, SearchHit, StatusReport,
    WireError, MAX_DU_DEPTH, MAX_DU_ROWS, MAX_SEARCH_LIMIT, PIPE_NAME, PROTO_VERSION,
};
use crate::snapshot;
use crate::sysinfo::current_memory;
use crate::volume::{UsnVolume, Win32Volume};
use crate::watch::{Synced, Watched};

/// Maximum concurrent pipe instances / handler threads. A
/// local client with no read timeout can hold a handler; the cap bounds the exposure.
pub const MAX_PIPE_INSTANCES: u32 = 64;

/// Process-wide shutdown flag, set by the console Ctrl-C handler ([`install_ctrl_handler`]) or
/// the service control handler (M3 step 6), and polled by the watch loop.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Request a graceful shutdown (final save + exit). Idempotent.
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Whether a shutdown has been requested. Used by the volume to abort a long enumeration.
pub fn is_shutdown_requested() -> bool {
    shutting_down()
}

fn shutting_down() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

/// Daemon configuration (from CLI flags or, under the service, the baked-in service arguments).
#[derive(Clone)]
pub struct DaemonConfig {
    /// Target volume argument, e.g. `C:`.
    pub volume: String,
    /// Pipe DACL mode.
    pub acl: AclMode,
    /// Snapshot path (absolute). Resumed on startup if present, saved periodically + on stop.
    pub snapshot_path: PathBuf,
    /// Journal poll interval.
    pub poll_interval: Duration,
    /// Arena-garbage fraction above which the index is rebuilt.
    pub garbage_threshold: f64,
    /// Minimum spacing between periodic snapshot writes.
    pub save_interval: Duration,
}

/// Status-only metadata, refreshed by the watch thread and read by the `Status` handler.
struct Meta {
    drive: char,
    poll_interval_ms: u64,
    started: Instant,
    /// Volume `next_usn` as of the last poll (for the lag figure).
    volume_next_usn: u64,
    last_sync: Instant,
    snapshot_generation: u64,
    last_snapshot: Option<Instant>,
    snapshot_cursor: u64,
}

impl Meta {
    fn new(drive: char, poll_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            drive,
            poll_interval_ms: poll_interval.as_millis() as u64,
            started: now,
            volume_next_usn: 0,
            last_sync: now,
            snapshot_generation: 0,
            last_snapshot: None,
            snapshot_cursor: 0,
        }
    }
}

/// Startup phase, so a search that arrives during the initial enumeration gets
/// a "still building" answer instead of an empty result (which reads as
/// "broken"). Serving begins the instant the pipe binds; the index fills in on
/// a background thread and flips this to `Ready`.
pub mod phase {
    /// Building the initial index (fresh enumeration in progress).
    pub const BUILDING: u8 = 0;
    /// Index ready: normal serving.
    pub const READY: u8 = 1;
}

/// Shared daemon state.
struct Shared {
    /// The live index. Empty (a placeholder) until the background acquire
    /// finishes and swaps the real one in under the write lock.
    watched: RwLock<Watched>,
    meta: Mutex<Meta>,
    /// `phase::*`: BUILDING until the initial index is in place.
    phase: std::sync::atomic::AtomicU8,
    /// Entries enumerated so far during the initial build (progress for the
    /// "building" status). Meaningful only while `phase == BUILDING`.
    build_progress: std::sync::atomic::AtomicUsize,
}

fn read_watched(shared: &Shared) -> RwLockReadGuard<'_, Watched> {
    // Ignore lock poisoning: search/apply are panic-free, and a poisoned lock must not take the
    // daemon down: recover the guard and keep serving.
    shared.watched.read().unwrap_or_else(|e| e.into_inner())
}

fn write_watched(shared: &Shared) -> RwLockWriteGuard<'_, Watched> {
    shared.watched.write().unwrap_or_else(|e| e.into_inner())
}

/// Run the daemon: acquire the index, bind the pipe, start the accept + watch loops, and block
/// until a shutdown is requested (then save and return). `foreground` installs the Ctrl-C
/// handler and logs to the console. `ready` is set to `true` once the pipe is listening (the
/// service uses it to transition from `StartPending` to `Running`).
pub fn run(cfg: DaemonConfig, foreground: bool, ready: Arc<AtomicBool>) -> Result<()> {
    let mut volume =
        Win32Volume::open(&cfg.volume).with_context(|| format!("opening volume {}", cfg.volume))?;
    let drive = volume.drive();

    // Abort a long initial enumeration promptly if a STOP arrives.
    volume.set_cancel(is_shutdown_requested);

    // Start serving immediately with an empty placeholder index, phase=BUILDING.
    // A search that arrives during the (possibly 20-40 s) initial enumeration
    // then gets a "still building" answer instead of an empty result that reads
    // as "broken": the bug that made a right-click search look dead on a cold
    // start. The real index is acquired below and swapped in under the write
    // lock before the watch loop begins.
    let shared = Arc::new(Shared {
        watched: RwLock::new(Watched::placeholder(drive)),
        meta: Mutex::new(Meta::new(drive, cfg.poll_interval)),
        phase: std::sync::atomic::AtomicU8::new(phase::BUILDING),
        build_progress: std::sync::atomic::AtomicUsize::new(0),
    });

    // Bind the pipe before acquiring; fail loudly if the name is already held.
    let server = PipeServer::bind(PIPE_NAME, cfg.acl, MAX_PIPE_INSTANCES)
        .context("binding the daemon pipe")?;
    ready.store(true, Ordering::SeqCst);
    tracing::info!(pipe = PIPE_NAME, acl = ?cfg.acl, "listening (index building)");

    // Accept loop on its own thread; each connection is served by a short-lived worker.
    let accept_shared = Arc::clone(&shared);
    let accept = thread::Builder::new()
        .name("efd-accept".into())
        .spawn(move || accept_loop(server, accept_shared))
        .context("spawning the accept thread")?;

    if foreground {
        install_ctrl_handler().context("installing the Ctrl-C handler")?;
        tracing::info!("running in the foreground; Ctrl-C to stop");
    }

    // Acquire the real index (resume snapshot or full enumeration), reporting
    // progress into `build_progress` so a concurrent `ef status` shows it.
    let acquire_start = Instant::now();
    let resumed_from_snapshot;
    {
        let progress = Arc::clone(&shared);
        volume.set_progress(move |n| {
            progress
                .build_progress
                .store(n, std::sync::atomic::Ordering::Relaxed);
        });
        let acquired = match acquire(&mut volume, drive, &cfg.snapshot_path) {
            Ok(a) => a,
            Err(_) if shutting_down() => {
                tracing::info!("enumeration cancelled by a stop request during startup");
                drop(accept);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        volume.clear_progress();
        resumed_from_snapshot = acquired.resumed;
        // Swap the real index in and flip to READY. Searches taken between here
        // and now saw BUILDING; from this point they hit the full index.
        *write_watched(&shared) = acquired.watched;
        shared
            .phase
            .store(phase::READY, std::sync::atomic::Ordering::SeqCst);
    }
    tracing::info!(
        entries = read_watched(&shared).index.len(),
        elapsed = ?acquire_start.elapsed(),
        "index ready"
    );

    // First-run snapshot: a fresh enumeration leaves the index unsaved (it is
    // not a "change", so the watch loop's dirty flag never trips on a quiet
    // machine: the index would never persist and every restart would
    // re-enumerate). Persist it once now so the next start resumes in ~1 s.
    if !resumed_from_snapshot && !shutting_down() {
        match save_snapshot(&shared, &cfg.snapshot_path) {
            Ok(()) => tracing::info!(path = %cfg.snapshot_path.display(), "initial snapshot saved"),
            Err(e) => tracing::warn!("initial snapshot save failed: {e:#}"),
        }
    }

    // Watch loop on this thread; returns after the final save when shutdown is requested.
    watch_loop(&mut volume, &shared, &cfg);

    tracing::info!("shutting down");
    // The accept thread is blocked in ConnectNamedPipe; it is abandoned as the process exits.
    drop(accept);
    Ok(())
}

/// A freshly acquired index plus whether it came from a snapshot (so the caller
/// can skip the first-run save when resuming).
struct Acquired {
    watched: Watched,
    resumed: bool,
}

/// Acquire the initial index: resume the snapshot if present and valid, else full enumeration;
/// then catch up the journal delta.
fn acquire(vol: &mut Win32Volume, drive: char, snapshot_path: &Path) -> Result<Acquired> {
    let (mut state, resumed) = if snapshot_path.exists() {
        match snapshot::load(snapshot_path) {
            Ok(loaded) => {
                tracing::info!(path = %snapshot_path.display(), "resuming from snapshot");
                (Watched::from_snapshot(loaded, vol, drive)?, true)
            }
            Err(e) => {
                tracing::warn!("snapshot load failed ({e:#}); enumerating instead");
                (Watched::enumerate(vol, drive)?, false)
            }
        }
    } else {
        tracing::info!("no snapshot; enumerating");
        (Watched::enumerate(vol, drive)?, false)
    };
    // Nothing is serving yet, so recovery here is just done in place.
    match state.sync(vol).context("catching up the journal")? {
        Synced::Applied(a) => {
            state.refresh_sizes(&*vol, &a.resized);
            if !state.index.fold_map_ready() {
                state.index.rebuild_fold_map();
            }
            if a.events > 0 {
                tracing::info!(caught_up = a.events, "applied journal delta since acquire");
            }
        }
        Synced::Discontinuity(why) => {
            tracing::warn!(why, "re-enumerating before serving");
            state.recover(vol).context("re-enumerating")?;
        }
    }
    // A resume can still fall back to enumeration inside `from_snapshot` (stale
    // journal); treat "the snapshot was actually usable" as resumed. If it
    // re-enumerated, the cursor differs from the loaded one, but simplest and
    // safe: only skip the first-run save when we truly loaded a snapshot and it
    // was accepted. `from_snapshot` returning without error covers that.
    Ok(Acquired {
        watched: state,
        resumed,
    })
}

/// Poll the journal, apply changes, compact on garbage, and snapshot periodically, until a
/// shutdown is requested, then take a final best-effort snapshot.
fn watch_loop(vol: &mut Win32Volume, shared: &Shared, cfg: &DaemonConfig) {
    let mut last_save = Instant::now();
    let mut dirty = false;

    loop {
        if shutting_down() {
            if let Err(e) = save_snapshot(shared, &cfg.snapshot_path) {
                tracing::warn!("final snapshot save failed: {e:#}");
            } else {
                tracing::info!(path = %cfg.snapshot_path.display(), "final snapshot saved");
            }
            return;
        }

        // Every job below is split the same way, and for the same reason: this lock is
        // **writer-preferring** (measured: a queued writer blocks readers that arrive after
        // it, even while the lock is merely read-held), so the watch thread taking it once a
        // second means every millisecond it holds it is a millisecond of searches that cannot
        // start. What has to be exclusive is held exclusively; everything else (the file
        // stats, the parallel sort, the enumeration, the logging) runs beside it.
        //
        // Note the binding: `let x = write_watched(..)...;` drops the guard at the end of the
        // statement. Writing this as `match write_watched(..).sync(vol) { ... }` instead keeps
        // it for the whole `match`, which would put the `tracing` calls below (synchronous,
        // unbuffered file writes under the service) inside the exclusive section.
        let synced = write_watched(shared).sync(vol);

        let mut rebuild_because = match synced {
            Ok(Synced::Applied(a)) => {
                if a.events > 0 {
                    dirty = true;
                    tracing::info!(applied = a.events, "applied journal changes");
                }
                // Stat the changed files off the lock, then write the sizes back under it.
                let measured = Watched::measure_sizes(vol, &a.resized);
                if !measured.is_empty() {
                    write_watched(shared).record_sizes(&measured);
                }
                // A directory that moved across the orphan boundary left its subtree behind on
                // the wrong side. Re-derived here, in the poll that noticed, so a stale flag
                // never reaches a search or a snapshot. Exclusive because it rewrites flags,
                // and O(n), but only on the rare event that moves a directory across.
                if read_watched(shared).index.orphans_stale() {
                    tracing::info!("a directory changed orphan status; re-deriving the subtree");
                    write_watched(shared).index.refresh_orphans();
                }
                // Same shape for the contig hit map, which a create or rename empties: the
                // parallel sort is a read, so it runs under the shared lock alongside
                // searches, and only the move into place is exclusive.
                if a.events > 0 {
                    let order = {
                        let w = read_watched(shared);
                        (!w.index.fold_map_ready()).then(|| w.index.fold_order())
                    };
                    if let Some(order) = order {
                        write_watched(shared).index.adopt_fold_map(order);
                    }
                }
                None
            }
            Ok(Synced::Discontinuity(why)) => Some(why),
            Err(e) => {
                tracing::error!("journal sync failed: {e:#}");
                None
            }
        };
        if rebuild_because.is_none() && read_watched(shared).compaction_due(cfg.garbage_threshold) {
            rebuild_because = Some("arena garbage crossed the threshold");
        }

        // The expensive half (a fresh enumeration of every record on the volume) with no
        // lock held at all. It takes 20-40 s on six million entries, and it used to take them
        // under the write lock: every search on the machine blocked for the duration, long
        // past the point where the Explorer shim gives up and hands the search back to
        // Windows, which then crawls an unindexed drive. Built beside the live index instead,
        // the only exclusive moment is the swap, and searches answer from the old contents
        // throughout. Nothing is lost by serving those: the cursor is captured *before* the
        // enumeration, so whatever changed while it ran is replayed by the next poll.
        if let Some(why) = rebuild_because {
            tracing::warn!(why, "rebuilding the index beside the live one");
            let started = Instant::now();
            let drive = read_watched(shared).drive();
            match Watched::rebuild(vol, drive) {
                Ok(rebuilt) => {
                    write_watched(shared).adopt(rebuilt);
                    dirty = true;
                    tracing::info!(
                        took = ?started.elapsed(),
                        entries = read_watched(shared).index.len(),
                        "adopted the rebuilt index"
                    );
                }
                Err(e) => tracing::error!("rebuild failed: {e:#}"),
            }
        }

        // Refresh status metadata (volume cursor + last-sync time).
        let volume_next_usn = vol.journal_info().map(|i| i.next_usn).ok();
        {
            let mut m = shared.meta.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = volume_next_usn {
                m.volume_next_usn = v;
            }
            m.last_sync = Instant::now();
        }

        // Periodic snapshot.
        if dirty && last_save.elapsed() >= cfg.save_interval {
            // The clock restarts whether or not it worked. Only bumping it on success turns a
            // full disk (or an antivirus holding `index.tmp` open) into a retry every poll
            // instead of every save interval: the whole index re-serialized and several
            // hundred MB written once a second, under the read lock, forever. `dirty` stays
            // true either way, so the next attempt still has something to save.
            last_save = Instant::now();
            match save_snapshot(shared, &cfg.snapshot_path) {
                Ok(()) => {
                    dirty = false;
                    tracing::info!(path = %cfg.snapshot_path.display(), "saved snapshot");
                }
                Err(e) => tracing::warn!("snapshot save failed: {e:#}"),
            }
        }

        sleep_responsive(cfg.poll_interval);
    }
}

/// Serialize the index + cursor to `path` (read lock; searches proceed) and bump the snapshot
/// metadata.
fn save_snapshot(shared: &Shared, path: &Path) -> Result<()> {
    let cursor = {
        let w = read_watched(shared);
        w.save(path)?;
        w.next_usn
    };
    let mut m = shared.meta.lock().unwrap_or_else(|e| e.into_inner());
    m.snapshot_generation += 1;
    m.last_snapshot = Some(Instant::now());
    m.snapshot_cursor = cursor;
    Ok(())
}

/// Accept connections until shutdown, dispatching each to a short-lived worker thread. The
/// pipe's `nMaxInstances` cap bounds concurrent workers.
fn accept_loop(mut server: PipeServer, shared: Arc<Shared>) {
    loop {
        match server.accept() {
            Ok(stream) => {
                if shutting_down() {
                    return;
                }
                let s = Arc::clone(&shared);
                if let Err(e) = thread::Builder::new()
                    .name("efd-conn".into())
                    .spawn(move || handle_connection(stream, &s))
                {
                    tracing::warn!("failed to spawn a connection worker: {e}");
                }
            }
            Err(e) => {
                if shutting_down() {
                    return;
                }
                tracing::warn!("accept failed: {e}");
                thread::sleep(Duration::from_millis(50)); // avoid a hot error loop
            }
        }
    }
}

/// How long a connected client has to send its request before the connection is abandoned.
///
/// A pipe instance is a shared, exhaustible resource: [`MAX_PIPE_INSTANCES`] of them exist, and
/// a handler blocked on a client that never speaks holds one forever. Fill them all (sixty-four
/// connections from an unprivileged process, sending nothing) and the server can no longer
/// create a listening instance, so the pipe name leaves the namespace and every other user's
/// `ef` reports "efd is not running". Permanently, and from a process with no privileges.
///
/// Generous by the standards of the thing it bounds: the request is one small frame that a
/// client writes immediately after connecting, and the round trip the client itself allows is
/// ten seconds.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// How long the answer has to reach the client before the connection is abandoned.
///
/// The same exhaustible resource as [`REQUEST_DEADLINE`], reached from the other half of the
/// round trip: a client that asks a question and then never reads the answer leaves this
/// handler blocked in `WriteFile` once the pipe buffer fills: one instance held indefinitely,
/// per connection, from an unprivileged process. Bounding the read and not the write closed
/// one door and left the other one open.
///
/// Longer than the request deadline because it bounds something genuinely larger: a thirty
/// thousand row answer is several megabytes through an 8 KiB pipe buffer, and a client that is
/// merely slow to drain it should get its answer, not a broken pipe.
const RESPONSE_DEADLINE: Duration = Duration::from_secs(30);

/// Run `f` with `abort` fired if it has not finished within `deadline`.
///
/// The deadline cannot be enforced by the thread that is blocked on the I/O, so a second one
/// waits on a channel and cancels the stream; the blocked call then fails with
/// `ERROR_OPERATION_ABORTED` and unwinds normally.
///
/// It waits on a channel rather than polling a flag: the first version slept in 100 ms steps
/// and was joined before serving, so **every** request paid 0-100 ms before the search even
/// started, on a tool whose headline is a 40 ms search, and whose Explorer shim sends one
/// request per keystroke. Dropping the sender wakes the watchdog at once.
fn with_deadline<T>(
    abort: crate::ipc::pipe::AbortToken,
    deadline: Duration,
    what: &'static str,
    f: impl FnOnce() -> T,
) -> T {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = thread::Builder::new()
        .name("efd-deadline".into())
        .spawn(move || {
            // Only a *timeout* is a reason to abort. `Disconnected` is the opposite news (it
            // is `f` having finished and dropped the sender) and treating the two alike was
            // harmless only while `abort` was a bare `CancelIoEx`, which does nothing when
            // nothing is in flight. Now that an abort also latches a flag the next I/O checks,
            // firing it on success poisons the stream: the request read fine, the watchdog woke
            // on the drop and latched, and the response write then failed before it began. To
            // the client that is a connection closed after the length prefix: every search on
            // the machine returning "failed to fill whole buffer".
            if done_rx.recv_timeout(deadline) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                tracing::debug!(what, "deadline passed; dropping the connection");
                abort.abort();
            }
        })
        .ok();
    let out = f();
    drop(done_tx);
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    out
}

/// Serve exactly one request on a connection (the client does one round trip, then closes).
fn handle_connection(mut stream: PipeStream, shared: &Shared) {
    // Both halves of the round trip are bounded, and only the halves that are I/O: the search
    // between them takes as long as it takes.
    let request = with_deadline(
        stream.abort_token(),
        REQUEST_DEADLINE,
        "reading the request",
        || read_request(&mut stream),
    );

    let response = match request {
        Ok(req) => serve(req, shared),
        Err(WireError::ProtocolMismatch { found, expected }) => Response::Error {
            code: ErrCode::ProtocolMismatch,
            message: format!("daemon protocol v{expected}, client v{found}"),
        },
        Err(WireError::Oversized { len, cap }) => Response::Error {
            code: ErrCode::OversizedFrame,
            message: format!("request frame {len} exceeds cap {cap}"),
        },
        Err(e) => Response::Error {
            code: ErrCode::BadRequest,
            message: e.to_string(),
        },
    };
    let written = with_deadline(
        stream.abort_token(),
        RESPONSE_DEADLINE,
        "writing the response",
        || write_response(&mut stream, &response),
    );
    if let Err(e) = written {
        tracing::debug!("failed to write response: {e}");
    }
}

/// Dispatch a decoded request against the shared index.
fn serve(req: Request, shared: &Shared) -> Response {
    match req {
        Request::Search {
            query,
            case_sensitive,
            include_orphans,
            limit,
            reserve_elsewhere,
        } => {
            // Still enumerating: answer "building", not an empty (looks-broken) result.
            if shared.phase.load(std::sync::atomic::Ordering::SeqCst) == phase::BUILDING {
                return Response::Search {
                    total_hits: 0,
                    results: Vec::new(),
                    building: true,
                };
            }
            // A client picks `limit`, and it sizes real work here: a heap reservation and one
            // reconstructed path per hit returned. Clamp before any of that happens
            // ([`MAX_SEARCH_LIMIT`]): the pipe is open to interactive users by default.
            let limit = limit.min(MAX_SEARCH_LIMIT) as usize;
            let w = read_watched(shared);
            // The query string carries the M6 operator syntax; parsing server-side keeps the
            // protocol stable (a plain word parses to itself, so simple searches are M1 semantics).
            let parsed = crate::index::query::parse(&query);
            let ids = w
                .index
                .search_query(&parsed, !case_sensitive, include_orphans);
            let total_hits = ids.len() as u64;
            // Rank and keep only the top `limit` (M6 step 3: exact > prefix > substring,
            // shorter path on ties), then reconstruct paths just for those (the truncation
            // contract), attaching `is_dir` from the entry's flags (protocol v2).
            let top = w.index.top_ranked(
                &ids,
                &parsed,
                !case_sensitive,
                limit,
                (reserve_elsewhere as usize).min(limit),
            );
            // Reconstructing a path walks the entry's ancestors and allocates a `String`, and
            // this was the one part of answering a search still on a single core. The rows are
            // independent and `path` only reads, so they are built across them; `par_iter()`
            // keeps rank order, which `tests/bench_query.rs::path_build_timings` asserts as
            // well as times:
            //
            // ```text
            // rows      serial    parallel   speedup
            //  1,000     0.3 ms     0.2 ms      1.4x
            //  5,000     1.6 ms     0.5 ms      3.4x
            // 30,000    11.1 ms     2.2 ms      5.1x
            // ```
            //
            // Thirty thousand is the row that matters: it is what the Explorer shim asks for
            // on every keystroke, so this is paid once per character typed whatever the search
            // itself cost. Tens of milliseconds, not the hundreds a first reading of the CLI
            // suggested: most of `ef <term> --limit 30000` is the CLI printing the rows.
            let entries = w.index.entries();
            let results = top
                .par_iter()
                .map(|&id| SearchHit {
                    path: w.index.path(id),
                    is_dir: entries[id as usize].flags & flags::IS_DIR != 0,
                })
                .collect();
            Response::Search {
                total_hits,
                results,
                building: false,
            }
        }
        Request::Status => Response::Status(build_status(shared)),
        Request::Du {
            path,
            depth,
            top_n,
            real,
        } => serve_du(shared, &path, depth, top_n, real),
    }
}

/// Serve an `ef du` request from the resident index (M5). Sizes are on-disk **allocated**.
/// `real: true` (a reserved future logical-size mode; v0.1 has no CLI flag for it and the
/// client always sends `false`) is rejected with an error rather than answered with a
/// mislabeled allocated total.
fn serve_du(shared: &Shared, path: &str, depth: u32, top_n: u32, real: bool) -> Response {
    if real {
        return Response::Error {
            code: ErrCode::BadRequest,
            message: "--real (logical size) is not yet supported; default is allocated on-disk"
                .to_string(),
        };
    }
    // Still enumerating: the index holds a synthesized root and nothing else, so every path
    // "is not found". Saying so would be a confident false statement about the filesystem,
    // worse than the empty search result the same phase check was added to prevent.
    if shared.phase.load(std::sync::atomic::Ordering::SeqCst) == phase::BUILDING {
        return Response::Error {
            code: ErrCode::Internal,
            message: "the index is still building (first start after boot). Try again in a \
                      moment. `ef status` shows progress."
                .to_string(),
        };
    }
    let w = read_watched(shared);
    let Some(id) = w.index.resolve_path(path) else {
        return Response::Error {
            code: ErrCode::BadRequest,
            message: format!("path not found in the index: {path}"),
        };
    };
    // Clamped for the same reason `Search`'s `limit` is, and it was not: both size real work
    // from a number a client picked, and the pipe is open to any interactive user.
    let rep = w.index.du(
        id,
        depth.clamp(1, MAX_DU_DEPTH),
        (top_n as usize).min(MAX_DU_ROWS as usize),
    );
    let cb = rep.cluster_bytes as u64;
    let entries = w.index.entries();
    let rows = rep
        .rows
        .iter()
        .map(|r| DuRowWire {
            path: w.index.path(r.id),
            is_dir: entries[r.id as usize].flags & flags::IS_DIR != 0,
            size_bytes: r.clusters.saturating_mul(cb),
            truncated: r.truncated,
        })
        .collect();
    Response::Du {
        total_bytes: rep.total_clusters.saturating_mul(cb),
        truncated: rep.truncated,
        real: false,
        rows,
        sizes_resolved: w.index.sizes_resolved() as u64,
        entries: w.index.len() as u64,
    }
}

/// Assemble a [`StatusReport`] from the index stats, the cursor, `Meta`, and process memory.
/// Takes the `Watched` read lock first, copies out the small values, releases it, then locks
/// `Meta`, never nesting the two locks.
fn build_status(shared: &Shared) -> StatusReport {
    let (entries, live_entries, sizes_resolved, next_usn) = {
        let w = read_watched(shared);
        let s = w.index.stats();
        (
            s.entry_count as u64,
            s.live_entries as u64,
            w.index.sizes_resolved() as u64,
            w.next_usn,
        )
    };
    let building = shared.phase.load(std::sync::atomic::Ordering::SeqCst) == phase::BUILDING;
    let build_progress = shared
        .build_progress
        .load(std::sync::atomic::Ordering::Relaxed) as u64;
    let m = shared.meta.lock().unwrap_or_else(|e| e.into_inner());
    let mem = current_memory();
    StatusReport {
        drive: m.drive,
        entries,
        live_entries,
        sizes_resolved,
        usn_lag: m.volume_next_usn.saturating_sub(next_usn),
        last_sync_secs: m.last_sync.elapsed().as_secs(),
        working_set: mem.map(|x| x.working_set).unwrap_or(0),
        private_usage: mem.map(|x| x.private_usage).unwrap_or(0),
        building,
        build_progress,
        snapshot_generation: m.snapshot_generation,
        last_snapshot_secs: m.last_snapshot.map(|i| i.elapsed().as_secs()),
        snapshot_cursor: m.snapshot_cursor,
        uptime_secs: m.started.elapsed().as_secs(),
        poll_interval_ms: m.poll_interval_ms,
        proto_version: PROTO_VERSION,
        pid: std::process::id(),
    }
}

/// Sleep `total`, but wake early (within ~100 ms) if a shutdown is requested.
fn sleep_responsive(total: Duration) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if shutting_down() {
            return;
        }
        let chunk = step.min(total - slept);
        thread::sleep(chunk);
        slept += chunk;
    }
}

// --- console Ctrl-C handling (foreground) ---

use windows_sys::Win32::Foundation::BOOL;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

/// `CTRL_C_EVENT`, `CTRL_BREAK_EVENT`, `CTRL_CLOSE_EVENT`, `CTRL_LOGOFF_EVENT`,
/// `CTRL_SHUTDOWN_EVENT`.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        0 | 1 | 2 | 5 | 6 => {
            request_shutdown();
            // Return TRUE (handled). For Ctrl-C/Break the process keeps running, so the watch
            // loop saves and exits cleanly. For CLOSE/LOGOFF/SHUTDOWN the OS may terminate soon
            // after this returns, so that save is best-effort; USN resume recovers it.
            1
        }
        _ => 0,
    }
}

/// Install the console control handler so Ctrl-C triggers a graceful shutdown.
fn install_ctrl_handler() -> Result<()> {
    // SAFETY: registering a valid handler routine.
    let ok = unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) };
    if ok == 0 {
        anyhow::bail!(
            "SetConsoleCtrlHandler failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::acl::AclMode;
    use crate::ipc::server::PipeServer;

    /// A connected pair on a name of this test's own.
    fn pair(tag: &str) -> (PipeStream, PipeStream) {
        let name = format!(r"\\.\pipe\everyfind-test-{tag}-{}", std::process::id());
        let mut server = PipeServer::bind(&name, AclMode::Interactive, 2).expect("bind");
        let client = crate::ipc::client::connect(&name, Duration::from_secs(2)).expect("connect");
        (server.accept().expect("accept"), client)
    }

    /// Work that finishes inside its deadline must leave the stream usable.
    ///
    /// The watchdog waits on a channel and the channel reports two things: the deadline passing,
    /// and the sender being dropped, which is the *success* signal. Acting on both aborted
    /// every connection the instant it succeeded, and since an abort now latches a flag that
    /// later I/O checks, the response was never written. Every search on the machine failed
    /// with "failed to fill whole buffer" (found live, 2026-08-14).
    #[test]
    fn meeting_the_deadline_leaves_the_stream_usable() {
        let (served, _client) = pair("met");
        let token = served.abort_token();

        let out = with_deadline(served.abort_token(), Duration::from_secs(30), "quick", || 7);

        assert_eq!(out, 7);
        assert!(
            !token.is_aborted(),
            "finishing in time must not abort the connection"
        );
    }

    /// And work that overruns it must not.
    #[test]
    fn overrunning_the_deadline_aborts_the_stream() {
        let (served, _client) = pair("overrun");
        let token = served.abort_token();

        with_deadline(
            served.abort_token(),
            Duration::from_millis(50),
            "slow",
            || thread::sleep(Duration::from_millis(400)),
        );

        assert!(token.is_aborted(), "overrunning the deadline must abort");
    }

    /// The whole handler, over a real pipe: a client asks and gets a complete answer back.
    ///
    /// Nothing in the ordinary test run went through `handle_connection`; the only test that
    /// did is `service_roundtrip`, which is `#[ignore]`d because it needs an elevated terminal
    /// and enumerates C:. So a handler that read the request and then never wrote the response
    /// shipped, and the first thing that noticed was every search on the machine failing. This
    /// asks for the cheapest request there is and insists on getting the whole of it.
    #[test]
    fn a_request_gets_a_complete_response() {
        let name = format!(r"\\.\pipe\everyfind-test-roundtrip-{}", std::process::id());
        let mut server = PipeServer::bind(&name, AclMode::Interactive, 2).expect("bind");
        let shared = Arc::new(Shared {
            watched: RwLock::new(Watched::placeholder('T')),
            meta: Mutex::new(Meta::new('T', Duration::from_millis(1000))),
            phase: std::sync::atomic::AtomicU8::new(phase::READY),
            build_progress: std::sync::atomic::AtomicUsize::new(0),
        });

        let handler = thread::spawn(move || {
            let stream = server.accept().expect("accept");
            handle_connection(stream, &shared);
        });

        let got = crate::ipc::client::request(&name, &Request::Status, Duration::from_secs(5))
            .expect("the daemon must answer, and answer completely");
        assert!(
            matches!(&got, Response::Status(s) if s.drive == 'T'),
            "unexpected response: {got:?}"
        );
        handler.join().expect("the handler must not panic");
    }
}
