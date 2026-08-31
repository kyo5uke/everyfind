//! Cross-check that the client highlight and the index engine fold **identically**: the
//! single-source-of-truth invariant. Both paths go through the
//! shared `index::fold`, so every engine match must yield a non-empty highlight span. This test
//! is the teeth of the invariant: if a future fold change (e.g. the M6 ASCII on-the-fly
//! optimization) touched only the engine, this would fail.

use everyfind::index::{build_from_volume, Index};
use everyfind::tui::highlight::filename_spans;
use everyfind::volume::{FakeRecord, FakeVolume};

/// Build an index whose files carry the given names (all directly under the root).
fn index_with(names: &[&str]) -> Index {
    let mut recs = vec![FakeRecord::dir(5, 5, "")];
    for (i, name) in names.iter().enumerate() {
        recs.push(FakeRecord::file(100 + i as u64, 5, name));
    }
    let mut vol = FakeVolume::new(recs);
    build_from_volume(&mut vol, 'C').unwrap()
}

#[test]
fn every_engine_match_is_highlighted_including_tricky_folds() {
    // Names spanning ASCII, full-width, emoji, and both length-changing folds (İ grows, ẞ shrinks).
    let names = [
        "Kernel32.DLL",
        "検索🔍ファイル.rs",
        "İstanbul.txt",
        "STRAẞE.log",
    ];
    let idx = index_with(&names);

    // Searching each name by itself must match (a name contains itself) AND highlight must find a
    // span in every reconstructed path the engine returned.
    for name in names {
        let ids = idx.search(name, true, false);
        assert!(
            !ids.is_empty(),
            "engine did not match name {name:?} by itself"
        );
        for id in ids {
            let path = idx.path(id);
            assert!(
                !filename_spans(&path, name).is_empty(),
                "engine matched {name:?} but highlight found no span in {path:?}",
            );
        }
    }
}

#[test]
fn substring_queries_agree_between_engine_and_highlight() {
    let idx = index_with(&["Kernel32.DLL", "検索🔍ファイル.rs", "STRAẞE.log"]);
    // (query, a name it should match). Each: engine matches, and highlight marks it in the path.
    for (query, name) in [
        ("kernel32", "Kernel32.DLL"),
        ("32.dll", "Kernel32.DLL"),
        ("検索", "検索🔍ファイル.rs"),
        ("🔍", "検索🔍ファイル.rs"),
        ("ß", "STRAẞE.log"),      // ẞ folds to ß
        ("straße", "STRAẞE.log"), // whole-run across the shrinking fold
    ] {
        let ids = idx.search(query, true, false);
        assert!(
            !ids.is_empty(),
            "engine did not match {query:?} (expected {name:?})"
        );
        // At least one matched path highlights the query.
        let any_highlight = ids
            .iter()
            .map(|&id| idx.path(id))
            .any(|p| !filename_spans(&p, query).is_empty());
        assert!(
            any_highlight,
            "engine matched {query:?} but no path highlighted"
        );
    }
}

#[test]
fn a_non_matching_query_matches_nothing() {
    let idx = index_with(&["Kernel32.DLL"]);
    assert!(idx.search("no-such-thing", true, false).is_empty());
}
