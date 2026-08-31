//! M6 query language: operator matching against the sample volume.

mod common;

use common::{sample_volume, sorted, DRIVE};
use everyfind::index::build_from_volume;
use everyfind::index::query::parse;

#[test]
fn plain_query_equals_the_m1_search() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        idx.search_query(&parse("dll"), true, false),
        idx.search("dll", true, false)
    );
    assert_eq!(
        idx.search_query(&parse(""), true, true),
        idx.search("", true, true)
    );
}

#[test]
fn multiple_words_and_together() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        idx.search_query(&parse("kernel dll"), true, false),
        vec![common::KERNEL32]
    );
    // Order of terms does not matter.
    assert_eq!(
        idx.search_query(&parse("dll kernel"), true, false),
        vec![common::KERNEL32]
    );
}

#[test]
fn ext_filters_by_extension() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        sorted(idx.search_query(&parse("ext:dll"), true, false)),
        vec![common::KERNEL32, common::K32LINK]
    );
    // `;` alternatives OR within one term; case-insensitive (README.MD).
    assert_eq!(
        sorted(idx.search_query(&parse("ext:dll;md"), true, false)),
        vec![common::KERNEL32, common::README_MD, common::K32LINK]
    );
    assert_eq!(
        idx.search_query(&parse("readme ext:txt"), true, false),
        vec![common::README_TXT]
    );
    // Directories have no extension.
    assert!(idx
        .search_query(&parse("system ext:dll"), true, false)
        .is_empty());
}

#[test]
fn bang_excludes_names() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        idx.search_query(&parse("dll !kernel"), true, false),
        vec![common::K32LINK]
    );
    assert_eq!(
        idx.search_query(&parse("dll !k32link"), true, false),
        vec![common::KERNEL32]
    );
    assert!(idx
        .search_query(&parse("dll !kernel !k32"), true, false)
        .is_empty());
}

#[test]
fn path_matches_ancestors_and_spans_separators() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Everything under (and including) System32.
    assert_eq!(
        sorted(idx.search_query(&parse("path:system32"), true, false)),
        vec![common::SYSTEM32, common::KERNEL32]
    );
    // A fragment spanning a separator; k32link.dll sits at the root, so only
    // the real kernel32.dll survives.
    assert_eq!(
        idx.search_query(&parse("path:windows\\system32 dll"), true, false),
        vec![common::KERNEL32]
    );
    // Forward slashes are accepted.
    assert_eq!(
        idx.search_query(&parse("path:windows/system32 dll"), true, false),
        vec![common::KERNEL32]
    );
    // Case-insensitive even in case-sensitive word mode (path: folds always).
    assert_eq!(
        idx.search_query(&parse("path:SYSTEM32 dll"), true, false),
        vec![common::KERNEL32]
    );
}

#[test]
fn operators_still_exclude_orphans_by_default() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        idx.search_query(&parse("ext:txt"), true, false),
        vec![common::README_TXT]
    );
    assert_eq!(
        sorted(idx.search_query(&parse("ext:txt"), true, true)),
        vec![common::README_TXT, common::ORPHAN]
    );
}

#[test]
fn negated_path_and_ext_exclude() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // `!path:` prunes the System32 subtree; the root-level twin survives.
    assert_eq!(
        idx.search_query(&parse("dll !path:system32"), true, false),
        vec![common::K32LINK]
    );
    // The quoted, separator-spanning form (what the config excludes generate).
    assert_eq!(
        idx.search_query(&parse("dll !path:\"windows\\system32\""), true, false),
        vec![common::K32LINK]
    );
    // `!ext:` removes the .md twin.
    assert_eq!(
        idx.search_query(&parse("readme !ext:md"), true, false),
        vec![common::README_TXT]
    );
    // Everything excluded -> empty, not an error.
    assert!(idx
        .search_query(&parse("dll !ext:dll"), true, false)
        .is_empty());
}

#[test]
fn words_respect_the_case_flag_with_operators_present() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Case-sensitive words: "README" matches only README.MD.
    assert_eq!(
        idx.search_query(&parse("README ext:md"), false, false),
        vec![common::README_MD]
    );
    assert!(idx
        .search_query(&parse("readme ext:md"), false, false)
        .is_empty());
}

