//! A/B timing for the query engine, on a synthetic volume.
//!
//! Only uses syntax that existed *before* the M7 rewrite, so the same file can
//! be run against either implementation and the numbers mean the same thing.
//!
//! `cargo test --release --test bench_query -- --ignored --nocapture`

mod common;

use std::time::Instant;

use everyfind::index::build_from_volume;
use everyfind::index::query::parse;
use everyfind::volume::{FakeRecord, FakeVolume};

const DIRS: u64 = 20_000;
const FILES: u64 = 2_000_000;

/// A tree with a plausible spread of names, extensions and depths.
fn big_volume() -> FakeVolume {
    let exts = [
        "rs", "toml", "md", "txt", "dll", "exe", "png", "json", "lock", "log",
    ];
    let stems = [
        "main", "lib", "mod", "config", "readme", "index", "utils", "kernel32", "package",
        "report", "test", "build", "cargo", "server", "client",
    ];
    let mut recs = Vec::with_capacity((DIRS + FILES) as usize + 1);
    recs.push(FakeRecord::dir(5, 5, ""));

    // Directories, chained so depth varies from 1 to ~5.
    for i in 0..DIRS {
        let frn = 100 + i;
        let parent = if i < 40 {
            5
        } else {
            100 + (i % 40) + (i / 400) * 41
        };
        recs.push(FakeRecord::dir(frn, parent, &format!("dir{i:04}")));
    }
    for i in 0..FILES {
        let frn = 1_000_000 + i;
        let parent = 100 + (i % DIRS);
        let stem = stems[(i % stems.len() as u64) as usize];
        let ext = exts[((i / 7) % exts.len() as u64) as usize];
        recs.push(FakeRecord::file(frn, parent, &format!("{stem}_{i}.{ext}")));
    }
    FakeVolume::new(recs)
}

fn time(idx: &everyfind::index::Index, q: &str, runs: u32) -> (u128, usize) {
    let parsed = parse(q);
    let mut best = u128::MAX;
    let mut hits = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let r = idx.search_query(&parsed, true, false);
        best = best.min(t.elapsed().as_micros());
        hits = r.len();
    }
    (best, hits)
}

#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn query_timings() {
    let mut vol = big_volume();
    let t = Instant::now();
    let idx = build_from_volume(&mut vol, 'C').unwrap();
    println!(
        "index: {} entries, built in {} ms\n",
        DIRS + FILES + 1,
        t.elapsed().as_millis()
    );

    // Pre-M7 syntax only: this is the comparison that has to hold.
    let cases = [
        ("kernel32", "plain word (fast path)"),
        ("cargo", "plain word (fast path)"),
        ("kernel32 dll", "two words, AND"),
        ("ext:rs", "ext"),
        ("ext:rs;toml;md", "ext, 3 alternatives"),
        ("readme ext:md", "word + ext"),
        ("path:dir0001", "path (component mode)"),
        ("path:dir0001\\", "path (joined mode)"),
        ("report !log", "word + negation"),
        ("report !ext:log !path:dir0002", "word + two negations"),
    ];
    println!("{:<34} {:>10} {:>12}", "query", "best (us)", "hits");
    for (q, label) in cases {
        let (us, hits) = time(&idx, q, 30);
        println!("{:<34} {us:>10} {hits:>12}   {label}", format!("{q:?}"));
    }
}

/// How long it takes to turn a page of hits into the paths that go on the wire.
///
/// The daemon's `Search` handler does this for every row it returns, and the Explorer shim
/// asks for 30,000 of them on every keystroke, so it is paid once per character typed,
/// whatever the search itself cost. Each row walks the entry's ancestors and allocates a
/// `String`, and the rows are independent, so the question is only whether spreading them
/// across the cores is worth it at the sizes actually asked for.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn path_build_timings() {
    use rayon::prelude::*;

    let mut vol = big_volume();
    let idx = build_from_volume(&mut vol, 'C').unwrap();
    let all: Vec<u32> = (1..=(DIRS + FILES) as u32).collect();

    println!(
        "{:<10} {:>12} {:>12} {:>9}",
        "rows", "serial (us)", "parallel (us)", "speedup"
    );
    for rows in [1_000usize, 5_000, 30_000, 200_000] {
        let ids = &all[..rows.min(all.len())];
        let mut serial = u128::MAX;
        let mut parallel = u128::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let a: Vec<String> = ids.iter().map(|&id| idx.path(id)).collect();
            serial = serial.min(t.elapsed().as_micros());
            let t = Instant::now();
            let b: Vec<String> = ids.par_iter().map(|&id| idx.path(id)).collect();
            parallel = parallel.min(t.elapsed().as_micros());
            assert_eq!(
                a, b,
                "parallel must produce the same paths, in the same order"
            );
        }
        println!(
            "{rows:<10} {serial:>12} {parallel:>12} {:>8.1}x",
            serial as f64 / parallel as f64
        );
    }
}

