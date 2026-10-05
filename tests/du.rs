//! M5 `ef du` aggregation correctness (pure logic on `FakeVolume`):
//! subtree totals, path resolution, **hardlink once-count**, **reparse-not-followed**,
//! u32-cluster saturation, and fail-soft (unresolved-size) accounting.
//!
//! Hardlink-once and reparse-not-followed are the core of method A's correctness (the reasons A
//! is right where the per-name method B is wrong); pinned here as regressions.

use everyfind::index::build_from_volume;
use everyfind::volume::{reason, FakeEvent, FakeRecord, FakeVolume, UsnVolume};
use everyfind::watch::{Synced, Watched, MAX_SIZE_STATS_PER_POLL};

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

/// A path on another drive is not in this index, whatever follows the drive letter.
///
/// The letter used to be dropped unread, so `D:\` resolved to this volume's root and
/// `ef du D:\` printed C:'s usage under D:'s name: a confident answer about a drive nobody
/// had looked at.
#[test]
fn a_path_on_another_drive_does_not_resolve() {
    let mut vol = du_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let alice = idx.resolve_path("C:\\Users\\alice").unwrap();

    assert_eq!(idx.resolve_path("D:\\"), None);
    assert_eq!(idx.resolve_path("D:"), None);
    assert_eq!(idx.resolve_path("d:\\Users\\alice"), None);
    // The indexed drive's own letter, in either case, and no letter at all, still do.
    assert_eq!(idx.resolve_path("c:\\Users\\alice"), Some(alice));
    assert_eq!(idx.resolve_path("\\Users\\alice"), Some(alice));
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

/// **Fail-soft.** A file the size pass skips stays at 0 and is *visible* via `unsized_files`
/// (surfaced by `ef status` and the `ef du` warning), never a silent miss.
#[test]
fn unresolved_size_is_zero_and_counted() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "sized.txt").size(4),
        FakeRecord::file(31, 5, "unsized.txt"), // no size -> skipped by the pass
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let root = idx.resolve_path("C:\\").unwrap();

    assert_eq!(idx.unsized_files(), 1); // unsized.txt
    assert_eq!(idx.sizes_resolved(), 2); // the root and sized.txt, of 3 live entries
    assert_eq!(idx.du(root, 1, 10).total_clusters, 4); // unsized contributes 0
}

/// The count of files without a size follows the index through every kind of change, so it
/// says how things stand now rather than how they stood when the index was built.
#[test]
fn the_unsized_count_follows_creates_renames_stats_and_deletes() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
        FakeRecord::file(30, 10, "a.bin").size(1),
        FakeRecord::file(31, 10, "b.bin"), // the size pass misses this one
    ]);
    let mut idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let apply = |idx: &mut everyfind::index::Index, ev: FakeEvent| {
        let journal = FakeVolume::new(vec![]).with_events(vec![ev]);
        journal
            .read_journal(0, 1, &mut |e| idx.apply_event(e))
            .unwrap();
    };
    assert_eq!(idx.unsized_files(), 1, "b.bin");

    // A rename moves a name, not a measurement.
    apply(
        &mut idx,
        FakeEvent::rename_new(100, 30, 10, "a-renamed.bin").close(),
    );
    assert_eq!(idx.unsized_files(), 1, "renaming a sized file");

    // A directory has no allocation of its own to read, and is never stat'ed.
    apply(
        &mut idx,
        FakeEvent::create(101, 40, 10, "sub").dir().close(),
    );
    assert_eq!(idx.unsized_files(), 1, "a new directory");

    // A new file is missing its size until a stat answers for it...
    apply(&mut idx, FakeEvent::create(102, 41, 10, "c.bin").close());
    assert_eq!(idx.unsized_files(), 2, "a new file, not yet stat'ed");
    idx.update_size(41, 3, false);
    assert_eq!(idx.unsized_files(), 1, "...and not after");
    idx.update_size(41, 5, false);
    assert_eq!(idx.unsized_files(), 1, "a second stat of the same file");

    // The running count against a count from scratch: a snapshot stores the flags and not
    // the number, so what comes back from one is the flags counted afresh.
    let recounted = |idx: &everyfind::index::Index| {
        let path = std::env::temp_dir().join(format!("ef-recount-{}.bin", std::process::id()));
        everyfind::snapshot::save(&path, idx, 1, 0).unwrap();
        let loaded = everyfind::snapshot::load(&path).unwrap().index;
        let _ = std::fs::remove_file(&path);
        loaded.unsized_files()
    };
    assert_eq!(recounted(&idx), 1, "the running count has not drifted");

    // A file that goes unmeasured is no longer a missing size.
    apply(&mut idx, FakeEvent::delete(103, 31, 10, "b.bin").close());
    assert_eq!(idx.unsized_files(), 0, "the unsized file was deleted");
    assert_eq!(idx.sizes_resolved(), idx.live_len());
    assert_eq!(recounted(&idx), 0, "the running count has not drifted");
}