// ---------------------------------------------------------------------------
// M7: boolean structure, wildcards, and the new functions.
// ---------------------------------------------------------------------------

#[test]
fn pipe_ors_and_angle_brackets_group() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        sorted(idx.search_query(&parse("kernel32 | readme.txt"), true, false)),
        vec![common::KERNEL32, common::README_TXT]
    );
    // AND binds tighter than OR: <dll AND kernel> OR <readme AND md>.
    assert_eq!(
        sorted(idx.search_query(&parse("dll kernel | readme md"), true, false)),
        vec![common::KERNEL32, common::README_MD]
    );
    // Brackets override that.
    assert_eq!(
        sorted(idx.search_query(&parse("readme <ext:txt | ext:md>"), true, false)),
        vec![common::README_TXT, common::README_MD]
    );
    // Negating a group.
    assert_eq!(
        idx.search_query(&parse("readme !<ext:md>"), true, false),
        vec![common::README_TXT]
    );
}

#[test]
fn wildcards_match_the_whole_name() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        sorted(idx.search_query(&parse("*.dll"), true, false)),
        vec![common::KERNEL32, common::K32LINK]
    );
    assert_eq!(
        idx.search_query(&parse("kernel??.dll"), true, false),
        vec![common::KERNEL32]
    );
    // A wildcard anchors the whole name: `*.dll` must not match `xkernel32.dllx`
    // style substrings, and a bare word is still a substring search.
    assert_eq!(
        sorted(idx.search_query(&parse("nowildcards:32"), true, false)),
        sorted(idx.search_query(&parse("32"), true, false))
    );
}

#[test]
fn shape_modifiers() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        idx.search_query(&parse("wfn:readme.txt"), true, false),
        vec![common::README_TXT]
    );
    assert!(idx
        .search_query(&parse("wfn:readme"), true, false)
        .is_empty());
    assert_eq!(
        sorted(idx.search_query(&parse("startwith:readme"), true, false)),
        vec![common::README_TXT, common::README_MD]
    );
    assert_eq!(
        sorted(idx.search_query(&parse("endwith:.dll"), true, false)),
        vec![common::KERNEL32, common::K32LINK]
    );
    // `case:` overrides the caller's insensitive flag for that term only.
    assert_eq!(
        idx.search_query(&parse("case:README"), true, false),
        vec![common::README_MD]
    );
}

#[test]
fn regex_over_name_and_path() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        sorted(idx.search_query(&parse(r"regex:^readme\..*$"), true, false)),
        vec![common::README_TXT, common::README_MD]
    );
    assert_eq!(
        // A separator in a regex is escaped, as in any regex: `\\` is one `\`.
        idx.search_query(&parse(r"pathregex:windows\\system32\\kernel"), true, false),
        vec![common::KERNEL32]
    );
}

#[test]
fn kind_root_and_attributes() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        sorted(idx.search_query(&parse("folder:"), true, false)),
        vec![common::WINDOWS, common::SYSTEM32, common::SYMLINK_DIR]
    );
    assert_eq!(
        sorted(idx.search_query(&parse("file: ext:dll"), true, false)),
        vec![common::KERNEL32, common::K32LINK]
    );
    // `root:`: directly under C:\.
    assert_eq!(
        sorted(idx.search_query(&parse("root: folder:"), true, false)),
        vec![common::WINDOWS, common::SYMLINK_DIR]
    );
    // Only the reparse point carries `l`.
    assert_eq!(
        idx.search_query(&parse("attrib:l"), true, false),
        vec![common::SYMLINK_DIR]
    );
}

#[test]
fn depth_and_name_length() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // kernel32.dll is C:\Windows\System32\kernel32.dll, two directories deep.
    assert_eq!(
        idx.search_query(&parse("parents:2"), true, false),
        vec![common::KERNEL32]
    );
    assert_eq!(
        sorted(idx.search_query(&parse("len:<8 folder:"), true, false)),
        vec![common::WINDOWS]
    );
}

#[test]
fn child_counts_and_emptiness() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // System32 holds exactly one file and no directories.
    assert_eq!(
        sorted(idx.search_query(&parse("childfilecount:1 childfoldercount:0"), true, false)),
        vec![common::SYSTEM32]
    );
    // The only childless directory is the symlink.
    assert_eq!(
        idx.search_query(&parse("folder: empty:"), true, false),
        vec![common::SYMLINK_DIR]
    );
    // `child:` selects the *parent* of a match.
    assert_eq!(
        idx.search_query(&parse("child:kernel32.dll"), true, false),
        vec![common::SYSTEM32]
    );
}

