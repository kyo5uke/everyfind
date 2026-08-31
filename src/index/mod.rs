//! In-memory filename index: string/fold arenas + a flat entry table with parent
//! links, plus path reconstruction and parallel substring search.
//!
//! The design rationale (directory-only FRN map, distinct
//! root/orphan sentinels, fold `0x00` separators, no stored full paths).

mod arena;
mod build;
mod du;
pub mod fold;
mod mutate;
pub mod query;

pub use du::{DuReport, DuRow};

use std::collections::HashMap;

use arena::StringArena;
use memchr::memmem;
use rayon::prelude::*;

pub use build::{build_from_volume, empty_index};

/// NTFS root directory file reference number.
pub const ROOT_FRN: u64 = 5;

/// Parent value marking the volume root (path prefix `<drive>:\`).
pub const ROOT_SENTINEL: u32 = u32::MAX;
/// Parent value marking an entry whose parent directory was not found in the MFT
/// stream. Distinct from [`ROOT_SENTINEL`] so orphans never masquerade as top-level
/// files (path prefix `<orphan>\`).
pub const ORPHAN_SENTINEL: u32 = u32::MAX - 1;

/// `flags` bits on [`Entry`].
pub mod flags {
    /// Entry is a directory.
    pub const IS_DIR: u16 = 1 << 0;
    /// Entry has `FILE_ATTRIBUTE_REPARSE_POINT` (symlink/junction).
    pub const REPARSE_POINT: u16 = 1 << 1;
    /// Entry's parent directory was not found (orphan). Excluded from search by default.
    pub const IS_ORPHAN: u16 = 1 << 2;
    /// Entry was deleted (tombstone). The slot is kept so parent ids stay stable, but it
    /// is excluded from search and its name bytes are counted as arena garbage. Set by
    /// [`Index::apply_event`] on `FILE_DELETE` (M2).
    pub const DEAD: u16 = 1 << 3;
    /// This entry's allocated size saturated `u32::MAX` clusters (a > 16 TiB file, M5). Its
    /// stored size is a lower bound; `du` propagates a "truncated" marker up the subtree and the
    /// client notes it, so a saturated total is never silently under-reported.
    pub const SIZE_TRUNCATED: u16 = 1 << 4;
}

/// A single index entry, ~20 bytes. Referenced by an internal `u32` id (its position
/// in [`Index::entries`]).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Offset of the original name in the string arena.
    pub name_off: u32,
    /// Byte length of the original name.
    pub name_len: u16,
    /// Offset of the case-folded name in the fold arena (own offsets, since folding can
    /// change byte length).
    pub fold_off: u32,
    /// Byte length of the case-folded name.
    pub fold_len: u16,
    /// Internal id of the parent directory, or [`ROOT_SENTINEL`] / [`ORPHAN_SENTINEL`].
    pub parent: u32,
    /// [`flags`] bitset.
    pub flags: u16,
}

