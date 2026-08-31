//! Two-pass index construction from a [`UsnVolume`] stream.
//!
//! Pass 1 (enumerate): append each name to the arenas, record its FRN and parent
//! FRN, and (**for directories only**) register `frn -> id` in the resolution map.
//! Pass 2 (resolve): map each entry's parent FRN to an internal id.
//!
//! The map is a **full** `frn -> id` map (all entries): M2 journal apply needs to resolve
//! any file's FRN, not just directories, and the same map is persisted into the [`Index`].
//! Parent resolution stays unambiguous because a parent is always a directory and directory
//! FRNs are unique (NTFS has no dir hardlinks); files enumerate one record per FRN (P9), so
//! the map is 1:1. (M1 used a dir-only temp map to save build memory; M2 keeps the full map
//! anyway, so the distinction no longer buys anything.)

use std::collections::HashMap;

use crate::volume::{UsnVolume, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

use super::arena::StringArena;
use super::{flags, Entry, Index, ORPHAN_SENTINEL, ROOT_FRN, ROOT_SENTINEL};

/// Build an [`Index`] from every record of `vol`. `drive` is the letter used to
/// prefix reconstructed paths (e.g. `'C'`).
/// An empty index (just the synthesized root) for `drive`, a placeholder the
/// daemon serves during the initial enumeration so the pipe can accept
/// connections immediately (searches return nothing, but the daemon reports
/// `BUILDING`, not "not running"). Replaced by the real index once acquired.
pub fn empty_index(drive: char) -> Index {
    let mut index = Builder::new(drive, None).finish();
    index.rebuild_fold_map();
    index
}

pub fn build_from_volume(vol: &mut dyn UsnVolume, drive: char) -> anyhow::Result<Index> {
    let hint = vol.size_hint();
    let mut b = Builder::new(drive, hint);
    vol.enum_records(&mut |rec| {
        b.add(rec.name_utf16, rec.frn, rec.parent_frn, rec.attributes);
    })?;
    let mut index = b.finish();

    // M5 additive size pass: attach an on-disk allocated size to each FRN. Fail-soft: a size
    // source that yields nothing (default no-op) leaves every entry at 0; the structure above is
    // already complete and correct regardless. Keyed by FRN, so NTFS system metafiles that the
    // size pass sees but ENUM never indexed are dropped (validated on the T: fixture).
    index.cluster_bytes = vol.cluster_bytes();
    if let Err(e) = vol.enum_sizes(&mut |frn, alloc_clusters, truncated| {
        index.set_size(frn, alloc_clusters, truncated);
    }) {
        // Fail-soft: keep the (complete) index; `ef du` shows 0 for unsized entries and
        // `ef status` reports the low resolved count. Never fail the build over sizes.
        tracing::warn!("size pass failed, du sizes unavailable: {e:#}");
    }
    // Contig hit map (M6): a bulk build appends names in fold-arena order, so this sort is
    // near-free here; doing it inside the build covers every acquisition path that ends in
    // a fresh enumeration (enumerate / reenumerate / garbage compaction).
    index.rebuild_fold_map();
    Ok(index)
}

struct Builder {
    drive: char,
    arena: StringArena,
    fold: StringArena,
    entries: Vec<Entry>,
    parent_frns: Vec<u64>,
    frn_to_id: HashMap<u64, u32>,
    root_id: Option<u32>,
    // Reused per-record scratch buffers to avoid a String allocation per name.
    name_buf: String,
    fold_buf: String,
}

impl Builder {
    fn new(drive: char, hint: Option<u64>) -> Self {
        // Pre-reserve from the entry-count estimate (P13: MFT segment count, ~1.045x actual
        // on real C:) so the bulk build makes one allocation per structure with no pow2
        // growth doublings, whose freed intermediate blocks the windows-gnu allocator would
        // retain, inflating PrivateUsage. Arena/fold byte reserves
        // use the measured ~20/22 B/entry averages (a slight under-reserve just costs one
        // small final doubling). `hint == None` (FakeVolume) reserves nothing.
        let n = hint.unwrap_or(0) as usize;
        Self {
            drive,
            arena: StringArena::with_capacity(false, n.saturating_mul(20)),
            fold: StringArena::with_capacity(true, n.saturating_mul(22)),
            entries: Vec::with_capacity(n),
            parent_frns: Vec::with_capacity(n),
            frn_to_id: HashMap::with_capacity(n),
            root_id: None,
            name_buf: String::new(),
            fold_buf: String::new(),
        }
    }

    fn add(&mut self, name_utf16: &[u16], frn: u64, parent_frn: u64, attributes: u32) {
        self.name_buf.clear();
        self.fold_buf.clear();
        // Decode UTF-16 with lossy replacement (matches `String::from_utf16_lossy`), folding to
        // lowercase into a parallel buffer in the same pass. The fold goes through the shared
        // `index::fold` so the client-side highlight folds identically (see `index::fold`).
        for r in char::decode_utf16(name_utf16.iter().copied()) {
            let c = r.unwrap_or('\u{FFFD}');
            self.name_buf.push(c);
            super::fold::fold_char_into(c, &mut self.fold_buf);
        }

        let (name_off, name_len) = self.arena.push(self.name_buf.as_bytes());
        let (fold_off, fold_len) = self.fold.push(self.fold_buf.as_bytes());

        let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let mut flag_bits = 0u16;
        if is_dir {
            flag_bits |= flags::IS_DIR;
        }
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            flag_bits |= flags::REPARSE_POINT;
        }

        let id = self.entries.len() as u32;
        self.entries.push(Entry {
            name_off,
            name_len,
            fold_off,
            fold_len,
            parent: 0,
            flags: flag_bits,
        });
        self.parent_frns.push(parent_frn);

        // Full frn -> id map, persisted for M2 event apply (see the module note).
        self.frn_to_id.insert(frn, id);
        if frn == ROOT_FRN {
            self.root_id = Some(id);
        }
    }

    fn finish(mut self) -> Index {
        // If the root (FRN 5) never appeared, synthesize an empty-named root so that
        // top-level items (parent FRN 5) resolve rather than becoming orphans.
        let root = match self.root_id {
            Some(id) => id,
            None => {
                let (name_off, name_len) = self.arena.push(&[]);
                let (fold_off, fold_len) = self.fold.push(&[]);
                let id = self.entries.len() as u32;
                self.entries.push(Entry {
                    name_off,
                    name_len,
                    fold_off,
                    fold_len,
                    parent: 0,
                    flags: flags::IS_DIR,
                });
                self.parent_frns.push(ROOT_FRN);
                self.frn_to_id.insert(ROOT_FRN, id);
                id
            }
        };

        // Pass 2: resolve parent FRNs to internal ids.
        for i in 0..self.entries.len() {
            if i == root as usize {
                self.entries[i].parent = ROOT_SENTINEL;
                continue;
            }
            match self.frn_to_id.get(&self.parent_frns[i]) {
                // Guard against a (malformed) self-referencing non-root entry.
                Some(&pid) if pid as usize != i => self.entries[i].parent = pid,
                _ => {
                    self.entries[i].parent = ORPHAN_SENTINEL;
                    self.entries[i].flags |= flags::IS_ORPHAN;
                }
            }
        }

        // Pass 3: transitively mark orphans. An entry whose parent chain does not
        // reach the root can only render under `<orphan>\`, so it is flagged too and
        // excluded from search by default (not just the directly-parentless entry).
        mark_transitive_orphans(&mut self.entries, root);

        // No shrink_to_fit: the structures were pre-reserved to the size hint (see `new`),
        // so they hold a single tight allocation. Shrinking would reallocate and *free* a
        // large block that the allocator retains anyway: the opposite of the intent.

        let alloc_clusters = vec![0u32; self.entries.len()];
        Index {
            arena: self.arena,
            fold: self.fold,
            entries: self.entries,
            frn_to_id: self.frn_to_id,
            root,
            drive: self.drive,
            garbage_bytes: 0,
            dead_entries: 0,
            alloc_clusters,
            cluster_bytes: 4096,
            sizes_resolved: 0,
            by_fold_off: Vec::new(), // filled by rebuild_fold_map in build_from_volume
            orphans_stale: false,    // just derived, by definition in step
        }
    }
}

