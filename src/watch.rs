//! Live index maintenance: acquire an index (fresh enumeration or a resumed snapshot),
//! tail the USN journal to keep it current, and recover from any discontinuity by a full
//! re-enumeration.
//!
//! The recovery principle: apply USN records in strict order; on a
//! journal-id change, a trimmed cursor (`ERROR_JOURNAL_ENTRY_DELETED`), or any other
//! discontinuity, re-enumerate from a fresh consistent snapshot. Arena garbage from
//! rename/delete triggers the same full re-enumeration once it crosses a threshold.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::index::{build_from_volume, empty_index, Index};
use crate::snapshot::{self, Loaded};
use crate::volume::{reason, JournalError, UsnVolume, FILE_ATTRIBUTE_DIRECTORY};

/// The live index plus the journal cursor needed to resume tailing.
pub struct Watched {
    pub index: Index,
    pub journal_id: u64,
    pub next_usn: u64,
    drive: char,
}

/// One file's freshly measured on-disk allocation.
pub struct Measured {
    pub frn: u64,
    pub clusters: u32,
    pub truncated: bool,
}

/// What one [`sync`](Watched::sync) applied.
pub struct Applied {
    /// How many journal events were applied.
    pub events: usize,
    /// Files whose on-disk allocation may have moved, with the path to stat each by. Handed
    /// back rather than stat'ed in place so the caller decides where that I/O runs; see
    /// [`Watched::measure_sizes`].
    pub resized: Vec<(u64, String)>,
}

/// What one [`sync`](Watched::sync) did.
pub enum Synced {
    /// Journal events applied (possibly zero); the index is current.
    Applied(Applied),
    /// The journal no longer continues from our cursor: it was recreated, trimmed past us, or
    /// cannot be queried at all. Nothing was applied, and nothing can be until the index is
    /// rebuilt. The string says which, for the log.
    Discontinuity(&'static str),
}

/// A replacement index, built without touching the live one.
///
/// The two halves of a rebuild cost wildly different amounts: enumerating six million MFT
/// records takes tens of seconds, and moving the finished index into place takes a pointer
/// write. Keeping them apart is what lets a caller hold its lock for the second half only,
/// see [`Watched::rebuild`].
pub struct Rebuilt {
    index: Index,
    journal_id: u64,
    next_usn: u64,
}

impl Watched {
    /// A placeholder holding only an empty index: what the daemon serves while
    /// the initial enumeration runs on the acquire path. Cursor 0 is never used
    /// for tailing (the real acquire replaces the whole `Watched` before the
    /// watch loop starts).
    pub fn placeholder(drive: char) -> Self {
        Self {
            index: empty_index(drive),
            journal_id: 0,
            next_usn: 0,
            drive,
        }
    }

    /// Acquire a fresh index by full MFT enumeration. The cursor is the journal's `next_usn`
    /// captured *before* the enum, so any change during the enum is merely re-applied
    /// (idempotently) rather than missed.
    pub fn enumerate(vol: &mut dyn UsnVolume, drive: char) -> Result<Self> {
        let r = Self::rebuild(vol, drive)?;
        Ok(Self {
            index: r.index,
            journal_id: r.journal_id,
            next_usn: r.next_usn,
            drive,
        })
    }

    /// The volume this index is of.
    pub fn drive(&self) -> char {
        self.drive
    }

    /// Whether arena garbage has crossed `threshold` (fraction in `[0, 1]`) and the index
    /// should be rebuilt. A read, so a caller can ask under a shared lock.
    pub fn compaction_due(&self, threshold: f64) -> bool {
        self.index.garbage_ratio() > threshold
    }

    /// Build a replacement index by full MFT enumeration. The cursor is the journal's
    /// `next_usn` captured *before* the enum, so any change during it is merely re-applied
    /// (idempotently) rather than missed.
    ///
    /// Deliberately an associated function rather than a method: it borrows no `Watched`, so a
    /// caller keeping the live index behind a lock can run this with the lock **released** and
    /// go on answering searches from the old contents for the tens of seconds it takes. Doing
    /// it the other way (rebuilding in place, under the write lock) froze every search on
    /// this volume for 20-40 s, which is long enough that a client gives up, hands the search
    /// back to Windows, and the user watches an unindexed drive get crawled instead.
    pub fn rebuild(vol: &mut dyn UsnVolume, drive: char) -> Result<Rebuilt> {
        let info = vol.journal_info()?;
        let index = build_from_volume(vol, drive)?;
        Ok(Rebuilt {
            index,
            journal_id: info.journal_id,
            next_usn: info.next_usn,
        })
    }

