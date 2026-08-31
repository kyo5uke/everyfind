//! `ef content`: search inside files through the grix engine.
//!
//! Design:
//! - **P-INT-1**: the content index lives entirely in the *client* process. The
//!   daemon holds only the name index, so `ef <query>` stays exactly as light as
//!   before, and the content index (per *directory root*, not per volume) is an
//!   opt-in cost paid only when `-c` is used. Content indexing whole volumes is
//!   deliberately out of scope: real content searches are project-scoped, which
//!   is grix's native per-root model.
//! - **P-INT-2**: grix is a crates.io library dependency; its repo stays
//!   independent. The index store is shared with a standalone grix install
//!   (`%LOCALAPPDATA%\grix`, `GRIX_DATA_DIR` override respected), so both tools
//!   reuse one index per root.
//! - **P-INT-3**: no IPC change; protocol stays v3.
//! - **P-INT-5**: first `-c` in a tree answers immediately from a walk scan
//!   (identical results, grep speed) while the caller kicks off a detached
//!   background build; later runs hit the index. This mirrors grix 0.5+'s
//!   proven first-run design, so there is no "wait for indexing" state.
//!
//! Security note: the content index is built with the caller's token, so it can
//!   only ever contain text the user can read, unlike the (LocalSystem) name
//! index, no privilege boundary is crossed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use grix::index::build::{self, BuildOptions};
use grix::index::format::{overlay_path, IndexReader};
use grix::search::{self, SearchOptions, View};
use grix::store;

/// One content match, ripgrep-shaped (`path:line:text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHit {
    /// Path relative to the indexed root, `/`-separated (grix convention).
    pub rel_path: String,
    pub line_number: u64,
    pub text: String,
}

/// Search result plus what the caller should do next.
#[derive(Debug)]
pub struct ContentOutcome {
    pub hits: Vec<ContentHit>,
    /// Root the content index covers (or would cover once built).
    pub root: PathBuf,
    /// True when no usable index existed and a walk scan answered; the caller
    /// should start a background build so the next run is index-fast.
    pub needs_index: bool,
}

/// The `ef content` option subset (v1).
#[derive(Debug, Default)]
pub struct ContentOpts {
    pub case_insensitive: bool,
    /// Restrict matching to these files/directories (inside the indexed root).
    pub scopes: Vec<PathBuf>,
}

/// Search file *contents* under the tree containing `start`.
///
/// With a usable index (this or any ancestor root): incremental refresh
/// (skipped while a `grix watch` / background builder heartbeat is live), then
/// an indexed search: the confirming scan re-reads live file bytes, so results
/// are never stale lines. Without one: a full walk scan answers now and
/// `needs_index` tells the caller to build in the background.
pub fn search(start: &Path, pattern: &str, opts: &ContentOpts) -> Result<ContentOutcome> {
    let sopts = SearchOptions {
        case_insensitive: opts.case_insensitive,
        ..Default::default()
    };
    let matcher = search::compile(pattern, &sopts).map_err(|e| anyhow!("{e}"))?;

    if let Some((idx, root)) = store::find_index_upward(start) {
        if IndexReader::open(&idx).is_ok() {
            // Freshness: with the grix overlay design this costs a directory
            // walk plus the churn since the base index, not a rebuild. A live
            // watcher (or builder) heartbeat already owns freshness.
            if !store::watcher_is_live(&idx) {
                if let Err(e) = build::build(&root, &idx, &BuildOptions::default()) {
                    tracing::warn!("content index refresh skipped: {e}");
                }
            }
            let reader = IndexReader::open(&idx)
                .map_err(|e| anyhow!("cannot open the content index ({e})"))?;
            let over = IndexReader::open(&overlay_path(&idx))
                .ok()
                .filter(|o| o.index_ids().parent_id == reader.index_ids().build_id);
            let view = View::new(&reader, over.as_ref());
            let mut sopts = sopts.clone();
            sopts.path_scopes = to_scopes(&opts.scopes, &root)?;
            let (results, _) =
                search::search_index(&view, &root, &matcher, &sopts).map_err(|e| anyhow!("{e}"))?;
            return Ok(ContentOutcome {
                hits: flatten(results),
                root,
                needs_index: false,
            });
        }
        // An index file exists but is unreadable (old format): treat like
        // first contact: walk now, rebuild in the background.
        let mut sopts = sopts.clone();
        sopts.path_scopes = to_scopes(&opts.scopes, &root)?;
        let (results, _) =
            search::search_walk(&root, &matcher, &sopts).map_err(|e| anyhow!("{e}"))?;
        return Ok(ContentOutcome {
            hits: flatten(results),
            root,
            needs_index: true,
        });
    }

    // First contact with this tree.
    let root = store::canonical_root(start).context("resolving the content search root")?;
    let mut sopts = sopts.clone();
    sopts.path_scopes = to_scopes(&opts.scopes, &root)?;
    let (results, _) = search::search_walk(&root, &matcher, &sopts).map_err(|e| anyhow!("{e}"))?;
    Ok(ContentOutcome {
        hits: flatten(results),
        root,
        needs_index: true,
    })
}