#[test]
fn duplicate_names() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Nothing in the sample tree shares a folded name...
    assert!(idx.search_query(&parse("dupe:"), true, false).is_empty());
    // ...so add one and it shows up on both sides.
    let mut vol = everyfind::volume::FakeVolume::new(vec![
        everyfind::volume::FakeRecord::dir(5, 5, ""),
        everyfind::volume::FakeRecord::dir(10, 5, "a"),
        everyfind::volume::FakeRecord::dir(11, 5, "b"),
        everyfind::volume::FakeRecord::file(20, 10, "same.txt"),
        everyfind::volume::FakeRecord::file(21, 11, "SAME.TXT"),
        everyfind::volume::FakeRecord::file(22, 11, "other.txt"),
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let dupes: Vec<String> = idx
        .search_query(&parse("dupe:"), true, false)
        .iter()
        .map(|&id| idx.path(id))
        .collect();
    assert_eq!(dupes.len(), 2, "got {dupes:?}");
    assert!(dupes.iter().all(|p| p.to_lowercase().ends_with("same.txt")));
}

/// `dupe:` over many names at once, with groups of every size.
///
/// The table behind it groups entries by a hash of the name and only compares the bytes when
/// two hashes agree, so "equal names land adjacent and nothing else does" is an invariant of
/// the sort rather than something visible in a three-file tree. This builds a few thousand
/// entries whose multiplicities are known by construction and insists on exactly the set with
/// more than one holder: a grouping that merges two names, or splits one, fails here and
/// nowhere else.
#[test]
fn duplicate_names_across_many_groups() {
    use everyfind::volume::{FakeRecord, FakeVolume};

    let mut recs = vec![FakeRecord::dir(5, 5, "")];
    // Ten directories to spread the copies over, so duplicates are never siblings.
    for d in 0..10u64 {
        recs.push(FakeRecord::dir(100 + d, 5, &format!("d{d}")));
    }
    // name `n{i}.txt` appears `i % 4` times, so groups of size 0, 1, 2 and 3 all occur.
    let mut frn = 1_000u64;
    let mut expected: Vec<String> = Vec::new();
    for i in 0..2_000u64 {
        let copies = i % 4;
        let name = format!("n{i}.txt");
        for c in 0..copies {
            recs.push(FakeRecord::file(frn, 100 + (frn % 10), &name));
            if copies > 1 {
                expected.push(format!(r"{DRIVE}:\d{}\{name}", frn % 10));
            }
            frn += 1;
            let _ = c;
        }
    }
    expected.sort();

    let mut vol = FakeVolume::new(recs);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let mut got: Vec<String> = idx
        .search_query(&parse("dupe:"), true, false)
        .iter()
        .map(|&id| idx.path(id))
        .collect();
    got.sort();

    assert_eq!(
        got.len(),
        expected.len(),
        "wrong number of duplicates: {} vs {}",
        got.len(),
        expected.len()
    );
    assert_eq!(got, expected, "the duplicate set does not match");
}

#[test]
fn type_groups_and_count() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // `doc:` covers both .txt and .md; `.dll` is in no group (`exe:` is entry
    // points, not libraries).
    assert_eq!(
        sorted(idx.search_query(&parse("doc:"), true, false)),
        vec![common::README_TXT, common::README_MD]
    );
    assert!(idx.search_query(&parse("exe:"), true, false).is_empty());
    // `count:` is a cap the ranker applies, not a filter.
    let q = parse("count:1 ext:dll");
    assert_eq!(q.limit, Some(1));
    let hits = idx.search_query(&q, true, false);
    assert_eq!(hits.len(), 2);
    assert_eq!(idx.top_ranked(&hits, &q, true, 100, 0).len(), 1);
}