    /// Move a rebuilt index into place: the only half that needs exclusive access.
    pub fn adopt(&mut self, rebuilt: Rebuilt) {
        self.index = rebuilt.index;
        self.journal_id = rebuilt.journal_id;
        self.next_usn = rebuilt.next_usn;
    }

    /// [`rebuild`](Self::rebuild) then [`adopt`](Self::adopt), for a caller that holds no lock
    /// and so has nothing to gain from splitting them.
    pub fn recover(&mut self, vol: &mut dyn UsnVolume) -> Result<()> {
        let rebuilt = Self::rebuild(vol, self.drive)?;
        self.adopt(rebuilt);
        Ok(())
    }

    /// Resume from a loaded snapshot if the journal still matches and the cursor is not
    /// trimmed; otherwise fall back to a full enumeration. The caller should then call
    /// [`sync`](Self::sync) once to catch up the delta.
    pub fn from_snapshot(loaded: Loaded, vol: &mut dyn UsnVolume, drive: char) -> Result<Self> {
        let info = vol.journal_info()?;
        if info.journal_id == loaded.journal_id && loaded.next_usn >= info.first_usn {
            // The contig hit map is not serialized (a resumed snapshot may carry mutated,
            // out-of-order fold offsets): one parallel sort here, before serving.
            let mut index = loaded.index;
            index.rebuild_fold_map();
            // A snapshot carries its sizes, so a resume skips the whole `$MFT` pass; that is
            // what makes it fast. But it carries their *absence* just as faithfully: one build
            // whose size pass came up empty saved a snapshot with zero sizes, and every restart
            // after that resumed the zeros without ever consulting the volume again. The `du`
            // warning told the user "a restart re-runs it", and that was only true for restarts
            // that had no snapshot to resume, i.e., almost none of them. Caught live: three
            // service restarts in a row, each reporting `0 / 6.2M resolved`, while the fix for
            // the original size-pass failure sat unexercised because nothing ever ran it.
            //
            // Under half resolved cannot be journal drift: a healthy build resolves ~95% and
            // decays by a fraction of a percent per day as the journal adds unsized entries,
            // so it means the pass itself failed, and the read is worth paying again.
            if index.sizes_resolved() * 2 < index.len() {
                tracing::info!(
                    resolved = index.sizes_resolved(),
                    entries = index.len(),
                    "resumed snapshot is missing its sizes; re-running the size pass"
                );
                index.restart_size_pass(vol.cluster_bytes());
                if let Err(e) = vol.enum_sizes(&mut |frn, clusters, truncated| {
                    index.set_size(frn, clusters, truncated);
                }) {
                    tracing::warn!("size pass failed on resume; du sizes stay missing: {e:#}");
                }
            }
            Ok(Self {
                index,
                journal_id: loaded.journal_id,
                next_usn: loaded.next_usn,
                drive,
            })
        } else {
            tracing::warn!(
                snapshot_id = format!("{:#x}", loaded.journal_id),
                volume_id = format!("{:#x}", info.journal_id),
                cursor = loaded.next_usn,
                first_usn = info.first_usn,
                "snapshot cursor stale (journal recreated or wrapped); re-enumerating",
            );
            Self::enumerate(vol, drive)
        }
    }