/// The built index. Read-only for search/path; mutated live by [`Index::apply_event`] (M2).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Index {
    arena: StringArena,
    fold: StringArena,
    entries: Vec<Entry>,
    /// Persistent `frn -> entry id` reverse map (all entries, not just dirs): the M2 index
    /// of record identity for journal event apply. `shrink_to_fit` after the bulk build.
    frn_to_id: HashMap<u64, u32>,
    /// Internal id of the root entry (FRN 5), synthesized if the MFT stream omitted it.
    root: u32,
    /// Drive letter used to prefix reconstructed paths (e.g. `C`).
    drive: char,
    /// Abandoned name+fold bytes (from live rename/delete), for the rebuild trigger.
    garbage_bytes: usize,
    /// Count of tombstoned (`DEAD`) entries, for the rebuild trigger.
    dead_entries: usize,
    /// M5 `ef du`: per-entry on-disk allocated size in **clusters** (parallel to `entries`,
    /// `+4 B/entry`). Populated by the fail-soft [`enum_sizes`](crate::volume::UsnVolume::enum_sizes)
    /// pass (keyed by FRN); 0 until set. Kept in lockstep with `entries` on every push.
    #[serde(default)]
    alloc_clusters: Vec<u32>,
    /// Bytes per NTFS cluster, for converting `alloc_clusters` to bytes (M5).
    #[serde(default = "default_cluster_bytes")]
    cluster_bytes: u32,
    /// Count of entries that received a size from the size pass, surfaced by `ef status`
    /// (`sizes: <resolved> / <entries>`) so fail-soft misses are observable, not silent (M5).
    #[serde(default)]
    sizes_resolved: usize,
    /// M6 `search_contig` adoption: entry ids sorted ascending by `fold_off`, the hit->entry
    /// map for the single-pass search (+4 B/entry). **Not serialized**: rebuilt after a bulk
    /// build, a snapshot load, and (in the watch loop) after journal applies. Emptied by any
    /// mutation that changes fold offsets or adds entries (see [`fold_map_ready`]); while
    /// empty, [`search`](Index::search) falls back to the parallel per-entry scan, so the
    /// map is an accelerator, never a correctness dependency.
    ///
    /// [`fold_map_ready`]: Index::fold_map_ready
    #[serde(skip)]
    by_fold_off: Vec<u32>,
    /// Set when a live event may have left [`flags::IS_ORPHAN`] disagreeing with the parent
    /// chain, and cleared by [`refresh_orphans`](Index::refresh_orphans).
    ///
    /// Not serialized, and it does not need to be: the watch loop clears it in the same poll
    /// that sets it, before any snapshot of that poll is written.
    #[serde(skip)]
    orphans_stale: bool,
}

fn default_cluster_bytes() -> u32 {
    4096
}

/// Rough index footprint, for stats output.
#[derive(Debug, Clone, Copy)]
pub struct IndexStats {
    pub entry_count: usize,
    pub arena_bytes: usize,
    pub fold_bytes: usize,
    pub entries_bytes: usize,
    /// Estimated bytes held by the `frn -> id` reverse map (M2).
    pub map_bytes: usize,
    pub total_bytes: usize,
    /// Live (non-`DEAD`) entry count.
    pub live_entries: usize,
    /// Abandoned arena bytes from live rename/delete.
    pub garbage_bytes: usize,
}

impl Index {
    /// Number of entries (including the root).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Internal id of the root entry.
    pub fn root(&self) -> u32 {
        self.root
    }

    /// The entry table (read-only).
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Bytes per NTFS cluster, for converting the M5 cluster sizes to bytes.
    pub fn cluster_bytes(&self) -> u32 {
        self.cluster_bytes
    }

    /// Count of entries that received a size from the M5 [`enum_sizes`] pass. `ef status` reports
    /// `sizes: <resolved> / <len>` so a fail-soft resolution shortfall is observable.
    pub fn sizes_resolved(&self) -> usize {
        self.sizes_resolved
    }

    /// Start the build-time size pass over: reset its counter and record the cluster size the
    /// incoming numbers are denominated in.
    ///
    /// For a caller about to re-run [`enum_sizes`] against an index that already exists: a
    /// resumed snapshot whose sizes are missing. `set_size` counts every hit, so re-running
    /// the pass without resetting would report more sizes than there are entries.
    ///
    /// [`enum_sizes`]: crate::volume::UsnVolume::enum_sizes
    pub fn restart_size_pass(&mut self, cluster_bytes: u32) {
        self.cluster_bytes = cluster_bytes;
        self.sizes_resolved = 0;
    }

    /// Set the drive letter used to prefix reconstructed paths (used by the daemon after load).
    pub fn set_drive(&mut self, drive: char) {
        self.drive = drive;
    }

    /// Point `id`'s parent at `parent`, making a cycle if `parent` is a descendant of `id`.
    ///
    /// Only reachable with `test-util`, and only so a test can build the one shape no
    /// well-formed MFT contains: a parent chain that loops. Every walk over that chain has to
    /// terminate, and before [`Index::ancestors`] five of them did not.
    #[cfg(feature = "test-util")]
    pub fn make_parent_cycle_for_test(&mut self, id: u32, parent: u32) {
        self.entries[id as usize].parent = parent;
    }