/// `folder:temp` means "a directory, named temp"; the value was being discarded, so it meant
/// "every directory". Measured on the real volume before the fix: `folder:temp` returned
/// 343,561 rows against 1,119 for the equivalent `temp folder:`, and `file:readme` returned
/// 2,821,039 against 2,321. The empty `words` also flattened ranking to path length.
#[test]
fn a_value_less_filter_still_takes_its_value() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let all_dirs = idx.search_query(&parse("folder:"), true, false);
    let named = idx.search_query(&parse("folder:system"), true, false);
    assert_eq!(named, vec![common::SYSTEM32]);
    assert!(
        named.len() < all_dirs.len(),
        "folder:system must narrow, not return every directory ({all_dirs:?})"
    );
    // Both spellings mean the same thing.
    assert_eq!(
        named,
        idx.search_query(&parse("system folder:"), true, false)
    );

    assert_eq!(
        idx.search_query(&parse("file:kernel"), true, false),
        vec![common::KERNEL32]
    );
    // The word has to reach ranking, or every hit ties in the bottom class.
    assert_eq!(parse("folder:system").words, vec!["system".to_string()]);
}

/// An unmatched `>` is a literal, not the end of the query. `parse` runs the descent once and
/// drops what is left, so breaking on a stray `>` silently truncated: `a > b` meant `a`, and
/// `>foo` meant `Expr::All`, every entry on the volume (measured: 6,163,356 rows).
#[test]
fn a_stray_close_does_not_truncate_the_query() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let everything = idx.search_query(&parse(""), true, true);
    let stray = idx.search_query(&parse(">readme"), true, true);
    assert!(
        stray.len() < everything.len(),
        "`>readme` must not match the whole volume ({stray:?} vs {} entries)",
        everything.len()
    );
    // It means what it would have meant without the stray character.
    assert_eq!(stray, idx.search_query(&parse("readme"), true, true));

    // The tail survives: `kernel > dll` is still both words, not just the first.
    assert_eq!(
        idx.search_query(&parse("kernel > dll"), true, false),
        vec![common::KERNEL32]
    );
    assert!(
        idx.search_query(&parse("kernel > zzzz"), true, false)
            .is_empty(),
        "the term after the stray `>` has to be applied"
    );

    // A matched group still groups.
    assert_eq!(
        idx.search_query(&parse("<kernel|readme> dll"), true, false),
        vec![common::KERNEL32]
    );
}

/// A size whose unit multiplication does not fit in `u64` is malformed input, and malformed
/// input degrades to a literal word. It used to wrap to zero in release: `size:>17179869184gb`
/// asked for files over sixteen exabytes and returned 2,220,544 of them, and to panic the
/// daemon's request thread in a debug build.
#[test]
fn an_oversized_size_does_not_wrap_or_panic() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let hits = idx.search_query(&parse("size:>17179869184gb"), true, true);
    assert!(
        hits.is_empty(),
        "nothing is larger than sixteen exabytes, so this must match nothing ({hits:?})"
    );
}

/// A path pattern is spelled with `\` in the haystack, so a typed `/` has to be normalised for
/// *every* shape, not just the one that had a test. `path:endwith:system32` worked; the same
/// term written with a forward slash in it compiled to a suffix nothing could match.
#[test]
fn forward_slashes_normalise_in_every_path_shape() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // `path:endwith:` is about the whole path, so it is the directory itself that ends there.
    let want = vec![common::SYSTEM32];
    assert_eq!(
        idx.search_query(&parse(r"path:endwith:windows\system32"), true, false),
        want
    );
    assert_eq!(
        idx.search_query(&parse("path:endwith:windows/system32"), true, false),
        want,
        "a forward slash has to mean the same thing"
    );
    assert_eq!(
        idx.search_query(&parse("path:startwith:windows/system32"), true, false),
        idx.search_query(&parse(r"path:startwith:windows\system32"), true, false)
    );
}