    /// Catch up: drain journal events from the cursor and apply them, returning the count.
    ///
    /// Reports a discontinuity rather than recovering from one. Recovery is a full
    /// enumeration, and where that runs (under the caller's lock or beside it) is the
    /// caller's decision to make, not a detail to bury in a poll (see [`rebuild`]).
    ///
    /// [`rebuild`]: Self::rebuild
    pub fn sync(&mut self, vol: &mut dyn UsnVolume) -> Result<Synced> {
        // A journal-id change means the journal was deleted+recreated.
        //
        // So does a journal that cannot be queried at all: `fsutil usn deletejournal` leaves
        // this call failing, and treating that as a plain error meant the daemon logged and
        // retried every poll forever with `phase` still READY: the index frozen while clients
        // were told it was current. A discontinuity is a discontinuity; say so.
        match vol.journal_info() {
            Ok(info) if info.journal_id == self.journal_id => {}
            Ok(_) => return Ok(Synced::Discontinuity("the journal was recreated")),
            Err(e) => {
                tracing::warn!("the journal cannot be queried ({e:#})");
                return Ok(Synced::Discontinuity("the journal cannot be queried"));
            }
        }

        let mut applied = 0usize;
        // FRNs of non-delete file events, whose on-disk size may have changed (create / grow /
        // rename). Refreshed after the drain so `ef du` stays live. USN records carry no
        // size (probe P18), so a single-file stat is the only way to learn the new allocation.
        let mut resized: Vec<u64> = Vec::new();
        let (start, jid) = (self.next_usn, self.journal_id);
        let index = &mut self.index;
        let result = vol.read_journal(start, jid, &mut |ev| {
            index.apply_event(ev);
            applied += 1;
            // Only the reasons that can move the allocation. "Not a delete, not a directory"
            // enqueued every touched file (a security change, a close, the discarded
            // pre-image of a rename) and each one costs a path rebuild plus a
            // CreateFileW/GetFileInformationByHandleEx/CloseHandle round trip, all of it
            // under the index write lock. One `cargo build` on the indexed volume is tens of
            // thousands of them, which is seconds of blocked searches on a tool whose premise
            // is a forty-millisecond search.
            if ev.reason & reason::SIZE_AFFECTING != 0
                && ev.reason & reason::FILE_DELETE == 0
                && ev.attributes & FILE_ATTRIBUTE_DIRECTORY == 0
            {
                resized.push(ev.frn);
            }
        });
        match result {
            Ok(cursor) => {
                self.next_usn = cursor;
                Ok(Synced::Applied(Applied {
                    events: applied,
                    resized: self.paths_to_measure(resized),
                }))
            }
            Err(JournalError::EntryDeleted) => Ok(Synced::Discontinuity(
                "the journal was trimmed past our cursor",
            )),
            Err(JournalError::IdChanged { .. }) => {
                Ok(Synced::Discontinuity("the journal was recreated mid-read"))
            }
            Err(JournalError::Other(e)) => Err(e),
        }
    }

    /// Turn the FRNs collected during a drain into the `(frn, path)` pairs a size refresh
    /// needs, reconstructing each path from the just-updated index.
    ///
    /// The path rebuild is in-memory and belongs with the apply; the stat that follows it does
    /// not, which is why they are separate calls (see [`measure_sizes`]).
    ///
    /// [`measure_sizes`]: Self::measure_sizes
    fn paths_to_measure(&self, mut resized: Vec<u64>) -> Vec<(u64, String)> {
        if resized.is_empty() {
            return Vec::new();
        }
        resized.sort_unstable();
        resized.dedup();
        // A ceiling on one poll's worth of stats, because a bulk operation can hand this an
        // unbounded list. The sizes that do not get refreshed are not lost; the next full
        // enumeration reconciles them, which is the same promise the fail-soft path already
        // makes. Said out loud rather than dropped silently: a cap nobody can see reads as
        // "everything was refreshed".
        const MAX_PER_POLL: usize = 4_096;
        if resized.len() > MAX_PER_POLL {
            tracing::info!(
                changed = resized.len(),
                refreshed = MAX_PER_POLL,
                "more size changes than one poll refreshes; the rest wait for the next \
                 enumeration"
            );
            resized.truncate(MAX_PER_POLL);
        }
        resized
            .into_iter()
            .filter_map(|frn| self.index.path_of_frn(frn).map(|path| (frn, path)))
            .collect()
    }

