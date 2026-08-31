//! Watch-loop recovery: normal catch-up, plus the discontinuity paths (journal-id change,
//! trimmed cursor) and the arena-garbage rebuild, all of which fall back to a full
//! re-enumeration. Driven through `Watched` on a `FakeVolume` whose journal metadata is
//! mutated mid-test with `set_journal`.
//!
//! `sync` *reports* a discontinuity and `recover` acts on it, so each of these tests names the
//! two separately. That split is the whole point of the design: the daemon runs the reporting
//! half under its write lock and the acting half beside it.

use everyfind::index::Index;
use everyfind::volume::{FakeEvent, FakeRecord, FakeVolume};
use everyfind::watch::{Synced, Watched};

fn tree() -> Vec<FakeRecord> {
    vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
        FakeRecord::file(20, 10, "a.txt"),
    ]
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
fn sync_applies_new_events_and_advances_the_cursor() {
    let mut vol =
        FakeVolume::new(tree())
            .with_events(vec![FakeEvent::create(100, 30, 10, "added.txt").close()]);
    vol.set_journal(1, 0, 0); // start the cursor before the events

    let mut state = Watched::enumerate(&mut vol, 'T').unwrap();
    let applied = applied(state.sync(&mut vol).unwrap());

    assert_eq!(applied, 1);
    assert_eq!(
        find(&state.index, "added"),
        vec![r"T:\docs\added.txt".to_owned()]
    );
    assert_eq!(state.next_usn, 101);
}

#[test]
fn journal_id_change_triggers_reenumeration() {
    let mut vol =
        FakeVolume::new(tree())
            .with_events(vec![FakeEvent::create(100, 30, 10, "ghost.txt").close()]);
    vol.set_journal(1, 0, 0);
    let mut state = Watched::enumerate(&mut vol, 'T').unwrap();

    // Journal deleted+recreated: new id, fresh cursor.
    vol.set_journal(2, 0, 999);
    let why = discontinuity(state.sync(&mut vol).unwrap());
    assert!(why.contains("recreated"), "unexpected reason: {why}");
    // Still the old index: nothing has been thrown away yet. The daemon serves searches from
    // exactly this state for as long as the rebuild takes.
    assert_eq!(state.journal_id, 1);
    state.recover(&mut vol).unwrap();

    assert_eq!(state.journal_id, 2);
    assert_eq!(state.next_usn, 999);
    // Rebuilt from the enum snapshot: the un-applied journal event is not present.
    assert!(find(&state.index, "ghost").is_empty());
    assert_eq!(
        find(&state.index, "a.txt"),
        vec![r"T:\docs\a.txt".to_owned()]
    );
}

#[test]
fn trimmed_cursor_triggers_reenumeration() {
    let mut vol =
        FakeVolume::new(tree())
            .with_events(vec![FakeEvent::create(100, 30, 10, "ghost.txt").close()]);
    vol.set_journal(1, 0, 0);
    let mut state = Watched::enumerate(&mut vol, 'T').unwrap();

    // Same journal id, but everything below usn 500 was trimmed (wrap past our cursor).
    vol.set_journal(1, 500, 500);
    let why = discontinuity(state.sync(&mut vol).unwrap());
    assert!(why.contains("trimmed"), "unexpected reason: {why}");
    state.recover(&mut vol).unwrap();

    assert_eq!(
        state.next_usn, 500,
        "cursor reset to the fresh journal tail"
    );
    assert!(find(&state.index, "ghost").is_empty());
    assert_eq!(
        find(&state.index, "a.txt"),
        vec![r"T:\docs\a.txt".to_owned()]
    );
}

#[test]
fn garbage_over_threshold_rebuilds() {
    // A rename leaves the old name as garbage; a tiny threshold forces a rebuild.
    let mut vol = FakeVolume::new(tree()).with_events(vec![
        FakeEvent::rename_old(100, 20, 10, "a.txt"),
        FakeEvent::rename_new(101, 20, 10, "renamed_to_something_much_longer.txt").close(),
    ]);
    vol.set_journal(1, 0, 0);
    let mut state = Watched::enumerate(&mut vol, 'T').unwrap();
    applied(state.sync(&mut vol).unwrap());
    assert!(state.index.garbage_ratio() > 0.0, "rename left no garbage");

    assert!(
        state.compaction_due(0.000_1),
        "garbage over threshold went unnoticed"
    );
    state.recover(&mut vol).unwrap();
    assert_eq!(state.index.garbage_ratio(), 0.0, "rebuild left garbage");
    assert_eq!(state.index.dead_count(), 0);
    assert!(
        !state.compaction_due(0.000_1),
        "a rebuilt index still asks to be rebuilt"
    );
}

/// A rebuilt index can be handed to a `Watched` that never saw the volume, which is how the
/// daemon adopts one built beside the live index, off the lock.
#[test]
fn a_rebuild_can_be_built_apart_and_adopted() {
    let mut vol = FakeVolume::new(tree());
    vol.set_journal(7, 0, 42);
    let mut state = Watched::placeholder('T');
    assert!(
        find(&state.index, "a.txt").is_empty(),
        "placeholder not empty"
    );

    let rebuilt = Watched::rebuild(&mut vol, 'T').unwrap();
    state.adopt(rebuilt);

    assert_eq!(state.journal_id, 7);
    assert_eq!(state.next_usn, 42);
    assert_eq!(
        find(&state.index, "a.txt"),
        vec![r"T:\docs\a.txt".to_owned()]
    );
}

/// `Synced::Applied` count, or a panic naming what came back instead.
fn applied(s: Synced) -> usize {
    match s {
        Synced::Applied(a) => a.events,
        Synced::Discontinuity(why) => panic!("expected events, got a discontinuity: {why}"),
    }
}

/// The reason a `Synced::Discontinuity` gave, or a panic naming what came back instead.
fn discontinuity(s: Synced) -> &'static str {
    match s {
        Synced::Applied(a) => panic!("expected a discontinuity, got {} applied events", a.events),
        Synced::Discontinuity(why) => why,
    }
}
