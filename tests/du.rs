//! M5 `ef du` aggregation correctness (pure logic on `FakeVolume`):
//! subtree totals, path resolution, **hardlink once-count**, **reparse-not-followed**,
//! u32-cluster saturation, and fail-soft (unresolved-size) accounting.
//!
//! Hardlink-once and reparse-not-followed are the core of method A's correctness (the reasons A
//! is right where the per-name method B is wrong); pinned here as regressions.

use everyfind::index::build_from_volume;
use everyfind::volume::{FakeEvent, FakeRecord, FakeVolume};
use everyfind::watch::{Synced, Watched};

const DRIVE: char = 'C';

/// ```text
/// id frn parent name        kind         alloc(clusters)
///  0   5    5   ""          root
///  1  10    5   Users       dir
///  2  20   10   alice       dir
///  3  30   20   a.txt       file            3
///  4  31   20   b.txt       file            5
///  5  21   10   bob         dir
///  6  32   21   c.txt       file            2
///  7  11    5   Windows     dir
///  8  40   11   k.dll       file           10
/// ```
fn du_volume() -> FakeVolume {
    FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "Users"),
        FakeRecord::dir(20, 10, "alice"),
        FakeRecord::file(30, 20, "a.txt").size(3),
        FakeRecord::file(31, 20, "b.txt").size(5),
        FakeRecord::dir(21, 10, "bob"),
        FakeRecord::file(32, 21, "c.txt").size(2),
        FakeRecord::dir(11, 5, "Windows"),
        FakeRecord::file(40, 11, "k.dll").size(10),
    ])
}

#[test]
fn root_total_and_children_sorted_by_size() {
    let mut vol = du_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();

    let rep = idx.du(root, 1, 10);
    assert_eq!(rep.total_clusters, 3 + 5 + 2 + 10); // 20
    assert_eq!(rep.cluster_bytes, 4096);
    assert!(!rep.truncated);

    // Immediate children: Users (10) and Windows (10); tie broken by name -> Users first.
    let names: Vec<(&str, u64)> = rep
        .rows
        .iter()
        .map(|r| (idx.name(r.id), r.clusters))
        .collect();
    assert_eq!(names, vec![("Users", 10), ("Windows", 10)]);
}

#[test]
fn subtree_total_is_recursive() {
    let mut vol = du_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let users = idx.resolve_path("C:\\Users").unwrap();
    let rep = idx.du(users, 1, 10);
    assert_eq!(rep.total_clusters, 10); // alice(8) + bob(2)
    let names: Vec<(&str, u64)> = rep
        .rows
        .iter()
        .map(|r| (idx.name(r.id), r.clusters))
        .collect();
    assert_eq!(names, vec![("alice", 8), ("bob", 2)]);

    // Deepest dir: children are files, largest first.
    let alice = idx.resolve_path("C:\\Users\\alice").unwrap();
    let rep = idx.du(alice, 1, 10);
    assert_eq!(rep.total_clusters, 8);
    let names: Vec<(&str, u64)> = rep
        .rows
        .iter()
        .map(|r| (idx.name(r.id), r.clusters))
        .collect();
    assert_eq!(names, vec![("b.txt", 5), ("a.txt", 3)]);
}

#[test]
fn resolve_path_is_case_insensitive_and_none_for_missing() {
    let mut vol = du_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let alice = idx.resolve_path("C:\\Users\\alice").unwrap();
    assert_eq!(idx.resolve_path("c:\\users\\ALICE"), Some(alice));
    assert_eq!(idx.resolve_path("C:\\"), Some(idx.root()));
    assert_eq!(idx.resolve_path("C:\\Users\\nope"), None);
}

#[test]
fn depth_limit_includes_descendants() {
    let mut vol = du_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();

    // depth 2 under root reaches alice/bob (grandchildren) as rows too.
    let rep = idx.du(root, 2, 20);
    let names: Vec<&str> = rep.rows.iter().map(|r| idx.name(r.id)).collect();
    assert!(names.contains(&"alice"));
    assert!(names.contains(&"bob"));
    // depth 1 does not.
    let rep1 = idx.du(root, 1, 20);
    let names1: Vec<&str> = rep1.rows.iter().map(|r| idx.name(r.id)).collect();
    assert!(!names1.contains(&"alice"));
}

/// **Hardlink once-count.** A file with one FRN under one name is counted once. The index keys by
/// FRN, so even if a backend defensively yields the shared FRN twice (two names), its size is
/// attributed once, never doubled. (Method B / per-name counting is what gets this wrong.)
#[test]
fn hardlink_is_counted_once() {
    // frn 50 appears under two names (a hardlink pair); the index dedups by FRN.
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "d1"),
        FakeRecord::file(50, 10, "primary.txt").size(5),
        FakeRecord::dir(11, 5, "d2"),
        FakeRecord::file(50, 11, "alt.txt").size(5),
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();

    // The 5-cluster file is counted ONCE across the whole volume, not 10.
    assert_eq!(idx.du(root, 1, 10).total_clusters, 5);
}

