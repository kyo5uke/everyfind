//! Live index mutation via `Index::apply_event`: create / delete / rename / move, the
//! directory-rename descendant-path proof, cumulative-reason idempotency (P10), coalesced
//! create+delete, orphan-on-create, and arena-garbage accounting.

use everyfind::index::{build_from_volume, Index};
use everyfind::volume::{reason, FakeEvent, FakeRecord, FakeVolume, UsnVolume};

const DRIVE: char = 'T';
/// `USN_REASON_DATA_EXTEND`, a non-name reason used to build realistic cumulative masks.
const DATA_EXTEND: u32 = 0x0000_0002;

/// Build an index from `records`, then drain `events` into it via `read_journal`
/// (exactly the path the M2 watch loop uses). Returns the mutated index.
fn build_then_apply(records: Vec<FakeRecord>, events: Vec<FakeEvent>) -> Index {
    let mut vol = FakeVolume::new(records).with_events(events);
    let mut index = build_from_volume(&mut vol, DRIVE).unwrap();
    let jid = vol.journal_info().unwrap().journal_id;
    vol.read_journal(0, jid, &mut |ev| index.apply_event(ev))
        .unwrap();
    index
}

/// Sorted list of reconstructed paths for a case-insensitive query (orphans excluded).
fn find(index: &Index, query: &str) -> Vec<String> {
    let mut paths: Vec<String> = index
        .search(query, true, false)
        .iter()
        .map(|&id| index.path(id))
        .collect();
    paths.sort();
    paths
}

/// A minimal tree: `T:\docs` (frn 10) with two files.
fn docs_tree() -> Vec<FakeRecord> {
    vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
        FakeRecord::file(20, 10, "a.txt"),
        FakeRecord::file(21, 10, "b.txt"),
    ]
}

#[test]
fn create_adds_a_searchable_entry() {
    let index = build_then_apply(
        docs_tree(),
        vec![FakeEvent::create(100, 30, 10, "new.txt").close()],
    );
    assert_eq!(find(&index, "new.txt"), vec![r"T:\docs\new.txt".to_owned()]);
}

#[test]
fn delete_tombstones_and_leaves_siblings_intact() {
    let index = build_then_apply(
        docs_tree(),
        vec![FakeEvent::delete(100, 20, 10, "a.txt").close()],
    );
    assert!(find(&index, "a.txt").is_empty(), "deleted file still found");
    assert_eq!(find(&index, "b.txt"), vec![r"T:\docs\b.txt".to_owned()]);
    assert_eq!(index.dead_count(), 1);
}

#[test]
fn rename_in_place_updates_the_name() {
    let index = build_then_apply(
        docs_tree(),
        vec![
            FakeEvent::rename_old(100, 20, 10, "a.txt"),
            FakeEvent::rename_new(101, 20, 10, "renamed.txt").close(),
        ],
    );
    assert!(find(&index, "a.txt").is_empty(), "old name still found");
    assert_eq!(
        find(&index, "renamed"),
        vec![r"T:\docs\renamed.txt".to_owned()]
    );
    assert!(index.garbage_ratio() > 0.0, "rename left no garbage");
}

#[test]
fn move_across_directories_reparents() {
    let mut records = docs_tree();
    records.push(FakeRecord::dir(11, 10, "sub")); // T:\docs\sub
    let index = build_then_apply(
        records,
        vec![
            FakeEvent::rename_old(100, 20, 10, "a.txt"),
            FakeEvent::rename_new(101, 20, 11, "a.txt").close(), // new parent = sub (frn 11)
        ],
    );
    assert_eq!(find(&index, "a.txt"), vec![r"T:\docs\sub\a.txt".to_owned()]);
}

#[test]
fn directory_rename_auto_follows_for_descendants() {
    // T:\docs\notes\todo.txt: rename `docs` -> `archive` and the descendant path must
    // update with no per-child events (the core M2 premise; P10 confirmed one dir record).
    let records = vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
        FakeRecord::dir(11, 10, "notes"),
        FakeRecord::file(20, 11, "todo.txt"),
    ];
    let index = build_then_apply(
        records,
        vec![
            FakeEvent::rename_old(100, 10, 5, "docs").dir(),
            FakeEvent::rename_new(101, 10, 5, "archive").dir().close(),
        ],
    );
    assert_eq!(
        find(&index, "todo"),
        vec![r"T:\archive\notes\todo.txt".to_owned()]
    );
    assert!(
        find(&index, "docs").is_empty(),
        "old directory name still reachable"
    );
}

#[test]
fn cumulative_reason_records_apply_once() {
    // One create surfaces across several OR-masked records until CLOSE (P10). The frn map
    // dedupes, so exactly one entry results.
    let index = build_then_apply(
        docs_tree(),
        vec![
            FakeEvent::create(100, 30, 10, "f.txt"),
            FakeEvent::create(101, 30, 10, "f.txt").also(DATA_EXTEND),
            FakeEvent::create(102, 30, 10, "f.txt")
                .also(DATA_EXTEND)
                .close(),
        ],
    );
    assert_eq!(find(&index, "f.txt"), vec![r"T:\docs\f.txt".to_owned()]);
    assert_eq!(index.search("f.txt", true, false).len(), 1);
}

#[test]
fn create_then_delete_in_one_record_is_a_no_op() {
    // FILE_CREATE|FILE_DELETE coalesced (created+deleted between reads) => net absent.
    let index = build_then_apply(
        docs_tree(),
        vec![FakeEvent::create(100, 30, 10, "tmp.txt")
            .also(reason::FILE_DELETE)
            .close()],
    );
    assert!(find(&index, "tmp.txt").is_empty());
}

#[test]
fn rename_old_name_alone_changes_nothing() {
    // A stray RENAME_OLD_NAME (pre-image) with no following NEW_NAME must not mutate.
    let index = build_then_apply(
        docs_tree(),
        vec![FakeEvent::rename_old(100, 20, 10, "a.txt")],
    );
    assert_eq!(find(&index, "a.txt"), vec![r"T:\docs\a.txt".to_owned()]);
    assert_eq!(index.garbage_ratio(), 0.0);
}

#[test]
fn create_under_unknown_parent_is_an_orphan() {
    let index = build_then_apply(
        vec![FakeRecord::dir(5, 5, "")],
        vec![FakeEvent::create(100, 30, 999, "lost.txt").close()], // parent 999 not indexed
    );
    assert!(
        find(&index, "lost").is_empty(),
        "orphan leaked into default search"
    );
    let with_orphans: Vec<String> = index
        .search("lost", true, true)
        .iter()
        .map(|&id| index.path(id))
        .collect();
    assert_eq!(with_orphans, vec![r"<orphan>\lost.txt".to_owned()]);
}