/// A parent chain that loops must not hang the daemon. A well-formed MFT cannot contain one,
/// but a partly-replayed journal can install it (`upsert` re-parents without an ancestry
/// check), and five of the seven parent walks were unbounded `loop`s, two of them growing a
/// `Vec` as they went, so the failure was a wedged daemon or an out-of-memory abort rather than
/// a wrong answer. One bounded walker now serves all of them.
#[test]
fn a_parent_cycle_terminates_every_walk() {
    use everyfind::index::query::parse;

    let mut vol = sample_volume();
    let mut idx = build_from_volume(&mut vol, DRIVE).unwrap();
    // A real cycle: WINDOWS is SYSTEM32's parent already, so point WINDOWS *back down* at it.
    // Naming them the other way round (as this test first did) is a no-op and tests nothing.
    idx.make_parent_cycle_for_test(common::WINDOWS, common::SYSTEM32);

    // Each of these walks the chain a different way; none may fail to return, and none may
    // build an unbounded string on the way.
    let rendered = idx.path(common::KERNEL32);
    assert!(
        rendered.len() < 200_000,
        "a cycle must be truncated, not turned into a path of every entry ({} bytes)",
        rendered.len()
    );
    let _ = idx.search_query(&parse("path:windows"), true, true);
    let _ = idx.search_query(&parse("parents:>1"), true, true);
    let _ = idx.search_query(&parse("kernel"), true, true);
}

/// `Query::words` is the *positive* substrings; `top_ranked` takes its needle from there. A
/// negated word used to be recorded too, so `report !documentation` ranked against a word every
/// surviving hit is guaranteed not to contain, and the exact/prefix/substring order collapsed
/// to path length for the whole result set.
#[test]
fn a_negated_word_is_not_the_ranking_needle() {
    assert_eq!(parse("dll !kernel").words, vec!["dll".to_string()]);
    assert_eq!(parse("!kernel").words, Vec::<String>::new());
    assert_eq!(
        parse("report !documentation").words,
        vec!["report".to_string()]
    );
    // Grouped negation counts too.
    assert_eq!(parse("a !<b|c>").words, vec!["a".to_string()]);
    // And the positives still all arrive.
    assert_eq!(
        parse("alpha beta").words,
        vec!["alpha".to_string(), "beta".to_string()]
    );
    assert_eq!(parse("!path:node_modules").paths, Vec::<String>::new());
}

/// `case:` on a path term has to mean something. Both readings were broken in opposite
/// directions: the single-component shortcut searched the folded arena and ignored the modifier
/// entirely, and the joined form compiled an unfolded needle and then tested it against the
/// folded path, where an uppercase byte cannot occur, so it matched nothing at all, always.
#[test]
fn case_on_a_path_term_is_honoured_in_both_directions() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // The fixture holds C:\Windows\System32\kernel32.dll. A `path:` term matches the
    // directory itself as well as what is under it.
    let under_system32 = vec![common::SYSTEM32, common::KERNEL32];
    let one_component = |q: &str| idx.search_query(&parse(q), true, false);
    assert_eq!(one_component("path:system32"), under_system32);
    assert!(
        !one_component("case:path:SYSTEM32").contains(&common::KERNEL32),
        "the wrong case must not match when case: was asked for"
    );
    assert_eq!(
        one_component("case:path:System32"),
        under_system32,
        "and the right case must"
    );

    // The joined form: more than one component, so it takes the other branch.
    let joined = |q: &str| idx.search_query(&parse(q), true, false);
    assert_eq!(
        joined(r"case:path:Windows\System32"),
        under_system32,
        "a case-sensitive joined path term used to match nothing at all"
    );
    assert!(joined(r"case:path:WINDOWS\SYSTEM32").is_empty());

    // And the same for a path regex.
    assert_eq!(
        idx.search_query(&parse("case:pathregex:System32"), true, false),
        under_system32
    );
    assert!(idx
        .search_query(&parse("case:pathregex:SYSTEM32"), true, false)
        .is_empty());
}