/// Where a broad search actually spends its time: finding the hits, or ordering them.
///
/// The answer is ordering them, which is not where anyone looks. `top_ranked` runs on one core
/// and calls `rank_key` per hit, which walks the entry's ancestors to measure the path length
/// it sorts by, millions of walks on a one-character query. Measured here at 56-82% of the
/// query, larger than the scan in every case.
///
/// Two fixes were tried against this number and **both measured no better**, which is why the
/// code above it is unchanged:
///
/// - *Parallelising the loop.* Each worker keeps its own bounded heap and the merge pushes one
///   through the other. With `limit` as large as the Explorer shim asks for (30,000) the merge
///   costs more than the walks it saves: `report` went 16.4 -> 23.6 ms.
/// - *Computing the cheap half of the key first.* A key is `(class, path_len, id)`, so a
///   candidate whose class is worse than the worst key held cannot get in whatever its path
///   length is. Interleaved A/B over three rounds: within noise, marginally worse. The reason
///   is that hits for one query overwhelmingly share a class, so the test never rejects and
///   only adds a comparison.
///
/// What would actually work is holding the path length per entry instead of deriving it,
/// +4 B/entry, and an invalidation problem on every re-parent. Not worth it until something
/// needs it; measured and left alone is the point of keeping this test.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn scan_versus_rank_timings() {
    let mut vol = big_volume();
    let idx = build_from_volume(&mut vol, 'C').unwrap();

    println!(
        "{:<20} {:>12} {:>10} {:>10} {:>7}",
        "query", "hits", "scan (us)", "rank (us)", "rank %"
    );
    for q in ["e", "re", "report", "kernel32", "ext:rs"] {
        let parsed = parse(q);
        let mut scan = u128::MAX;
        let mut rank = u128::MAX;
        let mut hits = 0usize;
        for _ in 0..5 {
            let t = Instant::now();
            let ids = idx.search_query(&parsed, true, false);
            scan = scan.min(t.elapsed().as_micros());
            hits = ids.len();
            let t = Instant::now();
            let top = idx.top_ranked(&ids, &parsed, true, 30_000, 3_000);
            rank = rank.min(t.elapsed().as_micros());
            assert!(top.len() <= 30_000);
        }
        println!(
            "{:<20} {hits:>12} {scan:>10} {rank:>10} {:>6.0}%",
            format!("{q:?}"),
            100.0 * rank as f64 / (scan + rank) as f64
        );
    }
}

/// `dupe:` builds a whole-volume table before it can answer anything, so it is the one request
/// a client can send that costs seconds, and it costs them under the index read lock, which
/// the writer-preferring watch thread then queues behind. Measured on the real volume before
/// this was parallelised: 5.9 s for `dupe:`, and 5.6 s for `kernel32 dupe:`, which returns 75
/// rows. Same table either way.
///
/// The sort is the whole of it, and what the sort costs is not its comparisons but where they
/// read from: comparing two names is two random loads into the fold arena, ~23 pairs deep per
/// element. Interleaved A/B of three builds over three rounds here (two million entries,
/// minima):
///
/// ```text
/// serial, comparing names       2.09 s
/// parallel, comparing names     0.49 s
/// parallel, comparing hashes    0.10 s
/// ```
///
/// Spreading the same comparisons across the cores was the obvious move and left it at
/// seconds on the real volume (5.9 -> 5.0 s). Hashing each name once and sorting on that,
/// consulting the arena only when two hashes collide, is what actually moved it.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn dupe_table_timings() {
    let mut vol = big_volume();
    let idx = build_from_volume(&mut vol, 'C').unwrap();
    for q in ["dupe:", "kernel32 dupe:"] {
        let (us, hits) = time(&idx, q, 3);
        println!("{:<18} {us:>10} us {hits:>10} hits", format!("{q:?}"));
    }
}

/// The excludes-file shape: a short word plus several separator-carrying `!path:` terms.
///
/// Each separator pattern takes the joined-path branch, and each one used to rebuild the
/// candidate's full path for itself: seven patterns, seven walks and seven `Vec`s per
/// candidate that survives the name test. On the real volume that was 1,650 ms for a
/// one-character query with the 18-line excludes file (196 ms with `--no-excludes`), which
/// is what the interactive TUI blocks on while the first keystroke's query is in flight.
/// None of the patterns here match anything, deliberately: the cost being measured is the
/// evaluation itself, not the filtering.
///
/// The arc, on this synthetic volume (2M entries, 133K candidates, best-of-5):
///
/// ```text
/// per-pattern path rebuilds (before)        74.4 ms
/// fused: one rebuild per candidate          35.8 ms
/// per-directory table (now)                 12.2 ms   (the word alone is ~11 ms)
/// ```
///
/// The full 18-line shape (7 separator + 11 component fragments) also lands at word-alone
/// speed: the component walk that used to run per candidate is one memoised probe per
/// directory plus one on the candidate's own name.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn excludes_shape_timings() {
    let mut vol = big_volume();
    let idx = build_from_volume(&mut vol, 'C').unwrap();

    let word = "mod"; // one stem in 15 -> a broad but not degenerate candidate set
    let seps = r#"!path:"go\pkg\mod" !path:"pip\cache" !path:"cargo\registry" !path:"conda\pkgs" !path:"git\objects" !path:"line\cache" !path:"apktool\out""#;
    let frags = "!path:node_modules !path:headlesschrome !path:bazelcache !path:genymotion !path:jadxout !path:smaliclasses !path:basedecompiled !path:ccache !path:objdir !path:npmcache !path:pipwheels";
    let cases = [
        (word.to_string(), "word alone"),
        (format!("{word} {seps}"), "word + 7 separator excludes"),
        (
            format!("{word} {seps} {frags}"),
            "word + full 18-line excludes",
        ),
    ];
    for (q, label) in &cases {
        let (us, hits) = time(&idx, q, 5);
        println!("{label:<28} {us:>10} us {hits:>10} hits");
    }
}
