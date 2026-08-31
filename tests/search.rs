//! Search correctness: substring matching, case sensitivity, orphan exclusion,
//! and the M6 contig hit-map lifecycle (build -> mutate -> rebuild).

mod common;

use common::{sample_volume, sorted, DRIVE};
use everyfind::index::build_from_volume;
use everyfind::volume::{FakeEvent, FakeRecord, FakeVolume, UsnVolume};

#[test]
fn substring_match_finds_the_file() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(idx.search("kernel32", true, false), vec![common::KERNEL32]);
}

#[test]
fn substring_matches_across_multiple_entries() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Both *.dll files contain "dll".
    assert_eq!(
        sorted(idx.search("dll", true, false)),
        vec![common::KERNEL32, common::K32LINK]
    );
}

#[test]
fn case_insensitive_is_the_default_and_ignores_case() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Upper-cased query still finds the lower-cased name.
    assert_eq!(idx.search("KERNEL32", true, false), vec![common::KERNEL32]);

    // "readme" (any case) matches both readme.txt and README.MD.
    assert_eq!(
        sorted(idx.search("ReAdMe", true, false)),
        vec![common::README_TXT, common::README_MD]
    );
}

#[test]
fn case_sensitive_search_respects_case() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Lower-case "readme" matches only readme.txt, not README.MD.
    assert_eq!(idx.search("readme", false, false), vec![common::README_TXT]);
    // Upper-case "README" matches only README.MD.
    assert_eq!(idx.search("README", false, false), vec![common::README_MD]);
}

#[test]
fn orphans_are_excluded_by_default_and_opt_in_with_include_orphans() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert!(idx.search("orphan", true, false).is_empty());
    assert_eq!(idx.search("orphan", true, true), vec![common::ORPHAN]);
}

#[test]
fn root_is_never_returned() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // The empty-named root must not match an empty-ish query via any path.
    assert!(!idx.search("", true, true).contains(&idx.root()));
}

#[test]
fn no_match_yields_empty() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert!(idx.search("no-such-file", true, true).is_empty());
}

#[test]
fn query_is_a_single_literal_substring_not_and_of_tokens() {
    // **Engine match semantics, pinned for the M4 highlight.** The
    // search is ONE case-insensitive `memmem` over the query, i.e. a single *literal*
    // substring, NOT whitespace tokenization / AND-of-terms. The M4 TUI highlight therefore
    // marks occurrences of the whole query string, and this test guarantees display and engine
    // can never drift into different meanings.
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // The literal (dots and all) is a substring of kernel32.dll only.
    assert_eq!(idx.search("32.dll", true, false), vec![common::KERNEL32]);
    assert_eq!(
        idx.search("kernel32.dll", true, false),
        vec![common::KERNEL32]
    );

    // The discriminating case: a space-separated "kernel dll" is a single literal containing a
    // space, which no name contains -> NO match. An AND-of-tokens engine would (wrongly) return
    // kernel32.dll here because the name contains both "kernel" and "dll". It does not.
    assert!(idx.search("kernel dll", true, false).is_empty());
    // A space anywhere makes it a literal-with-space: still nothing matches.
    assert!(idx.search("readme txt", true, false).is_empty());
}

#[test]
fn search_contig_agrees_with_parallel_scan() {
    // The experimental single-buffer memmem must return the same set as
    // the default parallel per-entry scan for the case-insensitive path.
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Entry ids sorted ascending by fold_off (a fresh index is already in this order).
    let mut by_fold_off: Vec<u32> = (0..idx.entries().len() as u32).collect();
    by_fold_off.sort_by_key(|&id| idx.entries()[id as usize].fold_off);

    for (query, orphans) in [
        ("kernel32", false),
        ("dll", false),
        ("readme", false),
        ("KERNEL32", false),
        ("orphan", false),
        ("orphan", true),
        ("no-such-file", true),
        ("", false),
    ] {
        assert_eq!(
            sorted(idx.search(query, true, orphans)),
            sorted(idx.search_contig(query, orphans, &by_fold_off)),
            "search_contig disagrees for {query:?} (orphans={orphans})",
        );
    }
}

