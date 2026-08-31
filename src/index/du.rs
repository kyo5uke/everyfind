//! M5 `ef du`: directory disk-usage aggregation over the resident index.
//!
//! Sizes come from the fail-soft [`enum_sizes`](crate::volume::UsnVolume::enum_sizes) pass
//! (per-entry allocated **clusters**, keyed by FRN). Because the index holds **one entry per FRN**
//! (P9), a hardlinked file's size is counted **once**, under its single indexed name's directory,
//! the correct disk-occupancy semantics that a per-name directory walk (method B) gets wrong. The
//! MFT parent structure does not nest a reparse target's children under the reparse point, so
//! aggregation never follows a junction/symlink (both pinned by `tests/du.rs`).
//!
//! A `du` query computes subtree totals fresh in **O(N)** (depth-sorted child->parent propagation);
//! nothing is maintained incrementally, so a live size change only touches a single entry's
//! `alloc_clusters` and the next `du` re-aggregates correctly.

use super::{flags, Index, ROOT_SENTINEL};

/// One row of an `ef du` report: a child (or descendant, with `--depth`) of the queried path and
/// its recursive subtree total.
#[derive(Debug, Clone)]
pub struct DuRow {
    /// Internal id (the daemon turns this into a path + is_dir for the IPC response).
    pub id: u32,
    /// Depth of this row below the queried path (1 = immediate child).
    pub depth: u32,
    /// Recursive subtree size in clusters (multiply by `cluster_bytes` for bytes).
    pub clusters: u64,
    /// Some file under this row saturated the 16 TiB cap; its total is a lower bound.
    pub truncated: bool,
}

/// The result of a `du` query: the queried subtree's total plus its largest children.
#[derive(Debug, Clone)]
pub struct DuReport {
    /// Recursive total size of the queried path, in clusters.
    pub total_clusters: u64,
    /// Bytes per cluster, for converting `clusters` fields to bytes.
    pub cluster_bytes: u32,
    /// Some file in the queried subtree exceeded the 16 TiB cap; totals are a lower bound.
    pub truncated: bool,
    /// Child/descendant rows, largest first (capped to the requested top-N).
    pub rows: Vec<DuRow>,
}

impl Index {
    /// Resolve a path string (`"C:\Users\foo"`, `"C:\"`, or `"C:"`) to an internal id, or `None`
    /// if it is not in the index. Case-insensitive (Windows paths), via the shared fold. A bare
    /// drive (`"C:\"`) resolves to the root.
    pub fn resolve_path(&self, path: &str) -> Option<u32> {
        // Strip an optional `<drive>:` prefix; the remainder is `\`-separated components.
        let rest = match path.split_once(':') {
            Some((_drive, rest)) => rest,
            None => path,
        };
        let mut cur = self.root;
        // Both separators, because the query language already accepts both (`path:` normalises
        // a typed `/`) and a shell prompt is where `ef du C:/Users/me` gets typed. Splitting on
        // `\` alone made that one component, no match, and "path not found in the index".
        for comp in rest.split(['\\', '/']).filter(|c| !c.is_empty()) {
            let needle = super::fold::fold_query(comp);
            let child = (0..self.entries.len() as u32).find(|&id| {
                let e = &self.entries[id as usize];
                e.parent == cur
                    && e.flags & flags::DEAD == 0
                    && self.fold.get(e.fold_off, e.fold_len) == needle.as_bytes()
            });
            cur = child?;
        }
        Some(cur)
    }

    /// Disk-usage report for the subtree rooted at `path_id`: the recursive total, and the largest
    /// descendants within `depth` levels (`depth` >= 1; 1 = immediate children), capped to `top_n`
    /// rows. Sizes are **allocated clusters** (the default; the caller converts to bytes and, for
    /// `--real`, replaces them with on-demand logical sizes over the returned rows).
    pub fn du(&self, path_id: u32, depth: u32, top_n: usize) -> DuReport {
        let (subtree, trunc) = self.subtree_clusters();
        let depth = depth.max(1);

        let mut rows: Vec<DuRow> = Vec::new();
        for id in 0..self.entries.len() as u32 {
            if id == path_id {
                continue;
            }
            let e = &self.entries[id as usize];
            if e.flags & flags::DEAD != 0 {
                continue;
            }
            if let Some(rel) = self.rel_depth_under(id, path_id, depth) {
                rows.push(DuRow {
                    id,
                    depth: rel,
                    clusters: subtree[id as usize],
                    truncated: trunc[id as usize],
                });
            }
        }
        // Largest first; ties broken by name for a stable, human-friendly order.
        rows.sort_by(|a, b| {
            b.clusters
                .cmp(&a.clusters)
                .then_with(|| self.name(a.id).cmp(self.name(b.id)))
        });
        rows.truncate(top_n);

        DuReport {
            total_clusters: subtree[path_id as usize],
            cluster_bytes: self.cluster_bytes,
            truncated: trunc[path_id as usize],
            rows,
        }
    }