/// Regressions found by review of the same day's changes. Each of these is a query a search box
/// sees while the user is still typing.
#[test]
fn half_typed_operators_do_not_blank_or_flood_the_result_set() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let everything = idx.search_query(&parse(""), true, true).len();

    // A trailing `|`: the second alternative has not been typed yet. `parse_and` yields
    // `Expr::All` for the empty branch and `simplify` lets one `All` swallow the whole `Or`,
    // so this matched the entire volume: the same failure the `<`/`>` depth counter fixed,
    // one function away.
    let dangling_or = idx.search_query(&parse("readme |"), true, true);
    assert!(
        dangling_or.len() < everything,
        "`readme |` must not match everything ({} of {everything})",
        dangling_or.len()
    );
    assert_eq!(dangling_or, idx.search_query(&parse("readme"), true, true));

    // A half-typed `count:`. Falling through to a literal made it AND a word no name contains,
    // so the list blanked on every keystroke of `count:50`.
    assert_eq!(
        idx.search_query(&parse("readme count:"), true, false),
        idx.search_query(&parse("readme"), true, false),
        "`count:` with no value yet must not blank the results"
    );

    // A value-less filter with a wildcard or a separator in its value. These used to return
    // *everything* (the value was discarded); making them take the value turned that into
    // *nothing*, because the value was forced to a literal `Contains`.
    assert_eq!(
        idx.search_query(&parse("file:*.dll"), true, false),
        idx.search_query(&parse("*.dll file:"), true, false),
        "a glob given to `file:` must mean what it means anywhere else"
    );
    assert!(!idx
        .search_query(&parse("file:*.dll"), true, false)
        .is_empty());
    assert_eq!(
        idx.search_query(&parse(r"folder:windows\system32"), true, false),
        idx.search_query(&parse(r"path:windows\system32 folder:"), true, false),
        "a path given to `folder:` likewise"
    );
}

/// A query is a 64 KiB frame from an unprivileged local process, and every `!` or `<` used to
/// cost a stack frame in the parser and again in `simplify` / `compile` / `eval` / `Drop`.
/// Measured in release with the default 2 MiB worker stack: twenty thousand `!` overflowed and
/// took the whole daemon with it: a stack overflow cannot be caught. Past the nesting cap the
/// character is taken literally, which is this module's policy for input it cannot parse.
#[test]
fn a_deeply_nested_query_is_bounded_rather_than_fatal() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    for n in [100usize, 5_000, 60_000] {
        let bangs = "!".repeat(n) + "readme";
        let q = parse(&bangs);
        let _ = idx.search_query(&q, true, true); // must return, not abort
        drop(q); // the tree's own Drop recurses too

        let brackets = "<".repeat(n) + "readme";
        let q = parse(&brackets);
        let _ = idx.search_query(&q, true, true);
    }

    // Shallow nesting still means what it says.
    assert_eq!(
        idx.search_query(&parse("!!readme"), true, false),
        idx.search_query(&parse("readme"), true, false)
    );
}

/// The persistent excludes narrow the answer whatever the user has typed so far.
///
/// They used to be appended as bare text, so a trailing operator took them as its own operand:
/// `readme |` meant "named readme, **or** not in any excluded directory". Measured on the real
/// volume, 3,195,581 rows against the 11,509 the same query returns with `--no-excludes`.
#[test]
fn the_excludes_cannot_be_captured_by_a_half_typed_operator() {
    use everyfind::config::{as_query_suffix, compose};

    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();
    let suffix = as_query_suffix(&["system32".to_string()]);

    let hits = |q: &str| idx.search_query(&parse(&compose(q, &suffix)), true, false);

    // The exclusion applies to both alternatives of a real OR...
    let both = hits("kernel | readme");
    assert!(
        !both.contains(&common::KERNEL32),
        "the excluded directory must be excluded from every branch: {both:?}"
    );
    assert!(both.contains(&common::README_TXT));

    // ...and a dangling one does not turn the exclusion into an alternative.
    let dangling = hits("readme |");
    assert_eq!(dangling, hits("readme"));
    assert!(
        !dangling.contains(&common::KERNEL32),
        "a trailing `|` must not hand the excludes their own branch: {dangling:?}"
    );

    // No excludes configured: the query is passed through untouched, fast path intact.
    assert_eq!(compose("kernel32", ""), "kernel32");
}

/// A tree shaped like the cases several `!path:"a\b"` excludes have to get right: the excluded
/// directory itself, a file under it, a file whose *name* completes the pattern across the last
/// separator (`go\pkg` + `module.txt` contains `go\pkg\mod`), and clean neighbours.
///
/// ```text
/// id  path
///  0  c:\
///  1  c:\go
///  2  c:\go\pkg
///  3  c:\go\pkg\mod            (dir)
///  4  c:\go\pkg\mod\inside.txt
///  5  c:\go\pkg\module.txt     (boundary: contains go\pkg\mod)
///  6  c:\go\pkg\clean.txt
///  7  c:\line
///  8  c:\line\cache
///  9  c:\line\cache\sticker.txt
/// 10  c:\top.txt
/// ```
fn excludes_volume() -> everyfind::volume::FakeVolume {
    use everyfind::volume::{FakeRecord, FakeVolume};
    FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "go"),
        FakeRecord::dir(11, 10, "pkg"),
        FakeRecord::dir(12, 11, "mod"),
        FakeRecord::file(13, 12, "inside.txt"),
        FakeRecord::file(14, 11, "module.txt"),
        FakeRecord::file(15, 11, "clean.txt"),
        FakeRecord::dir(20, 5, "line"),
        FakeRecord::dir(21, 20, "cache"),
        FakeRecord::file(22, 21, "sticker.txt"),
        FakeRecord::file(23, 5, "top.txt"),
    ])
}