/// A snapshot carries which entries were measured, not just the numbers: after a restart the
/// index knows what it knew before, and a file the size pass missed is still counted as
/// missing rather than passing for a file of zero bytes.
#[test]
fn a_snapshot_round_trip_keeps_which_files_are_unsized() {
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "sized.bin").size(4),
        FakeRecord::file(31, 5, "empty-but-measured.bin").size(0),
        FakeRecord::file(32, 5, "unsized.bin"),
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    assert_eq!(idx.unsized_files(), 1, "precondition");

    let path = std::env::temp_dir().join(format!("ef-sized-flags-{}.bin", std::process::id()));
    everyfind::snapshot::save(&path, &idx, 1, 0).unwrap();
    let loaded = everyfind::snapshot::load(&path).unwrap().index;
    let _ = std::fs::remove_file(&path);

    assert_eq!(loaded.unsized_files(), 1);
    assert_eq!(loaded.sizes_resolved(), idx.sizes_resolved());
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

/// **A burst larger than one poll's stat budget.** A bulk copy changes far more files in one
/// drain than a single poll is allowed to stat. The surplus has to wait its turn and be
/// measured by the polls that follow: the journal never mentions those files again, so a size
/// that is skipped once is a size that stays wrong. Found live: four `robocopy` runs side by
/// side left a third of the 370,000 files they copied at 0 bytes, and `ef du` 275 GiB short
/// of what the volume actually held.
#[test]
fn a_burst_over_the_poll_budget_is_sized_by_later_polls() {
    // Comfortably past one poll's budget, and not a multiple of it.
    const FILES: u64 = 10_000;
    let events: Vec<FakeEvent> = (0..FILES)
        .map(|i| FakeEvent::create(100 + i, 1_000 + i, 10, &format!("f{i}.bin")).close())
        .collect();
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "copy"),
    ])
    .with_events(events)
    .with_live_sizes((0..FILES).map(|i| (1_000 + i, (1u32, false))));
    vol.set_journal(1, 0, 0);

    let mut w = Watched::enumerate(&mut vol, DRIVE).unwrap();
    let root = w.index.resolve_path("C:\\").unwrap();

    // The first poll drains every event; the ones after it find the journal quiet and have
    // only the backlog to work through.
    let mut stats_per_poll = Vec::new();
    for _ in 0..4 {
        let Synced::Applied(a) = w.sync(&mut vol).unwrap() else {
            panic!("the journal did not apply");
        };
        stats_per_poll.push(a.resized.len());
        w.refresh_sizes(&vol, &a.resized);
    }

    assert!(
        stats_per_poll.iter().all(|&n| n <= MAX_SIZE_STATS_PER_POLL),
        "a poll went over its stat budget: {stats_per_poll:?}"
    );
    assert_eq!(
        stats_per_poll.iter().sum::<usize>(),
        FILES as usize,
        "every file is stat'ed, and only once: {stats_per_poll:?}"
    );
    assert_eq!(
        w.index.du(root, 1, 10).total_clusters,
        FILES,
        "files past the first poll's budget were never sized"
    );
    assert_eq!(w.sizes_pending(), 0, "nothing is left waiting");
}