/// Make [`flags::IS_ORPHAN`] agree with reachability: set on every entry whose parent chain
/// does not terminate at `root`, and **cleared** on every entry whose chain does.
///
/// Memoized, so the whole forest is processed in O(n); a `visiting` marker guards against
/// (theoretically impossible) cycles.
///
/// Clearing matters only to the second caller. At build time nothing carries the flag yet, so
/// it is a no-op there, but a live re-parent can make an entry stop being an orphan, and the
/// incremental rule in `upsert` (inherit the parent's flag) only reaches the entry that moved,
/// never the subtree hanging off it. Re-deriving both directions from the chain is what makes
/// the answer independent of which events arrived in what order.
pub(super) fn mark_transitive_orphans(entries: &mut [Entry], root: u32) {
    const UNKNOWN: u8 = 0;
    const REACHES_ROOT: u8 = 1;
    const ORPHAN: u8 = 2;
    const VISITING: u8 = 3;

    let mut state = vec![UNKNOWN; entries.len()];
    state[root as usize] = REACHES_ROOT;

    let mut stack: Vec<u32> = Vec::new();
    for start in 0..entries.len() as u32 {
        if state[start as usize] != UNKNOWN {
            continue;
        }
        stack.clear();
        let mut cur = start;
        let verdict = loop {
            match state[cur as usize] {
                REACHES_ROOT => break REACHES_ROOT,
                ORPHAN | VISITING => break ORPHAN, // VISITING => cycle, treat as orphan
                _ => {}
            }
            state[cur as usize] = VISITING;
            stack.push(cur);
            match entries[cur as usize].parent {
                ROOT_SENTINEL => break REACHES_ROOT,
                ORPHAN_SENTINEL => break ORPHAN,
                p => cur = p,
            }
        };
        for &id in &stack {
            state[id as usize] = verdict;
            if verdict == ORPHAN {
                entries[id as usize].flags |= flags::IS_ORPHAN;
            } else {
                entries[id as usize].flags &= !flags::IS_ORPHAN;
            }
        }
    }
    // Never in a stack (it is the one entry seeded as reachable) so it is cleared here.
    entries[root as usize].flags &= !flags::IS_ORPHAN;
}