    /// Attach the allocated size (in clusters) for `frn` to its entry, if that FRN is indexed
    /// (M5 size pass: fail-soft, keyed by FRN). Sets [`flags::SIZE_TRUNCATED`] when `truncated`.
    /// FRNs not in the index (NTFS system metafiles) are ignored (validated on the T: fixture).
    pub fn set_size(&mut self, frn: u64, alloc_clusters: u32, truncated: bool) {
        if let Some(&id) = self.frn_to_id.get(&frn) {
            self.alloc_clusters[id as usize] = alloc_clusters;
            if truncated {
                self.entries[id as usize].flags |= flags::SIZE_TRUNCATED;
            }
            self.sizes_resolved += 1;
        }
    }

    /// Reconstructed path of a live `frn` (for the live size refresh), or `None` if it is not indexed or
    /// is tombstoned. The watch loop passes this to the volume's by-path stat.
    pub fn path_of_frn(&self, frn: u64) -> Option<String> {
        let &id = self.frn_to_id.get(&frn)?;
        (self.entries[id as usize].flags & flags::DEAD == 0).then(|| self.path(id))
    }

    /// Live-refresh the allocated size of `frn`. Unlike [`set_size`](Self::set_size) this
    /// does **not** bump `sizes_resolved` (that counter reflects the build-time size pass), and it
    /// clears [`flags::SIZE_TRUNCATED`] when the file is no longer oversize.
    pub fn update_size(&mut self, frn: u64, alloc_clusters: u32, truncated: bool) {
        if let Some(&id) = self.frn_to_id.get(&frn) {
            self.alloc_clusters[id as usize] = alloc_clusters;
            if truncated {
                self.entries[id as usize].flags |= flags::SIZE_TRUNCATED;
            } else {
                self.entries[id as usize].flags &= !flags::SIZE_TRUNCATED;
            }
        }
    }

    /// Estimated heap footprint of the core structures.
    pub fn stats(&self) -> IndexStats {
        let arena_bytes = self.arena.byte_len();
        let fold_bytes = self.fold.byte_len();
        let entries_bytes = self.entries.len() * std::mem::size_of::<Entry>();
        // SwissTable: (u64 key + u32 val = 12) + 1 control byte per bucket.
        let map_bytes = self.frn_to_id.capacity() * 13;
        IndexStats {
            entry_count: self.entries.len(),
            arena_bytes,
            fold_bytes,
            entries_bytes,
            map_bytes,
            total_bytes: arena_bytes + fold_bytes + entries_bytes + map_bytes,
            live_entries: self.entries.len() - self.dead_entries,
            garbage_bytes: self.garbage_bytes,
        }
    }

    /// The original (non-folded) name of `id`.
    pub fn name(&self, id: u32) -> &str {
        let e = &self.entries[id as usize];
        // The arena only ever receives valid UTF-8 (pushed from `String`).
        std::str::from_utf8(self.arena.get(e.name_off, e.name_len)).unwrap_or("")
    }