/// The contig pass walks its `fold_off -> id` map forward from where the previous hit left it,
/// instead of bisecting the whole map for each one. That is only sound because `find_iter`
/// yields offsets in increasing order, and it is only *fast* if the walk gallops, so this
/// exercises both ends of that: a one-character needle matching nearly every name (the cursor
/// advances constantly, and several hits land inside one name), and a needle that occurs once
/// at the far end (the cursor must jump most of the map in one go).
#[test]
fn search_contig_agrees_when_the_cursor_has_to_move_far() {
    let mut records = vec![FakeRecord::dir(5, 5, "")];
    for i in 0..2_000u64 {
        // Names of differing lengths so the fold offsets are not uniformly spaced.
        records.push(FakeRecord::file(
            100 + i,
            5,
            &format!("aa{}aa{}.txt", "a".repeat((i % 7) as usize), i),
        ));
    }
    records.push(FakeRecord::file(9_999, 5, "zzz-only-at-the-very-end.log"));
    let mut vol = FakeVolume::new(records);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let mut by_fold_off: Vec<u32> = (0..idx.entries().len() as u32).collect();
    by_fold_off.sort_by_key(|&id| idx.entries()[id as usize].fold_off);

    for query in [
        "a",
        "aa",
        "z",
        "zzz-only-at-the-very-end",
        ".txt",
        "1999",
        "0",
        "nothing-here",
    ] {
        assert_eq!(
            sorted(idx.search(query, true, false)),
            sorted(idx.search_contig(query, false, &by_fold_off)),
            "search_contig disagrees for {query:?}",
        );
    }
}

#[test]
fn fold_map_lifecycle_across_mutations() {
    // Build -> the adopted contig path is active (map complete).
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "docs"),
        FakeRecord::file(20, 10, "alpha.txt"),
        FakeRecord::file(21, 10, "beta.txt"),
    ]);
    let mut idx = build_from_volume(&mut vol, DRIVE).unwrap();
    assert!(idx.fold_map_ready(), "map must be ready after a bulk build");
    assert_eq!(idx.search("alpha", true, false).len(), 1);

    // A create empties the map (the new id is uncovered) - and the fallback
    // parallel scan still finds the new file immediately.
    let jid = vol.journal_info().unwrap().journal_id;
    let vol2 =
        FakeVolume::new(Vec::new())
            .with_events(vec![FakeEvent::create(100, 30, 10, "gamma.txt").close()]);
    vol2.read_journal(0, jid, &mut |ev| idx.apply_event(ev))
        .unwrap();
    assert!(!idx.fold_map_ready(), "a create must invalidate the map");
    assert_eq!(idx.search("gamma", true, false).len(), 1);

    // Rebuild (what the watch loop does after applies) -> contig again, and it
    // must see the post-mutation truth: gamma found, renamed names swapped.
    idx.rebuild_fold_map();
    assert!(idx.fold_map_ready());
    assert_eq!(idx.search("gamma", true, false).len(), 1);

    // A rename also invalidates (fold_off moves)...
    let vol3 = FakeVolume::new(Vec::new()).with_events(vec![
        FakeEvent::rename_old(200, 20, 10, "alpha.txt"),
        FakeEvent::rename_new(201, 20, 10, "delta.txt").close(),
    ]);
    vol3.read_journal(0, jid, &mut |ev| idx.apply_event(ev))
        .unwrap();
    assert!(!idx.fold_map_ready(), "a rename must invalidate the map");
    idx.rebuild_fold_map();
    assert!(idx.search("alpha", true, false).is_empty());
    assert_eq!(idx.search("delta", true, false).len(), 1);

    // ...but a delete-only batch keeps the map valid (tombstones are filtered
    // at hit time; no fold offset changed), and the dead entry disappears.
    let vol4 = FakeVolume::new(Vec::new())
        .with_events(vec![FakeEvent::delete(300, 21, 10, "beta.txt").close()]);
    vol4.read_journal(0, jid, &mut |ev| idx.apply_event(ev))
        .unwrap();
    assert!(idx.fold_map_ready(), "a delete must NOT invalidate the map");
    assert!(idx.search("beta", true, false).is_empty());
}
