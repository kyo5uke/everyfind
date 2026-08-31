//! Snapshot persistence: save->load round-trip produces an identical index and journal
//! cursor, live mutations survive the round-trip, and foreign/stale files are rejected.

mod common;

use common::{sample_volume, DRIVE};
use everyfind::index::{build_from_volume, Index};
use everyfind::snapshot;
use everyfind::volume::{FakeEvent, FakeRecord, FakeVolume, UsnVolume};

/// A unique scratch path on the (C:) temp volume; snapshot I/O is ordinary file I/O.
fn scratch(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ef_snap_{tag}_{}.bin", std::process::id()))
}

fn find(index: &Index, query: &str) -> Vec<String> {
    let mut v: Vec<String> = index
        .search(query, true, false)
        .iter()
        .map(|&id| index.path(id))
        .collect();
    v.sort();
    v
}

#[test]
fn save_load_roundtrip_preserves_the_index() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let path = scratch("rt");
    snapshot::save(&path, &idx, 0xDEAD_BEEF, 12_345).unwrap();
    let loaded = snapshot::load(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(loaded.journal_id, 0xDEAD_BEEF);
    assert_eq!(loaded.next_usn, 12_345);
    assert_eq!(loaded.index.len(), idx.len());
    for q in ["dll", "readme", "system32", "windows"] {
        assert_eq!(
            find(&idx, q),
            find(&loaded.index, q),
            "query {q:?} differs after round-trip"
        );
    }
}

#[test]
fn roundtrip_preserves_live_mutations() {
    // Build, apply a delete + a create, then snapshot/reload and verify the mutated state
    // survives, proving the frn map (with the deleted key removed) round-trips.
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
        FakeRecord::file(20, 10, "keep.txt"),
        FakeRecord::file(21, 10, "gone.txt"),
    ])
    .with_events(vec![
        FakeEvent::delete(100, 21, 10, "gone.txt").close(),
        FakeEvent::create(101, 30, 10, "added.txt").close(),
    ]);
    let mut idx = build_from_volume(&mut vol, 'T').unwrap();
    let jid = vol.journal_info().unwrap().journal_id;
    let next = vol
        .read_journal(0, jid, &mut |ev| idx.apply_event(ev))
        .unwrap();

    let path = scratch("mut");
    snapshot::save(&path, &idx, jid, next).unwrap();
    let loaded = snapshot::load(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(loaded.next_usn, next);
    assert!(
        find(&loaded.index, "gone").is_empty(),
        "deleted file survived"
    );
    assert_eq!(
        find(&loaded.index, "added"),
        vec![r"T:\docs\added.txt".to_owned()]
    );
    assert_eq!(
        find(&loaded.index, "keep"),
        vec![r"T:\docs\keep.txt".to_owned()]
    );

    // The reloaded index is still mutable and its map is intact: a follow-up delete works.
    let mut reloaded = loaded.index;
    let vol2 = FakeVolume::new(vec![])
        .with_events(vec![FakeEvent::delete(200, 20, 10, "keep.txt").close()]);
    let _ = vol2.read_journal(0, 1, &mut |ev| reloaded.apply_event(ev));
    assert!(
        find(&reloaded, "keep").is_empty(),
        "post-reload delete failed"
    );
}

#[test]
fn load_rejects_a_foreign_file() {
    let path = scratch("bad");
    std::fs::write(&path, b"definitely not an Everyfind snapshot").unwrap();
    let r = snapshot::load(&path);
    let _ = std::fs::remove_file(&path);
    assert!(r.is_err(), "foreign file was accepted as a snapshot");
}