    /// Walk from `id` up to (but not including) the root, yielding each id on the way.
    ///
    /// One walker for a chain that was being walked seven times: here, and in `depth`,
    /// `path_len`, `path_hits`, `folded_path_bytes` and twice in `du`. Five of those shared a
    /// three-arm match; the two in `du` used a different rule, which is the only reason `du`
    /// was safe against a parent cycle and the other five would spin forever (two of them
    /// growing a `Vec` as they went, so the spin ends in OOM rather than a hang).
    ///
    /// This one is bounded by construction: an entry cannot appear twice in a chain of at most
    /// `entries.len()` steps, so the count is the stop. A cycle then renders as a truncated
    /// path rather than as a hung daemon, which is the right trade for a shape that a
    /// well-formed MFT never contains but a partially-replayed journal could install.
    pub(crate) fn ancestors(&self, id: u32) -> Ancestors<'_> {
        Ancestors {
            index: self,
            cur: Some(id),
            left: MAX_PATH_DEPTH,
            orphaned: false,
        }
    }

    /// Reconstruct the full path of `id` by walking the parent chain.
    ///
    /// - Root -> `"<drive>:\"`.
    /// - A chain that reaches [`ORPHAN_SENTINEL`] -> `"<orphan>\..."` (never a fake
    ///   `<drive>:\...`).
    pub fn path(&self, id: u32) -> String {
        if id == self.root {
            return format!("{}:\\", self.drive);
        }

        let mut walk = self.ancestors(id);
        let mut parts: Vec<&str> = Vec::new();
        for cur in walk.by_ref() {
            parts.push(self.name(cur));
        }
        let orphaned = walk.orphaned();
        parts.reverse();
        let joined = parts.join("\\");
        if orphaned {
            format!("<orphan>\\{joined}")
        } else {
            format!("{}:\\{joined}", self.drive)
        }
    }

    /// Rebuild the `fold_off -> id` hit map for [`search_contig`]. One parallel sort of all
    /// entry ids (~100-400 ms at 6M when unsorted; near-free right after a bulk build, where
    /// ids are already in fold-arena order). Called after build / snapshot load / journal
    /// applies, never on the search path.
    ///
    /// [`search_contig`]: Index::search_contig
    pub fn rebuild_fold_map(&mut self) {
        let order = self.fold_order();
        self.adopt_fold_map(order);
    }

    /// The sorted map [`rebuild_fold_map`](Self::rebuild_fold_map) would install, computed
    /// without installing it.
    ///
    /// The sort is the whole cost and it is a *read*; only the assignment needs exclusive
    /// access. Split so the daemon can spend the 100-400 ms under its shared lock, where
    /// searches run alongside, and take the exclusive one for the move alone. Held as an
    /// exclusive lock it was a 100-400 ms stall on nearly every poll, because any create or
    /// rename empties the map and a live desktop has one most seconds.
    pub fn fold_order(&self) -> Vec<u32> {
        let mut v: Vec<u32> = (0..self.entries.len() as u32).collect();
        let entries = &self.entries;
        v.par_sort_unstable_by_key(|&id| entries[id as usize].fold_off);
        v
    }

    /// Install a map from [`fold_order`](Self::fold_order), unless the index moved on while it
    /// was being computed: a map that does not cover every entry is not merely stale, it maps
    /// hits to the wrong rows. Rejecting it costs nothing: [`fold_map_ready`] then reports
    /// false and searches take the parallel scan until the next poll offers a fresh one.
    ///
    /// [`fold_map_ready`]: Index::fold_map_ready
    pub fn adopt_fold_map(&mut self, order: Vec<u32>) {
        if order.len() == self.entries.len() {
            self.by_fold_off = order;
        }
    }

    /// Whether a live event may have left [`flags::IS_ORPHAN`] out of step with the parent
    /// chain; see [`refresh_orphans`](Self::refresh_orphans).
    pub fn orphans_stale(&self) -> bool {
        self.orphans_stale
    }

    /// Re-derive [`flags::IS_ORPHAN`] for every entry from its parent chain.
    ///
    /// `upsert` maintains the flag incrementally, by giving a new entry its parent's. That is
    /// exact while a tree is only being *built* (parents arrive before children) and wrong
    /// as soon as one is **moved**: a directory that stops being an orphan takes the flag off
    /// itself and leaves it on everything underneath, which stays out of every search that
    /// does not ask for orphans. The reverse case is as quiet and worse: a subtree that
    /// *became* unreachable keeps answering searches with paths beginning `<orphan>\`.
    ///
    /// One O(n) pass, run by the watch loop in the poll that set the flag, so the cost lands
    /// on the rare event that moves a directory across the boundary rather than on every
    /// event.
    pub fn refresh_orphans(&mut self) {
        super::index::build::mark_transitive_orphans(&mut self.entries, self.root);
        self.orphans_stale = false;
    }

    /// True when the contig hit map covers every entry (complete and in sync). Tombstoning
    /// keeps the map valid (`DEAD` is filtered at hit time; `fold_off` is untouched); a
    /// create or rename empties it until the next [`rebuild_fold_map`](Self::rebuild_fold_map).
    pub fn fold_map_ready(&self) -> bool {
        !self.entries.is_empty() && self.by_fold_off.len() == self.entries.len()
    }

    /// Return the ids of entries whose name contains `query` as a substring.
    ///
    /// - `case_insensitive` (the default for the CLI) scans the fold arena, via the
    ///   single-pass contig search when the hit map is ready (adopted M6: -39 % on selective
    ///   queries, >= parity on broad ones), else the parallel per-entry scan.
    /// - Orphans are excluded unless `include_orphans` is set.
    /// - The root entry is never returned.
    ///
    /// The `memmem::Finder` is built once and shared across rayon worker threads.
    pub fn search(&self, query: &str, case_insensitive: bool, include_orphans: bool) -> Vec<u32> {
        if case_insensitive {
            // Single-sourced with the fold arena + highlight (see `index::fold`).
            let needle = fold::fold_query(query);
            // Empty needle = match-everything: the parallel scan is O(entries), while the
            // contig pass would visit every byte offset. Keep empties on the scan path.
            if !needle.is_empty() && self.fold_map_ready() {
                return self.contig_scan(&needle, include_orphans, &self.by_fold_off);
            }
            let finder = memmem::Finder::new(needle.as_bytes());
            self.scan(&finder, true, include_orphans)
        } else {
            let finder = memmem::Finder::new(query.as_bytes());
            self.scan(&finder, false, include_orphans)
        }
    }

    /// Single-pass search (adopted M6; formerly the M3 "insurance switch"): one `memmem`
    /// over the whole contiguous fold buffer, mapping each hit back to an entry by binary
    /// search over `by_fold_off` (entry ids sorted ascending by `fold_off`). Single-threaded,
    /// case-insensitive only. The `0x00` separators guarantee a hit never spans two names.
    /// This explicit-map form is kept for the A/B bench and the agreement tests; the adopted
    /// path is [`search`](Self::search) routing through the internal map.
    pub fn search_contig(
        &self,
        query: &str,
        include_orphans: bool,
        by_fold_off: &[u32],
    ) -> Vec<u32> {
        let needle = fold::fold_query(query);
        self.contig_scan(&needle, include_orphans, by_fold_off)
    }

    fn contig_scan(&self, needle: &str, include_orphans: bool, by_fold_off: &[u32]) -> Vec<u32> {
        let finder = memmem::Finder::new(needle.as_bytes());
        let nlen = needle.len();
        let hay = self.fold.buf();
        let mut out = Vec::new();
        let mut last = u32::MAX;
        let mut pos = 0usize;
        for h in finder.find_iter(hay) {
            let h = h as u32;
            // Greatest entry whose fold_off <= h.
            pos = self.first_after(by_fold_off, pos, h);
            if pos == 0 {
                continue;
            }
            let id = by_fold_off[pos - 1];
            if id == last {
                continue; // multiple hits within one name -> one id
            }
            let e = &self.entries[id as usize];
            // The hit must lie fully inside this entry's name (not run into the separator).
            if h as usize + nlen > e.fold_off as usize + e.fold_len as usize {
                continue;
            }
            if id == self.root || e.flags & flags::DEAD != 0 {
                continue;
            }
            if !include_orphans && e.flags & flags::IS_ORPHAN != 0 {
                continue;
            }
            out.push(id);
            last = id;
        }
        out
    }

    /// Index of the first entry in `by_fold_off` whose `fold_off` exceeds `h`, searching
    /// forward from `from`, which the caller guarantees is not past the answer.
    ///
    /// A plain `partition_point` over the whole map gives the same number, and gave it at a
    /// price that scaled with the wrong thing. `find_iter` yields offsets in increasing order,
    /// so the answer only ever moves forward; bisecting all six million ids for each hit threw
    /// that away and paid ~23 random probes to land, almost always, on the very next id. For a
    /// one-character query that is the whole cost of the search: `a` occurs about ten million
    /// times in a 143 MB fold buffer, so ~230 million random probes into a 130 MB table,
    /// seconds, single-threaded, holding the index read lock, on the first keystroke of every
    /// search.
    ///
    /// Galloping rather than a straight walk, because the two extremes want opposite things: a
    /// dense needle wants the next answer to be a step away (it is), and a selective one must
    /// not walk millions of entries to reach its single hit. Doubling until the bracket
    /// contains the answer, then bisecting inside it, is O(log Δ) either way.
    fn first_after(&self, by_fold_off: &[u32], from: usize, h: u32) -> usize {
        let off = |i: usize| self.entries[by_fold_off[i] as usize].fold_off;
        let n = by_fold_off.len();
        if from >= n || off(from) > h {
            return from;
        }
        let (mut lo, mut step) = (from, 1usize);
        while lo + step < n && off(lo + step) <= h {
            lo += step;
            step *= 2;
        }
        let hi = lo.saturating_add(step).min(n);
        lo + by_fold_off[lo..hi].partition_point(|&id| self.entries[id as usize].fold_off <= h)
    }

    fn scan(&self, finder: &memmem::Finder<'_>, folded: bool, include_orphans: bool) -> Vec<u32> {
        (0..self.entries.len() as u32)
            .into_par_iter()
            .filter(|&id| {
                if id == self.root {
                    return false;
                }
                let e = &self.entries[id as usize];
                if e.flags & flags::DEAD != 0 {
                    return false;
                }
                if !include_orphans && e.flags & flags::IS_ORPHAN != 0 {
                    return false;
                }
                let hay = if folded {
                    self.fold.get(e.fold_off, e.fold_len)
                } else {
                    self.arena.get(e.name_off, e.name_len)
                };
                finder.find(hay).is_some()
            })
            .collect()
    }
}