/// What becomes of a file between the poll that queues it and the poll that stats it.
///
/// Written to again: still stat'ed once, and with the allocation it has by then (it is the
/// stat that is late, not the number). Deleted: dropped, there being nothing left to measure.
#[test]
fn a_waiting_file_is_stat_ed_once_and_not_at_all_once_deleted() {
    const FILES: u64 = 5_000; // 904 past one poll's budget
    let records = vec![FakeRecord::dir(5, 5, ""), FakeRecord::dir(10, 5, "copy")];
    let burst: Vec<FakeEvent> = (0..FILES)
        .map(|i| FakeEvent::create(100 + i, 1_000 + i, 10, &format!("f{i}.bin")).close())
        .collect();
    // The last two of the burst, so both are still waiting when the first poll is done.
    let (grown, gone) = (1_000 + FILES - 1, 1_000 + FILES - 2);

    let mut vol = FakeVolume::new(records.clone())
        .with_events(burst.clone())
        .with_live_sizes((0..FILES).map(|i| (1_000 + i, (1u32, false))));
    vol.set_journal(1, 0, 0);
    let mut w = Watched::enumerate(&mut vol, DRIVE).unwrap();
    let Synced::Applied(first) = w.sync(&mut vol).unwrap() else {
        panic!("the burst did not apply");
    };
    w.refresh_sizes(&vol, &first.resized);
    assert!(
        !first
            .resized
            .iter()
            .any(|(frn, _)| *frn == grown || *frn == gone),
        "precondition: both files are past the first poll's budget"
    );

    // The same journal a moment later: one waiting file grew, the other was deleted.
    let mut later = burst;
    later.push(
        FakeEvent::create(20_000, grown, 10, "f4999.bin")
            .also(reason::DATA_EXTEND)
            .close(),
    );
    later.push(FakeEvent::delete(20_001, gone, 10, "f4998.bin").close());
    let mut vol = FakeVolume::new(records)
        .with_events(later)
        .with_live_sizes((0..FILES).map(|i| {
            let frn = 1_000 + i;
            (frn, (if frn == grown { 9u32 } else { 1 }, false))
        }));
    let Synced::Applied(second) = w.sync(&mut vol).unwrap() else {
        panic!("the later events did not apply");
    };
    w.refresh_sizes(&vol, &second.resized);

    assert_eq!(
        second
            .resized
            .iter()
            .filter(|(frn, _)| *frn == grown)
            .count(),
        1,
        "a file touched again while waiting is stat'ed once"
    );
    assert!(
        !second.resized.iter().any(|(frn, _)| *frn == gone),
        "a file deleted while waiting is not stat'ed"
    );
    // 4,998 files of one cluster each, and the one that grew to nine.
    let root = w.index.resolve_path("C:\\").unwrap();
    assert_eq!(w.index.du(root, 1, 10).total_clusters, FILES - 2 + 9);
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
        state.index.unsized_files(),
        1,
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

    assert_eq!(
        resumed.index.unsized_files(),
        0,
        "the size pass did not re-run on resume"
    );
    let root = resumed.index.resolve_path("C:\\").unwrap();
    assert_eq!(resumed.index.du(root, 1, 10).total_clusters, 7);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other side of that rule, which is the side the `ef du` warning has to describe: a
/// snapshot missing only some of its sizes is resumed as it is. A restart does not re-read
/// them, and the warning used to say that it did.
#[test]
fn a_resumed_snapshot_missing_a_minority_of_sizes_is_left_as_it_is() {
    let dir = std::env::temp_dir().join(format!("ef-size-minority-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("index.snapshot");

    let mut partial = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "a.bin").size(1),
        FakeRecord::file(31, 5, "b.bin").size(1),
        FakeRecord::file(32, 5, "missed.bin"),
    ]);
    partial.set_journal(1, 0, 0);
    let state = Watched::enumerate(&mut partial, DRIVE).unwrap();
    assert_eq!(state.index.unsized_files(), 1, "precondition: one of three");
    state.save(&path).unwrap();

    // The volume could answer for all three now, but nobody asks it.
    let mut whole = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(30, 5, "a.bin").size(1),
        FakeRecord::file(31, 5, "b.bin").size(1),
        FakeRecord::file(32, 5, "missed.bin").size(50),
    ]);
    whole.set_journal(1, 0, 0);
    let loaded = everyfind::snapshot::load(&path).unwrap();
    let resumed = Watched::from_snapshot(loaded, &mut whole, DRIVE).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        resumed.index.unsized_files(),
        1,
        "a resume re-reads sizes only when more than half of the files lack one"
    );
    let root = resumed.index.resolve_path("C:\\").unwrap();
    assert_eq!(resumed.index.du(root, 1, 10).total_clusters, 2);
}

/// A resumed index answers to the letter its volume has now, which need not be the letter it
/// had when the snapshot was saved. Paths are printed with that letter and resolved only if
/// they name it, so an index that kept the old one would show one drive and answer to another.
#[test]
fn a_resumed_snapshot_takes_the_drive_letter_the_volume_has_now() {
    let dir = std::env::temp_dir().join(format!("ef-drive-letter-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("index.snapshot");

    let mut vol = du_volume();
    vol.set_journal(1, 0, 0);
    Watched::enumerate(&mut vol, 'C')
        .unwrap()
        .save(&path)
        .unwrap();

    let loaded = everyfind::snapshot::load(&path).unwrap();
    let resumed = Watched::from_snapshot(loaded, &mut vol, 'E').unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let alice = resumed
        .index
        .resolve_path(r"E:\Users\alice")
        .expect("the current letter resolves");
    assert_eq!(resumed.index.path(alice), r"E:\Users\alice");
    assert_eq!(resumed.index.resolve_path(r"C:\Users\alice"), None);
}