    /// Stat each changed file to learn its new on-disk allocation.
    ///
    /// Borrows no index, so a caller keeping one behind a lock runs this with the lock
    /// released. That is not a nicety: each entry is a `CreateFileW` +
    /// `GetFileInformationByHandleEx` x2 + `CloseHandle` through whatever filter drivers are
    /// installed, and one `cargo build` on the indexed volume fills the whole 4,096-entry
    /// budget on every poll: held exclusively, that is seconds of blocked searches per second
    /// of compiling, on a tool whose premise is a forty-millisecond search.
    ///
    /// Fail-soft: a file that no longer stats (deleted in the race between the apply and here)
    /// is simply left out, and keeps whatever size it had until the next full enumeration.
    pub fn measure_sizes(vol: &dyn UsnVolume, resized: &[(u64, String)]) -> Vec<Measured> {
        resized
            .iter()
            .filter_map(|(frn, path)| {
                // Stat by path: the masked FRN is not an `OpenFileById` reference (probe P5).
                vol.alloc_clusters(*frn, path)
                    .map(|(clusters, truncated)| Measured {
                        frn: *frn,
                        clusters,
                        truncated,
                    })
            })
            .collect()
    }

    /// Record what [`measure_sizes`](Self::measure_sizes) found. A few thousand array writes,
    /// the half that genuinely needs exclusive access.
    pub fn record_sizes(&mut self, measured: &[Measured]) {
        for m in measured {
            self.index.update_size(m.frn, m.clusters, m.truncated);
        }
    }

    /// [`measure_sizes`] then [`record_sizes`], for a caller that holds no lock.
    ///
    /// [`measure_sizes`]: Self::measure_sizes
    /// [`record_sizes`]: Self::record_sizes
    pub fn refresh_sizes(&mut self, vol: &dyn UsnVolume, resized: &[(u64, String)]) {
        let measured = Self::measure_sizes(vol, resized);
        self.record_sizes(&measured);
    }

    /// Persist the index + cursor to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        snapshot::save(path, &self.index, self.journal_id, self.next_usn)
    }
}

/// Options for the [`run`] poll loop.
pub struct WatchOpts {
    /// Delay between journal polls.
    pub poll_interval: Duration,
    /// Arena garbage fraction above which the index is rebuilt.
    pub garbage_threshold: f64,
    /// If set, persist a snapshot to this path at most every [`save_interval`](Self::save_interval).
    pub snapshot_path: Option<PathBuf>,
    /// Minimum spacing between snapshot writes.
    pub save_interval: Duration,
}

/// Poll the journal forever (until the process is interrupted), keeping `state` live.
/// Applies changes, rebuilds on excessive garbage, and periodically snapshots.
pub fn run(state: &mut Watched, vol: &mut dyn UsnVolume, opts: &WatchOpts) -> Result<()> {
    tracing::info!(
        entries = state.index.len(),
        next_usn = state.next_usn,
        "watching {}: poll every {:?} (Ctrl-C to stop)",
        state.drive,
        opts.poll_interval,
    );
    let mut last_save = Instant::now();
    let mut dirty = false;

    loop {
        // Nothing here holds a lock (this loop *is* the only owner of the index) so each
        // job's two halves are simply run together.
        let why = match state.sync(vol)? {
            Synced::Applied(a) if a.events == 0 => {
                state.refresh_sizes(vol, &a.resized);
                None
            }
            Synced::Applied(a) => {
                dirty = true;
                state.refresh_sizes(vol, &a.resized);
                if state.index.orphans_stale() {
                    state.index.refresh_orphans();
                }
                // A create/rename in the drain emptied the contig hit map; rebuild it here, on
                // the watch thread and off the search path, so searches stay on the fast
                // single-pass. Delete-only batches leave the map valid; nothing to do.
                if !state.index.fold_map_ready() {
                    state.index.rebuild_fold_map();
                }
                let s = state.index.stats();
                tracing::info!(
                    applied = a.events,
                    live = s.live_entries,
                    next_usn = state.next_usn,
                    "applied journal changes"
                );
                None
            }
            Synced::Discontinuity(why) => Some(why),
        };
        let why = why.or_else(|| {
            state
                .compaction_due(opts.garbage_threshold)
                .then_some("arena garbage crossed the threshold")
        });
        if let Some(why) = why {
            tracing::warn!(why, "rebuilding the index");
            state.recover(vol)?;
            dirty = true;
            tracing::info!(entries = state.index.len(), "rebuilt index");
        }
        if let Some(path) = &opts.snapshot_path {
            if dirty && last_save.elapsed() >= opts.save_interval {
                state.save(path)?;
                last_save = Instant::now();
                dirty = false;
                tracing::info!(path = %path.display(), "saved snapshot");
            }
        }
        std::thread::sleep(opts.poll_interval);
    }
}
