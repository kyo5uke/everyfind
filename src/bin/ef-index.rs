//! `ef-index`: the M2 index engine CLI.
//!
//! Acquires an in-memory filename index (a fresh MFT enumeration, or a resumed snapshot)
//! then reports stats, runs a one-shot query, saves a snapshot, and/or tails the USN
//! journal to keep the index live (`--watch`).
//!
//! Requires an ELEVATED terminal (opening `\\.\C:` needs admin). The only volume write is
//! `--create-journal`, an explicit opt-in for test volumes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;

use everyfind::index::Index;
use everyfind::snapshot;
use everyfind::sysinfo::current_memory;
use everyfind::volume::Win32Volume;
use everyfind::watch::{self, Synced, WatchOpts, Watched};

/// Instant filename indexer: build/resume, search, snapshot, and live USN tailing.
#[derive(Debug, Parser)]
#[command(name = "ef-index", version, about)]
struct Args {
    /// Target NTFS volume, e.g. `C:` or `C`.
    #[arg(long)]
    volume: String,

    /// Print index/build statistics.
    #[arg(long)]
    stats: bool,

    /// Run a single substring query after acquiring the index.
    #[arg(long)]
    query: Option<String>,

    /// Match case-sensitively (default: case-insensitive).
    #[arg(long)]
    case_sensitive: bool,

    /// Include orphan entries (parent directory not found) in results.
    #[arg(long)]
    include_orphans: bool,

    /// Maximum number of matches to print.
    #[arg(long, default_value_t = 20)]
    limit: usize,

    /// Report disk usage (allocated on-disk size) for this path, largest children first (M5).
    #[arg(long, value_name = "PATH")]
    du: Option<String>,

    /// `--du` depth: 1 = immediate children, N = descendants within N levels.
    #[arg(long, default_value_t = 1)]
    du_depth: u32,

    /// `--du` maximum number of rows to print.
    #[arg(long, default_value_t = 20)]
    du_top: usize,

    /// Print raw byte counts (for `--du`) instead of human-readable sizes.
    #[arg(long)]
    bytes: bool,

    /// Keep the index live by tailing the USN journal (foreground; Ctrl-C to stop).
    #[arg(long)]
    watch: bool,

    /// Save an index snapshot to this path (after catch-up; periodically while `--watch`).
    #[arg(long, value_name = "PATH")]
    save_snapshot: Option<PathBuf>,

    /// Resume from a snapshot instead of a full enumeration (falls back to enum if stale).
    #[arg(long, value_name = "PATH")]
    load_snapshot: Option<PathBuf>,

    /// Create the USN journal before indexing: an explicit WRITE, test volumes only.
    #[arg(long)]
    create_journal: bool,

    /// USN journal maximum size for `--create-journal` (bytes).
    #[arg(long, default_value_t = 33_554_432)]
    journal_max_bytes: u64,

    /// USN journal allocation delta for `--create-journal` (bytes).
    #[arg(long, default_value_t = 8_388_608)]
    journal_delta_bytes: u64,

    /// Journal poll interval while `--watch` (milliseconds).
    #[arg(long, default_value_t = 1000)]
    poll_interval_ms: u64,

    /// Rebuild the index when arena garbage exceeds this fraction (0..1).
    #[arg(long, default_value_t = 0.3)]
    garbage_threshold: f64,

    /// Minimum seconds between snapshot writes while `--watch`.
    #[arg(long, default_value_t = 60)]
    save_interval_secs: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let mut volume = Win32Volume::open(&args.volume)
        .with_context(|| format!("opening volume {}", args.volume))?;
    let drive = volume.drive();

    if args.create_journal {
        // WRITE FSCTL: explicit opt-in only; never run implicitly or on a system volume.
        tracing::warn!("--create-journal: writing a USN journal on {drive}: (test-volume write)");
        volume
            .create_journal(args.journal_max_bytes, args.journal_delta_bytes)
            .context("creating USN journal")?;
    }

    // Acquire the index: resume a snapshot if given (falling back to enum on any problem),
    // else enumerate. Then catch up any journal delta since the snapshot cursor.
    let acquire_start = Instant::now();
    let mut state = match &args.load_snapshot {
        Some(path) => match snapshot::load(path) {
            Ok(loaded) => {
                tracing::info!(path = %path.display(), "loaded snapshot; resuming tail");
                Watched::from_snapshot(loaded, &mut volume, drive)?
            }
            Err(e) => {
                tracing::warn!("snapshot load failed ({e:#}); enumerating instead");
                Watched::enumerate(&mut volume, drive)?
            }
        },
        None => Watched::enumerate(&mut volume, drive)?,
    };
    match state.sync(&mut volume).context("catching up the journal")? {
        Synced::Applied(a) => {
            state.refresh_sizes(&volume, &a.resized);
            if !state.index.fold_map_ready() {
                state.index.rebuild_fold_map();
            }
            if a.events > 0 {
                tracing::info!(
                    caught_up = a.events,
                    "applied journal changes since the snapshot"
                );
            }
        }
        Synced::Discontinuity(why) => {
            tracing::warn!(why, "re-enumerating");
            state.recover(&mut volume).context("re-enumerating")?;
        }
    }
    let acquire_elapsed = acquire_start.elapsed();