/// The indexed root covering `start`, if a readable index exists, decided
/// without searching anything.
///
/// Build (or refresh) the content index for `root`, holding the grix watch
/// heartbeat throughout so concurrent `ef content` / `grix` runs neither
/// self-refresh nor spawn duplicate builders. This is what the hidden
/// `ef content-index <root>` plumbing subcommand runs, detached.
pub fn build_index(root: &Path) -> Result<()> {
    let root = store::canonical_root(root).context("resolving the content index root")?;
    let idx = store::index_path(&root).map_err(|e| anyhow!("{e}"))?;
    if let Some(parent) = idx.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let hb = store::start_heartbeat(&idx, Duration::from_secs(5));
    let result = build::build(&root, &idx, &BuildOptions::default());
    hb.stop_and_clear();
    result
        .map(|_| ())
        .map_err(|e| anyhow!("content index build failed: {e}"))
}

/// Fire-and-forget `ef content-index <root>` in a detached child, claiming the
/// grix watch marker first so racing searches back off. Call *after* printing
/// results: the walk scan just warmed the file cache the builder will read.
pub fn spawn_background_build(root: &Path) {
    let Ok(idx) = store::index_path(root) else {
        return;
    };
    if store::watcher_is_live(&idx) {
        return; // a watcher or another builder already owns freshness
    }
    if let Some(parent) = idx.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = store::write_watch_heartbeat(&idx);
    let Ok(exe) = std::env::current_exe() else {
        store::remove_watch_marker(&idx);
        return;
    };
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("content-index")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    match cmd.spawn() {
        // Deliberately not waited on: the child outlives this process.
        Ok(child) => std::mem::forget(child),
        // Could not start it: clear the claim so future runs self-refresh.
        Err(_) => store::remove_watch_marker(&idx),
    }
}

/// Path arguments -> grix index scopes (paths relative to the indexed root).
fn to_scopes(paths: &[PathBuf], root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let canon =
            store::canonical_root(p).with_context(|| format!("cannot resolve {}", p.display()))?;
        if canon == *root {
            continue; // scoping to the whole tree = no scope
        }
        let rel = canon.strip_prefix(root).map_err(|_| {
            anyhow!(
                "{} is outside the content-indexed tree ({})",
                p.display(),
                root.display()
            )
        })?;
        let s = rel.to_string_lossy().replace('\\', "/");
        if !s.is_empty() {
            out.push(s);
        }
    }
    Ok(out)
}

/// grix per-file results -> flat ripgrep-shaped hits (match lines only).
fn flatten(results: Vec<search::FileResult>) -> Vec<ContentHit> {
    let mut hits = Vec::new();
    for fr in results {
        for line in &fr.lines {
            if !line.is_match {
                continue;
            }
            let text = String::from_utf8_lossy(&line.line);
            hits.push(ContentHit {
                rel_path: fr.rel_path.clone(),
                line_number: line.line_number,
                text: text.trim_end_matches('\r').to_string(),
            });
        }
    }
    hits
}
