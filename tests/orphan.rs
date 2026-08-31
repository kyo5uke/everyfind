//! Orphan handling and root synthesis when the MFT stream omits FRN 5.

mod common;

use everyfind::index::{build_from_volume, flags};
use everyfind::volume::{FakeRecord, FakeVolume};

const DRIVE: char = 'C';

#[test]
fn root_is_synthesized_when_frn5_is_absent() {
    // No FRN-5 record; a top-level file references parent FRN 5.
    let mut vol = FakeVolume::new(vec![FakeRecord::file(100, 5, "topfile.txt")]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // The file must resolve under the synthesized root, NOT be treated as an orphan.
    assert_eq!(idx.path(0), r"C:\topfile.txt");
    assert_eq!(idx.entries()[0].flags & flags::IS_ORPHAN, 0);
    assert_eq!(idx.path(idx.root()), r"C:\");
}

#[test]
fn direct_orphan_renders_with_orphan_prefix() {
    let mut vol = FakeVolume::new(vec![FakeRecord::file(20, 999, "lonely.txt")]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(idx.path(0), r"<orphan>\lonely.txt");
    assert_ne!(idx.entries()[0].flags & flags::IS_ORPHAN, 0);
}

#[test]
fn children_of_an_orphan_directory_are_also_orphaned() {
    // ghostdir's parent (888) is missing; child.txt's parent (ghostdir) resolves.
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(200, 888, "ghostdir"),
        FakeRecord::file(201, 200, "child.txt"),
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Path renders entirely under <orphan>, never a fake C:\ path.
    assert_eq!(idx.path(1), r"<orphan>\ghostdir\child.txt");

    // Both the orphan dir and its child are flagged, so both are excluded from search
    // by default (their paths do not reach a real location).
    assert_ne!(idx.entries()[0].flags & flags::IS_ORPHAN, 0);
    assert_ne!(idx.entries()[1].flags & flags::IS_ORPHAN, 0);
    assert!(idx.search("child", true, false).is_empty());
    assert_eq!(idx.search("child", true, true), vec![1]);
}

/// A directory that stops being an orphan takes its subtree with it.
///
/// `upsert` gives a new entry its parent's orphan flag, which is exact while a tree is only
/// being built (parents arrive before children) and wrong as soon as one is **moved**. The
/// directory that moved loses the flag; everything already indexed underneath keeps it, and
/// stays out of every search that does not ask for orphans. Nothing reports this: the files
/// are in the index, they simply never come back.
#[test]
fn a_subtree_follows_its_directory_out_of_orphanhood() {
    use everyfind::volume::{FakeEvent, FakeVolume};
    use everyfind::watch::{Synced, Watched};

    // `ghostdir` names a parent (FRN 900) that is not in the MFT stream, so it and its child
    // are both orphans.
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 900, "ghostdir"),
        FakeRecord::file(20, 10, "buried.txt"),
    ]);
    vol.set_journal(1, 0, 0);
    let mut state = Watched::enumerate(&mut vol, DRIVE).unwrap();
    let orphaned = |idx: &everyfind::index::Index, needle: &str| -> bool {
        idx.search(needle, true, true)
            .iter()
            .all(|&id| idx.entries()[id as usize].flags & flags::IS_ORPHAN != 0)
    };
    assert!(orphaned(&state.index, "buried"), "precondition: orphaned");
    assert!(
        state.index.search("buried", true, false).is_empty(),
        "precondition: an orphan is out of an ordinary search"
    );

    // `ghostdir` is moved under the root. Only its own record is re-stated, which is exactly
    // what the journal delivers.
    let mut vol = vol.with_events(vec![FakeEvent::rename_new(100, 10, 5, "ghostdir")
        .dir()
        .close()]);
    vol.set_journal(1, 0, 0);
    let Synced::Applied(a) = state.sync(&mut vol).unwrap() else {
        panic!("the move was not applied");
    };
    assert_eq!(a.events, 1);
    if state.index.orphans_stale() {
        state.index.refresh_orphans();
    }

    let found = state.index.search("buried", true, true);
    assert_eq!(found.len(), 1);
    assert_eq!(
        state.index.path(found[0]),
        r"C:\ghostdir\buried.txt",
        "the child should now render under the root"
    );
    assert!(
        !state.index.search("buried", true, false).is_empty(),
        "the child is still flagged an orphan, so it never comes back from a search"
    );
}
