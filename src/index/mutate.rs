//! Live index mutation: apply USN journal events (create / delete / rename) to a built
//! [`Index`], plus arena-garbage accounting for the rebuild trigger.
//!
//! Event-apply rules follow the P10 measurements: reasons
//! are cumulative (OR-ed) across records until `CLOSE`, so apply is **idempotent**: we
//! reconcile (upsert) the index to each record's authoritative `(frn, parent_frn, name,
//! attributes)`. Only `FILE_DELETE` removes an entry; `RENAME_OLD_NAME` (the pre-image) is
//! skipped; `HARD_LINK_CHANGE` and non-name reasons (`DATA_*`, ...) are ignored in v1.

use crate::volume::{reason, UsnEvent, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

use super::{flags, Entry, Index, ORPHAN_SENTINEL};

impl Index {
    /// Apply one USN journal event, keeping [`entries`](Index::entries) and the `frn -> id`
    /// map consistent. Idempotent to the cumulative-reason records NTFS emits (P10).
    pub fn apply_event(&mut self, ev: UsnEvent<'_>) {
        let r = ev.reason;

        // FILE_DELETE wins even when coalesced with CREATE (created+deleted between reads
        // => net gone). Removal is the only op that drops an entry.
        if r & reason::FILE_DELETE != 0 {
            self.remove(ev.frn);
            return;
        }

        // The pre-image of a rename carries the OLD name/parent; the following
        // RENAME_NEW_NAME record carries the truth. Skip OLD-only records.
        if r & reason::RENAME_OLD_NAME != 0 && r & reason::RENAME_NEW_NAME == 0 {
            return;
        }

        // Create, or the post-image of a rename/move: reconcile the index to this record.
        if r & (reason::FILE_CREATE | reason::RENAME_NEW_NAME) != 0 {
            self.upsert(ev.frn, ev.parent_frn, ev.name_utf16, ev.attributes);
        }
    }

    /// Fraction of arena bytes that are garbage (abandoned by rename/delete), in `[0, 1]`.
    /// The watch loop rebuilds when this exceeds its threshold.
    pub fn garbage_ratio(&self) -> f64 {
        let total = self.arena.byte_len() + self.fold.byte_len();
        if total == 0 {
            0.0
        } else {
            self.garbage_bytes as f64 / total as f64
        }
    }

    /// Number of tombstoned (`DEAD`) entries.
    pub fn dead_count(&self) -> usize {
        self.dead_entries
    }

    /// Tombstone the entry for `frn` (if present) and drop its map key, so a later
    /// record-number reuse inserts a fresh entry. The slot is retained so existing parent
    /// ids stay valid.
    fn remove(&mut self, frn: u64) {
        if let Some(id) = self.frn_to_id.remove(&frn) {
            let e = &mut self.entries[id as usize];
            if e.flags & flags::DEAD == 0 {
                e.flags |= flags::DEAD;
                let freed = e.name_len as usize + e.fold_len as usize;
                self.dead_entries += 1;
                self.garbage_bytes += freed;
            }
        }
    }

    /// Insert (new frn) or reconcile (known frn) the entry to `(frn, parent_frn, name,
    /// attributes)`. Old name bytes on a reconcile become garbage.
    fn upsert(&mut self, frn: u64, parent_frn: u64, name_utf16: &[u16], attributes: u32) {
        // Both branches invalidate the contig hit map: a create appends an id the map does
        // not cover, a reconcile moves `fold_off`. (A tombstone does neither, so `remove`
        // leaves the map valid.) Search falls back to the parallel scan until the watch
        // loop rebuilds it (`Watched::sync`).
        self.by_fold_off = Vec::new();
        let parent = self.resolve_parent(parent_frn);
        let mut flag_bits = 0u16;
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            flag_bits |= flags::IS_DIR;
        }
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            flag_bits |= flags::REPARSE_POINT;
        }
        // Orphan if the parent is unknown, or transitively if the parent is itself an
        // orphan. Parents are created before children (USN order), so this propagates the
        // full chain incrementally.
        if parent == ORPHAN_SENTINEL || self.entries[parent as usize].flags & flags::IS_ORPHAN != 0
        {
            flag_bits |= flags::IS_ORPHAN;
        }

        let (name, fold) = decode_fold(name_utf16);
        let (name_off, name_len) = self.arena.push(name.as_bytes());
        let (fold_off, fold_len) = self.fold.push(fold.as_bytes());

        match self.frn_to_id.get(&frn).copied() {
            Some(id) => {
                let e = &mut self.entries[id as usize];
                let old = e.name_len as usize + e.fold_len as usize;
                e.name_off = name_off;
                e.name_len = name_len;
                e.fold_off = fold_off;
                e.fold_len = fold_len;
                e.parent = parent;
                // `SIZE_TRUNCATED` survives, because a rename does not change how big a file
                // is. Overwriting the whole bitset dropped it while `alloc_clusters` kept the
                // saturated `u32::MAX`, so renaming a file over 16 TiB made `du` report a
                // subtree total that was short and `truncated: false`, the one thing the M5
                // contract says never happens silently.
                // A directory that crossed the orphan boundary takes its whole subtree with
                // it, and the rule above only reaches the entry that moved. Note it and let
                // the watch loop re-derive the flag for everyone in this poll; descendants
                // left behind on the wrong side are invisible to every search that does not
                // ask for orphans, or visible with a path beginning `<orphan>\`.
                let was_orphan = e.flags & flags::IS_ORPHAN != 0;
                let is_orphan = flag_bits & flags::IS_ORPHAN != 0;
                // Directory according to *either* reading. Only a directory can have a subtree
                // to strand, so a file crossing the boundary needs no pass, but asking only
                // the incoming record would miss the case where a re-statement arrives without
                // the attribute, and a missed pass is silent where a spare one is merely slow.
                let a_directory = (flag_bits | e.flags) & flags::IS_DIR != 0;
                e.flags = flag_bits | (e.flags & flags::SIZE_TRUNCATED);
                if was_orphan != is_orphan && a_directory {
                    self.orphans_stale = true;
                }
                self.garbage_bytes += old;
            }
            None => {
                let id = self.entries.len() as u32;
                self.entries.push(Entry {
                    name_off,
                    name_len,
                    fold_off,
                    fold_len,
                    parent,
                    flags: flag_bits,
                });
                // Keep the M5 size column in lockstep with `entries` (new entry -> size 0 until a
                // live size refresh fills it). INVARIANT: alloc_clusters.len() == entries.len().
                self.alloc_clusters.push(0);
                self.frn_to_id.insert(frn, id);
            }
        }
    }

    /// Resolve a parent FRN to an internal id, or [`ORPHAN_SENTINEL`] if not indexed. The
    /// root's FRN is in the map (-> its id), so top-level parents resolve like any other.
    fn resolve_parent(&self, parent_frn: u64) -> u32 {
        self.frn_to_id
            .get(&parent_frn)
            .copied()
            .unwrap_or(ORPHAN_SENTINEL)
    }
}

/// Decode a UTF-16 name (lossy) into its original and lowercase-folded UTF-8 forms,
/// mirrors the build path so live entries match built ones.
fn decode_fold(name_utf16: &[u16]) -> (String, String) {
    let mut name = String::new();
    let mut fold = String::new();
    for r in char::decode_utf16(name_utf16.iter().copied()) {
        let c = r.unwrap_or('\u{FFFD}');
        name.push(c);
        // Through the fold module, not an inlined copy of it. `fold.rs` says in capitals that
        // every caller changes in lockstep, "which is the whole point of centralizing it",
        // and this was a caller that did not go through it, and was missing from its own list
        // of callers. Byte-identical today; the deferred ASCII fast path would have updated
        // the bulk build and left live-created entries folding differently, unsearchable until
        // the next rebuild.
        super::fold::fold_char_into(c, &mut fold);
    }
    (name, fold)
}