/// Several joined `!path:` terms on one conjunction: the excludes-file shape. Whatever the
/// evaluator does to amortise the path reconstruction, the semantics stay "substring of the
/// joined path", including the pattern that completes inside a file *name*.
#[test]
fn several_joined_path_excludes_keep_joined_path_semantics() {
    let mut vol = excludes_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let hits = |q: &str| sorted(idx.search_query(&parse(q), true, false));

    assert_eq!(
        hits(r#"txt !path:"go\pkg\mod" !path:"line\cache""#),
        vec![6, 10],
        "under-the-directory, name-boundary and second-exclude hits must all drop"
    );

    // A positive joined term and a negative one share the same reconstructed path.
    assert_eq!(hits(r#"txt path:"go\pkg" !path:"go\pkg\mod""#), vec![6]);

    // A `case:` path term keeps its own arena: raw `LINE\cache` does not match `line\cache`.
    assert_eq!(
        hits(r#"txt !path:"go\pkg\mod" !case:path:"LINE\cache""#),
        vec![6, 9, 10]
    );
}

/// The fused excludes against a brute-force scan of [`Index::path`], on a tree built to hit
/// the accelerator's corners: a needle whose first segment is a component *suffix* and whose
/// last completes inside a file name (`go\pkg\mod` in `c:\xgo\pkg\modish.txt`), repeated
/// segments (`a\a\b`), a trailing separator, a drive-anchored needle, and a leading separator
/// that must also reach entries under the synthetic `<orphan>` prefix.
#[test]
fn fused_excludes_match_a_bruteforce_path_scan() {
    use everyfind::volume::{FakeRecord, FakeVolume};
    let mut vol = FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "go"),
        FakeRecord::dir(11, 10, "pkg"),
        FakeRecord::dir(12, 11, "mod"),
        FakeRecord::file(13, 12, "inside.txt"),
        FakeRecord::file(14, 11, "module.txt"),
        FakeRecord::dir(20, 5, "a"),
        FakeRecord::dir(21, 20, "a"),
        FakeRecord::dir(22, 21, "b"),
        FakeRecord::file(23, 22, "deep.txt"),
        FakeRecord::file(24, 21, "still_a_a.txt"),
        FakeRecord::dir(30, 5, "cache"),
        FakeRecord::file(31, 30, "hot.txt"),
        FakeRecord::file(32, 5, "cachet.txt"),
        FakeRecord::file(40, 999, "lost.txt"),
        FakeRecord::file(41, 5, "top.txt"),
        FakeRecord::dir(50, 5, "xgo"),
        FakeRecord::dir(51, 50, "pkg"),
        FakeRecord::file(52, 51, "modish.txt"),
    ]);
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let needles = [
        r"go\pkg\mod",
        r"a\a\b",
        r"cache\",
        r"c:\go",
        r"\lost",
        "od",     // component fragment, matches dirs and file names alike
        "till_a", // component fragment inside one long name
    ];
    let quoted: Vec<String> = needles.iter().map(|n| format!("!path:\"{n}\"")).collect();
    let q = format!("txt {}", quoted.join(" "));

    for include_orphans in [false, true] {
        let engine = sorted(idx.search_query(&parse(&q), true, include_orphans));
        let brute: Vec<u32> = idx
            .search_query(&parse("txt"), true, include_orphans)
            .into_iter()
            .filter(|&id| {
                let p = idx.path(id).to_ascii_lowercase();
                !needles.iter().any(|n| p.contains(n))
            })
            .collect();
        assert_eq!(engine, sorted(brute), "include_orphans={include_orphans}");
    }
}
