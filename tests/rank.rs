//! M6 step 3, result ranking: exact > prefix > substring, shorter path on ties,
//! bounded top-N selection.

use everyfind::index::build_from_volume;
use everyfind::index::query::parse;
use everyfind::volume::{FakeRecord, FakeVolume};

/// ```text
/// id frn parent  name              rank vs "kernel32.dll"
///  0   5    5    ""                (root)
///  1  10    5    deep              dir
///  2  11   10    deeper            dir
///  3  20    5    kernel32.dll      exact, shallow
///  4  21    5    kernel32.dll.mui  prefix
///  5  22    5    akernel32.dll     substring
///  6  23   11    kernel32.dll      exact, deep
/// ```
fn vol() -> FakeVolume {
    FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "deep"),
        FakeRecord::dir(11, 10, "deeper"),
        FakeRecord::file(20, 5, "kernel32.dll"),
        FakeRecord::file(21, 5, "kernel32.dll.mui"),
        FakeRecord::file(22, 5, "akernel32.dll"),
        FakeRecord::file(23, 11, "kernel32.dll"),
    ])
}

#[test]
fn exact_then_prefix_then_substring_then_path_length() {
    let mut v = vol();
    let idx = build_from_volume(&mut v, 'C').unwrap();
    let q = parse("kernel32.dll");
    let ids = idx.search_query(&q, true, false);

    let top = idx.top_ranked(&ids, &q, true, 10, 0);
    assert_eq!(
        top,
        vec![3, 6, 4, 5],
        "exact-shallow, exact-deep, prefix, substring"
    );
}

#[test]
fn limit_truncates_after_ranking_not_before() {
    let mut v = vol();
    let idx = build_from_volume(&mut v, 'C').unwrap();
    let q = parse("kernel32.dll");
    let ids = idx.search_query(&q, true, false);

    // Even though id 4/5 precede id 6 in index order, the two exact matches win.
    assert_eq!(idx.top_ranked(&ids, &q, true, 2, 0), vec![3, 6]);
    assert_eq!(idx.top_ranked(&ids, &q, true, 1, 0), vec![3]);
    assert!(idx.top_ranked(&ids, &q, true, 0, 0).is_empty());
}

/// A `limit` far larger than the hit set still returns exactly the hits, in rank order.
/// `limit` reaches the index straight off the wire, and it sizes the heap reservation; the
/// bound that keeps that reservation proportional to the hit count must not also truncate
/// the answer.
#[test]
fn oversized_limit_returns_every_hit_in_rank_order() {
    let mut v = vol();
    let idx = build_from_volume(&mut v, 'C').unwrap();
    let q = parse("kernel32.dll");
    let ids = idx.search_query(&q, true, false);

    assert_eq!(
        idx.top_ranked(&ids, &q, true, u32::MAX as usize, 0),
        vec![3, 6, 4, 5],
        "an oversized limit still returns exactly the hits, in rank order"
    );
}

#[test]
fn ranking_is_case_fold_aware() {
    let mut v = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::file(20, 5, "KERNEL32.DLL"),
        FakeRecord::file(21, 5, "kernel32.dll.mui"),
    ]);
    let idx = build_from_volume(&mut v, 'C').unwrap();
    let q = parse("kernel32.dll");
    let ids = idx.search_query(&q, true, false);

    // The folded name equals the folded needle -> exact class despite the case.
    assert_eq!(idx.top_ranked(&ids, &q, true, 10, 0), vec![1, 2]);
}

#[test]
fn operator_only_query_orders_by_path_length() {
    let mut v = vol();
    let idx = build_from_volume(&mut v, 'C').unwrap();
    let q = parse("ext:dll");
    let ids = idx.search_query(&q, true, false);

    // No positive word -> everything ties on class; shortest paths surface first.
    // c:\kernel32.dll (3) < c:\akernel32.dll (5) < c:\deep\deeper\kernel32.dll (6).
    assert_eq!(idx.top_ranked(&ids, &q, true, 10, 0), vec![3, 5, 6]);
}

/// A term with more name-prefix matches than there are rows leaves no room for the hits
/// matched *inside* a name: the ones only a substring search finds. Measured on a real
/// volume: `a` has 93,232 names beginning with it, so a page of 500 held one substring hit.
/// The reservation buys those rows back.
#[test]
fn a_reservation_keeps_room_for_matches_inside_the_name() {
    let mut v = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        // Four names starting with "ab" ...
        FakeRecord::file(20, 5, "ab1.txt"),
        FakeRecord::file(21, 5, "ab2.txt"),
        FakeRecord::file(22, 5, "ab3.txt"),
        FakeRecord::file(23, 5, "ab4.txt"),
        // ... and two that merely contain it.
        FakeRecord::file(24, 5, "xxab1.txt"),
        FakeRecord::file(25, 5, "xxab2.txt"),
    ]);
    let idx = build_from_volume(&mut v, 'C').unwrap();
    let q = parse("ab");
    let ids = idx.search_query(&q, true, false);

    // Rank order alone: the four prefix matches take every row.
    assert_eq!(idx.top_ranked(&ids, &q, true, 4, 0), vec![1, 2, 3, 4]);

    // Reserving two rows drops the weakest two prefix matches for the two inside-the-name
    // hits, and the result is still in rank order.
    assert_eq!(idx.top_ranked(&ids, &q, true, 4, 2), vec![1, 2, 5, 6]);

    // A reservation larger than the number of such hits takes only what exists.
    assert_eq!(idx.top_ranked(&ids, &q, true, 4, 9), vec![1, 2, 5, 6]);

    // With room for everything the reservation changes nothing.
    assert_eq!(
        idx.top_ranked(&ids, &q, true, 10, 2),
        vec![1, 2, 3, 4, 5, 6]
    );
}