/// **Reparse-not-followed.** A junction/symlink dir has no children in the MFT parent structure
/// (the target's children have the target as their real parent), so aggregation never doubles the
/// target's bytes under the junction.
#[test]
fn reparse_point_is_not_followed() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "realdir"),
        FakeRecord::file(30, 10, "big.bin").size(100),
        FakeRecord::dir(11, 5, "junction").reparse(), // reparse dir, NO indexed children
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();
    let junction = idx.resolve_path("C:\\junction").unwrap();

    // Root counts big.bin once (100), not twice via the junction.
    assert_eq!(idx.du(root, 1, 10).total_clusters, 100);
    // The junction's own subtree is empty (its target is aggregated at its real location).
    let jrep = idx.du(junction, 1, 10);
    assert_eq!(jrep.total_clusters, 0);
    assert!(jrep.rows.is_empty());
}

/// **u32 saturation.** A > 16 TiB file saturates and marks the subtree truncated; the total is a
/// lower bound, never a silent under-count.
#[test]
fn oversize_file_saturates_and_marks_truncated() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "huge.bin").size_truncated(u32::MAX),
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();

    let rep = idx.du(root, 1, 10);
    assert_eq!(rep.total_clusters, u32::MAX as u64);
    assert!(rep.truncated);
    assert!(rep.rows[0].truncated);
}

/// **Fail-soft.** A file the size pass skips stays at 0 and is *visible* via `sizes_resolved`
/// (surfaced by `ef status`), never a silent miss.
#[test]
fn unresolved_size_is_zero_and_counted() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "sized.txt").size(4),
        FakeRecord::file(31, 5, "unsized.txt"), // no size -> skipped by the pass
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();

    assert_eq!(idx.sizes_resolved(), 1); // only sized.txt resolved
    assert_eq!(idx.du(root, 1, 10).total_clusters, 4); // unsized contributes 0
}

/// **Live size refresh.** A file created after the build gets its size from a single-file
/// stat in the poll that applies the create, so `ef du` reflects it within one poll; USN
/// events carry no size.
///
/// `sync` names the files to stat and `refresh_sizes` stats them, because the stats are
/// filesystem round trips and the apply is not: the daemon runs the first under its write lock
/// and the second beside it.
#[test]
fn live_created_file_is_sized_on_next_sync() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
    ])
    .with_events(vec![FakeEvent::create(100, 30, 10, "new.bin").close()])
    .with_live_sizes([(30u64, (7u32, false))]);
    // Reset the cursor to 0 so the create event is drained by sync (with_events set it to the tail).
    vol.set_journal(1, 0, 0);

    let mut w = Watched::enumerate(&mut vol, DRIVE).unwrap();
    let root = w.index.resolve_path("C:\\").unwrap();
    assert_eq!(w.index.du(root, 1, 10).total_clusters, 0); // nothing sized yet

    // Applies create(30), and names it as a file whose allocation may have moved.
    let Synced::Applied(a) = w.sync(&mut vol).unwrap() else {
        panic!("the create was not applied");
    };
    assert_eq!(
        a.resized,
        vec![(30u64, format!("{DRIVE}:\\docs\\new.bin"))],
        "sync did not hand back the created file to be stat'ed"
    );
    assert_eq!(
        w.index.du(root, 1, 10).total_clusters,
        0,
        "the apply must not have stat'ed anything by itself"
    );

    w.refresh_sizes(&vol, &a.resized); // 7 clusters, learned by stat
    assert_eq!(w.index.du(root, 1, 10).total_clusters, 7);
    let docs = w.index.resolve_path("C:\\docs").unwrap();
    assert_eq!(w.index.du(docs, 1, 10).total_clusters, 7);
}

/// A snapshot that carries no sizes must not be trusted over the volume it resumes against.
///
/// The size pass runs only during a fresh enumeration, so a snapshot saved after a failed
/// pass used to pin `du` at zero forever: every restart resumed the zeros and nothing ever
/// asked the volume again. Found live: three service restarts in a row reporting
/// `0 / 6.2M resolved` while the actual size-pass fix sat unexercised.
#[test]
fn a_resumed_snapshot_without_sizes_reruns_the_size_pass() {
    let dir = std::env::temp_dir().join(format!("ef-size-backfill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("index.snapshot");

    // Build against a volume whose size pass yields nothing (no record carries a size).
    let mut bare = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "a.bin"),
    ]);
    bare.set_journal(1, 0, 0);
    let state = Watched::enumerate(&mut bare, DRIVE).unwrap();
    assert_eq!(
        state.index.sizes_resolved(),
        0,
        "precondition: an unsized build"
    );
    state.save(&path).unwrap();

    // Resume against the same volume, now able to answer sizes; the live analogue is the
    // size-pass bug being fixed between the save and the restart.
    let mut sized = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "a.bin").size(7),
    ]);
    sized.set_journal(1, 0, 0);
    let loaded = everyfind::snapshot::load(&path).unwrap();
    let resumed = Watched::from_snapshot(loaded, &mut sized, DRIVE).unwrap();

    assert!(
        resumed.index.sizes_resolved() > 0,
        "the size pass did not re-run on resume"
    );
    let root = resumed.index.resolve_path("C:\\").unwrap();
    assert_eq!(resumed.index.du(root, 1, 10).total_clusters, 7);
    let _ = std::fs::remove_dir_all(&dir);
}