/// How deep a path this walk will follow before deciding the chain is cyclic.
///
/// Bounded by the entry count at first, which terminates but is not a bound worth having: on a
/// six-million-entry index a cycle then yields six million components, a multi-megabyte
/// `String` per row returned, built inside the result loop and inside a rayon filter. That is
/// an out-of-memory, not the "truncated path" the design intends. Windows itself cannot reach
/// four thousand components (an extended path is ~32,767 characters and a component is at
/// least two), so nothing legitimate is cut.
const MAX_PATH_DEPTH: usize = 4_096;

/// The chain of ids from a starting entry up to (but not including) the root.
///
/// See [`Index::ancestors`] for why this exists and why it is bounded.
pub(crate) struct Ancestors<'a> {
    index: &'a Index,
    cur: Option<u32>,
    /// Steps left before the walk is declared cyclic. One per entry is more than any acyclic
    /// chain can need.
    left: usize,
    orphaned: bool,
}

impl Ancestors<'_> {
    /// Whether the chain ended at [`ORPHAN_SENTINEL`]: the caller renders `<orphan>\...` rather
    /// than a drive letter. Only meaningful once the iterator is exhausted.
    pub(crate) fn orphaned(&self) -> bool {
        self.orphaned
    }
}

impl Iterator for Ancestors<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let cur = self.cur?;
        if self.left == 0 {
            self.cur = None;
            return None;
        }
        self.left -= 1;
        let parent = self.index.entries[cur as usize].parent;
        self.cur = match parent {
            ORPHAN_SENTINEL => {
                self.orphaned = true;
                None
            }
            ROOT_SENTINEL => None,
            p if p == self.index.root => None,
            p if (p as usize) >= self.index.entries.len() => None,
            p => Some(p),
        };
        Some(cur)
    }
}