    /// If `id` is a descendant of `ancestor` within `max_depth` levels, its relative depth
    /// (1 = immediate child); else `None`. O(max_depth) per call.
    fn rel_depth_under(&self, id: u32, ancestor: u32, max_depth: u32) -> Option<u32> {
        let n = self.entries.len() as u32;
        let mut cur = id;
        for step in 1..=max_depth {
            let p = self.entries[cur as usize].parent;
            if p == ancestor {
                return Some(step);
            }
            if p >= n || p == cur {
                return None; // reached a sentinel / self-root without hitting `ancestor`
            }
            cur = p;
        }
        None
    }

    /// Recursive subtree allocated clusters (and a truncated marker) for **every** entry, in O(N):
    /// seed each entry with its own size, then add each node's subtree into its parent in order of
    /// decreasing depth so children are complete before their parent is folded up. `DEAD` entries
    /// contribute 0; orphans (parent chain not reaching root) do not propagate into the root total.
    fn subtree_clusters(&self) -> (Vec<u64>, Vec<bool>) {
        let n = self.entries.len();
        let mut subtree = vec![0u64; n];
        let mut trunc = vec![false; n];
        for (id, e) in self.entries.iter().enumerate() {
            if e.flags & flags::DEAD != 0 {
                continue;
            }
            subtree[id] = self.alloc_clusters[id] as u64;
            trunc[id] = e.flags & flags::SIZE_TRUNCATED != 0;
        }

        let depth = self.depths();
        let maxd = depth
            .iter()
            .copied()
            .filter(|&d| d != u32::MAX)
            .max()
            .unwrap_or(0);
        // Counting-sort ids into depth buckets, then fold deepest-first (O(N + maxd)).
        let mut by_depth: Vec<Vec<u32>> = vec![Vec::new(); maxd as usize + 1];
        for (id, &d) in depth.iter().enumerate() {
            if d != u32::MAX {
                by_depth[d as usize].push(id as u32);
            }
        }
        for d in (1..=maxd).rev() {
            for &id in &by_depth[d as usize] {
                let p = self.entries[id as usize].parent;
                // Only a real parent id accumulates (root parent is ROOT_SENTINEL = u32::MAX).
                if (p as usize) < n && p != id {
                    subtree[p as usize] += subtree[id as usize];
                    trunc[p as usize] |= trunc[id as usize];
                }
            }
        }
        (subtree, trunc)
    }

    /// Depth of every entry from its root along the parent chain (root/orphan-root = 0), memoized,
    /// O(N). A self-referencing or sentinel parent terminates a chain at depth 0.
    fn depths(&self) -> Vec<u32> {
        let n = self.entries.len();
        let mut depth = vec![u32::MAX; n];
        let mut stack: Vec<u32> = Vec::new();
        for start in 0..n as u32 {
            if depth[start as usize] != u32::MAX {
                continue;
            }
            stack.clear();
            let mut cur = start;
            // Walk up to a node with known depth or a chain root (parent = sentinel / self).
            let base = loop {
                if depth[cur as usize] != u32::MAX {
                    break depth[cur as usize];
                }
                let p = self.entries[cur as usize].parent;
                if (p as usize) >= n || p == cur || p == ROOT_SENTINEL {
                    depth[cur as usize] = 0;
                    break 0;
                }
                stack.push(cur);
                cur = p;
            };
            // Assign increasing depths back down the walked chain (closest-to-base first).
            let mut d = base;
            while let Some(id) = stack.pop() {
                d += 1;
                depth[id as usize] = d;
            }
        }
        depth
    }
}