    if args.stats {
        print_stats(&state, drive, acquire_elapsed);
    }
    if let Some(query) = &args.query {
        run_query(&state.index, query, &args);
    }
    if let Some(path) = &args.du {
        run_du(&state.index, path, &args);
    }
    if let Some(path) = &args.save_snapshot {
        state
            .save(path)
            .with_context(|| format!("saving snapshot to {}", path.display()))?;
        tracing::info!(path = %path.display(), "saved snapshot");
    }

    if args.watch {
        let opts = WatchOpts {
            poll_interval: Duration::from_millis(args.poll_interval_ms),
            garbage_threshold: args.garbage_threshold,
            snapshot_path: args.save_snapshot.clone(),
            save_interval: Duration::from_secs(args.save_interval_secs),
        };
        watch::run(&mut state, &mut volume, &opts)?; // loops until interrupted
    }

    Ok(())
}

fn print_stats(state: &Watched, drive: char, acquire_elapsed: Duration) {
    let s = state.index.stats();
    println!("volume            : {drive}:");
    println!(
        "entries           : {} ({} live)",
        s.entry_count, s.live_entries
    );
    println!("acquire time      : {acquire_elapsed:.2?}");
    println!(
        "index heap (est.) : {:.1} MiB  (arena {:.1} + fold {:.1} + entries {:.1} + frn-map {:.1})",
        mib(s.total_bytes),
        mib(s.arena_bytes),
        mib(s.fold_bytes),
        mib(s.entries_bytes),
        mib(s.map_bytes),
    );
    println!(
        "arena garbage     : {:.1} MiB ({:.1}%)",
        mib(s.garbage_bytes),
        state.index.garbage_ratio() * 100.0,
    );
    match current_memory() {
        Some(m) => println!(
            "process memory    : WorkingSet {:.1} MiB / PrivateUsage {:.1} MiB",
            mib(m.working_set as usize),
            mib(m.private_usage as usize),
        ),
        None => println!("process memory    : (unavailable)"),
    }
    println!(
        "sizes resolved    : {} / {} entries",
        state.index.sizes_resolved(),
        s.entry_count
    );
    println!("usn next cursor   : {}", state.next_usn);
}

/// Print an `ef du` report for `path` (allocated on-disk size, M5).
fn run_du(index: &Index, path: &str, args: &Args) {
    let Some(id) = index.resolve_path(path) else {
        println!("\ndu {path:?}: path not found in the index");
        return;
    };
    let du_start = Instant::now();
    let rep = index.du(id, args.du_depth, args.du_top);
    let du_elapsed = du_start.elapsed();
    let cb = rep.cluster_bytes as u64;
    let total = rep.total_clusters.saturating_mul(cb);

    println!(
        "\ndu {path}  (sizes: allocated on-disk; --bytes for raw; computed in {du_elapsed:.2?})"
    );
    if rep.truncated {
        println!("  warning: some files exceed the 16 TiB size cap; totals are a lower bound");
    }
    println!("  total: {}", fmt_size(total, args.bytes));
    for row in &rep.rows {
        let sz = row.clusters.saturating_mul(cb);
        let pct = if total > 0 {
            100.0 * sz as f64 / total as f64
        } else {
            0.0
        };
        println!(
            "  {:>11}  {:5.1}%  {}{}",
            fmt_size(sz, args.bytes),
            pct,
            index.path(row.id),
            if row.truncated { "  (+)" } else { "" },
        );
    }
}

/// Human-readable (or raw, when `raw`) byte size.
fn fmt_size(bytes: u64, raw: bool) -> String {
    if raw {
        return bytes.to_string();
    }
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn run_query(index: &Index, query: &str, args: &Args) {
    let case_insensitive = !args.case_sensitive;
    let search_start = Instant::now();
    let parsed = everyfind::index::query::parse(query);
    let hits = index.search_query(&parsed, case_insensitive, args.include_orphans);
    let search_elapsed = search_start.elapsed();

    println!(
        "\nquery {query:?}  ({}, orphans {}) : {} match(es) in {search_elapsed:.2?}",
        if case_insensitive {
            "case-insensitive"
        } else {
            "case-sensitive"
        },
        if args.include_orphans {
            "included"
        } else {
            "excluded"
        },
        hits.len(),
    );
    let top = index.top_ranked(&hits, &parsed, case_insensitive, args.limit, 0);
    for id in &top {
        println!("  {}", index.path(*id));
    }
    if hits.len() > args.limit {
        println!("  ... and {} more (raise --limit)", hits.len() - args.limit);
    }
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
