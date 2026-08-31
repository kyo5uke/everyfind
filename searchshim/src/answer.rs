//! What Everyfind answers an Explorer search with, and how much of it.
//!
//! Everything here is a decision about the *result*: which volume can serve the folder, which
//! rows survive the user's excludes and Windows' own crawl scope, how many of them a page
//! holds, and what order they go out in. None of it knows what COM is; the rowset, the
//! vtables and the OLE DB conversation live next door in `lib.rs`, and the only thing that
//! crosses is [`Answer`].
//!
//! That separation is the point. Every number in this file was arrived at by measuring
//! Explorer, and the measurements are quoted where the numbers are set; being able to read
//! them without reading a vtable patch is most of why they are readable at all.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{crawlscope, log, logging_rows, now_ms, provider};

// How many rows a page may hold, when the `MaxRows` value overrides [`MAX_ROWS`]. Zero means
// "no override". Read from the registry rather than compiled in because the ceiling is the
// *view's*, not ours, and what a view can take depends on the machine and on what the rows
// are: a page of media files costs Explorer thumbnails a page of source files does not.
// Settable without a rebuild, so the number can be found by trying it.
pub(crate) static MAX_ROWS_SET: AtomicUsize = AtomicUsize::new(0);

/// Whether Everyfind can answer this search at all.
///
/// The two cases look alike from the outside and behave nothing alike: `Rows(vec![])` blanks
/// the window (the user searched, and there genuinely is nothing) while `Defer` hands the
/// search back to Windows untouched. Confusing them is silent, because a blanked window and
/// an empty folder look identical, so every "we cannot answer" path says so with a reason
/// and the only route to `Rows` is having actually asked the daemon about the right volume.
pub(crate) enum Answer {
    Rows(Vec<provider::Row>),
    Defer(&'static str),
}

/// Recent answers, keyed by term+scope, each with the time it was taken.
///
/// Execute fires many times per search, and one daemon round trip per call would be pointless
/// traffic on an Explorer thread. But this Explorer will be running for days and the index
/// behind it is live, so an answer expires.
///
/// A handful of entries rather than one, because of how a search box is actually used. Typing
/// `taikonauts` runs ten searches, and backspacing over it runs them again in reverse. With a
/// single slot every one of those is a miss, and the expensive ones are the *short* prefixes:
/// `t` under one home directory is 1.87 million hits, measured between 2.0 and 5.5 seconds.
/// Deleting back through them is exactly the reported "it sits on searching and never
/// finishes", and all of it is recomputing answers we had a moment ago.
static RECENT: std::sync::Mutex<Vec<(String, Vec<provider::Row>, u128)>> =
    std::sync::Mutex::new(Vec::new());

/// How many answers to keep. Enough to cover backspacing through a word, few enough that the
/// pages (up to [`MAX_ROWS`] rows, each holding a path and its metadata) stay a few megabytes
/// inside explorer.exe.
const RECENT_KEEP: usize = 8;

/// How long a cached answer stands. Long enough to cover a burst of typing, short enough that
/// a search repeated later sees the current filesystem.
const RESULT_TTL_MS: u128 = 15_000;

/// The drive the daemon indexes, and when we last asked.
///
/// Everyfind serves one volume. A search rooted anywhere else has no answer here, and saying
/// "no matches" for it would blank a window Windows could have filled, so the volume has to
/// be checked, which means asking. The answer is held briefly: long enough that typing does
/// not pay for it, short enough to pick up a daemon restarted onto another volume.
static DAEMON_DRIVE: std::sync::Mutex<Option<(char, u128)>> = std::sync::Mutex::new(None);
const DRIVE_TTL_MS: u128 = 60_000;

fn daemon_drive() -> Option<char> {
    use everyfind_ipc::{client, Request, Response, PIPE_NAME};

    if let Ok(g) = DAEMON_DRIVE.lock() {
        if let Some((drive, at)) = *g {
            if now_ms().saturating_sub(at) < DRIVE_TTL_MS {
                return Some(drive);
            }
        }
    }
    let drive = match client::request(
        PIPE_NAME,
        &Request::Status,
        std::time::Duration::from_secs(2),
    ) {
        Ok(Response::Status(s)) => s.drive,
        _ => return None,
    };
    if let Ok(mut g) = DAEMON_DRIVE.lock() {
        *g = Some((drive, now_ms()));
    }
    Some(drive)
}

/// This machine's crawl scope, and when we last read it.
///
/// Held for the same reason and the same length of time as the drive: a search should not pay
/// to re-read a hundred-odd registry keys, and the rules change when somebody opens Indexing
/// Options rather than between keystrokes.
static CRAWL_RULES: std::sync::Mutex<Option<(crawlscope::Rules, u128)>> =
    std::sync::Mutex::new(None);

fn crawl_rules() -> crawlscope::Rules {
    if let Ok(g) = CRAWL_RULES.lock() {
        if let Some((rules, at)) = g.as_ref() {
            if now_ms().saturating_sub(*at) < DRIVE_TTL_MS {
                return rules.clone();
            }
        }
    }
    let rules = crawlscope::load();
    if let Ok(mut g) = CRAWL_RULES.lock() {
        *g = Some((rules.clone(), now_ms()));
    }
    rules
}

/// Whether `path_lc` is `scope_lc` or sits underneath it. Both arguments are already
/// lower-cased, and `scope_lc` carries no trailing separator.
///
/// A bare prefix test is not enough: it puts `c:\workspace\a.txt` under a search of
/// `c:\work`, where the characters match and the directory does not. The boundary has to
/// land on a separator or on the end of the path.
fn within_scope(path_lc: &str, scope_lc: &str) -> bool {
    if scope_lc.is_empty() {
        return true;
    }
    path_lc
        .strip_prefix(scope_lc)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['\\', '/']))
}

/// Whether a daemon indexing `drive` can serve a search rooted at `scope_lc`.
///
/// Everyfind indexes one volume. The folder has to name a location, and that location has to
/// sit on that volume; anything else the daemon simply cannot see, and the honest answer is
/// to let Windows take the search rather than report the emptiness as a result.
fn servable_by(scope_lc: &str, drive: char) -> bool {
    let mut chars = scope_lc.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(c), Some(':')) if c.eq_ignore_ascii_case(&drive)
    )
}

/// Everyfind's answer for one search, or the reason there is none.
pub(crate) fn answer_for(term: &str, scope: &str) -> Answer {
    let scope_lc = scope
        .trim_end_matches('"')
        .trim_end_matches('\\')
        .to_lowercase();

    // The volume first: everything downstream assumes the daemon can see this folder, and a
    // folder it cannot see is exactly the case that would otherwise arrive as a plausible
    // zero: a drive the daemon does not index returns no paths, every one of them survives
    // the scope filter, and the window goes blank.
    match daemon_drive() {
        None => return Answer::Defer("daemon did not answer a status request"),
        // Only checkable when the shell told us which folder was searched. When it did not,
        // the search is volume-wide and there is no folder to be on the wrong volume.
        Some(drive) if !scope_lc.is_empty() && !servable_by(&scope_lc, drive) => {
            return Answer::Defer("the searched folder is not on the volume the daemon indexes")
        }
        Some(_) => {}
    }

    let key = format!("{term}\u{1}{scope}");
    if let Ok(guard) = RECENT.lock() {
        if let Some((_, rows, _)) = guard
            .iter()
            .find(|(k, _, at)| *k == key && now_ms().saturating_sub(*at) < RESULT_TTL_MS)
        {
            log(&format!("CACHE: hit for '{term}' ({} rows)", rows.len()));
            return Answer::Rows(rows.clone());
        }
    }

    let t0 = now_ms();
    let rows = match everyfind_paths(term, scope, &scope_lc) {
        Ok(r) => r,
        Err(why) => return Answer::Defer(why),
    };
    log(&format!(
        "IPC: daemon round trip {}ms for '{term}' ({} paths)",
        now_ms() - t0,
        rows.len()
    ));
    if let Ok(mut guard) = RECENT.lock() {
        // Newest first, and expired entries dropped on the way, so a repeat is found on the
        // first comparison and nothing accumulates for a window nobody is typing in.
        guard.retain(|(k, _, at)| *k != key && now_ms().saturating_sub(*at) < RESULT_TTL_MS);
        guard.insert(0, (key, rows.clone(), now_ms()));
        guard.truncate(RECENT_KEEP);
    }
    Answer::Rows(rows)
}

/// Cap on rows handed to the view, unless the `MaxRows` value overrides it.
///
/// Five thousand, from measuring the view rather than guessing at it. `json` under one home
/// directory, which leaves 4,864 rows after every filter:
///
/// ```text
/// cap    shown   our Execute   explorer.exe        responding
///  1000   1000        779 ms   +37 s CPU, 389 MB   yes
///  5000   4864       2692 ms   +53 s CPU, 425 MB   yes
/// 20000   4864       3030 ms   +50 s CPU, 428 MB   yes
/// ```
///
/// The shell takes it in its stride; the earlier reading that it could not was a probe of
/// ours hung on a COM call, not Explorer, and the page had in fact filled within five seconds.
/// The cost that does grow is ours, and it is the date each row is ordered by: about half a
/// millisecond a row on cold paths even spread across the cores. Five thousand spends two
/// seconds of that in the worst case and stops well short of the point where a page is more
/// than anyone scrolls.
///
/// Raise it with `MaxRows` on the shim's key if a machine wants more; there was nothing at
/// twenty thousand that broke, only rows that never arrived because [`FETCH_ROWS`] ran out.
const MAX_ROWS: u32 = 5_000;

/// How many rows to ask the daemon for, deliberately far more than [`MAX_ROWS`].
///
/// Excludes and the same-name limit are applied to the answer, so rows that get thrown away
/// still cost a slot in what we asked for. Measured volume-wide on "a": of the top 500, the
/// excludes drop all but 92 and the same-name limit leaves 59; a whole-drive search showing
/// under sixty results. Asking for 5,000 instead leaves 2,811, and costs nothing: the daemon's
/// time goes into the scan, not the answer (905 ms for 500 rows, 907 ms for 5,000, and still
/// about a second for 100,000, and the flatness holds all the way up).
///
/// Raised to thirty thousand once the flatness was measured properly. It is not free in
/// principle (thirty thousand paths cross the pipe), but it is free in practice, and it is
/// the only lever that puts *more* rows on a page the view will accept: of 10,437 hits for
/// "tai", the scope, the excludes and the crawl-scope rules leave 1,713. Asking for more
/// candidates is how a filtered page fills up.
pub(crate) const FETCH_ROWS: u32 = 30_000;

/// How many of the rows we ask for are held back for hits matched *inside* a name.
///
/// One tenth, measured. A third put nothing else on the page at all: directly under a home
/// directory only four names begin with "a" while three dozen merely contain one, so a large
/// reservation filled the shallow rows entirely and the nearest-first sort then carried them
/// to the top, pushing `AppData` and `anaconda3` off a fifty-row page. At a tenth both show:
/// the prefix matches lead and the rest follow, which is the order stock Explorer produces.
const RESERVE_ELSEWHERE: u32 = FETCH_ROWS / 10;

/// How many rows to show: the cap, whatever the search found.
///
/// This used to shrink for a wide search, on the reasoning that a wide one arrives on the
/// first keystroke and its answer is least useful when its cost is most visible. Both halves
/// of that turned out to be wrong the way it mattered. The cost of a page is rows times what
/// Explorer spends on a row, and it does not know or care how many hits it came from, so
/// there is nothing to save by showing *fewer* rows precisely when there is more to look at.
/// And a hit count is a bad proxy for a vague query: `json` under one home directory matches
/// ninety thousand names, which put it in the "wide" bucket and cut the list to five hundred,
/// for a term nobody types by accident.
///
/// One number, and it is [`max_rows`], except for a term too short to be a search yet.
///
/// The hit count is the wrong signal; the *term* is the right one. Explorer does not run a
/// search per keystroke, it debounces, so typing a long word fires one search on an early
/// prefix and another when the typing settles, and the log of a real session shows exactly
/// that: `t` (2,239,943 hits, five thousand rows) issued while the box already read
/// `taikonauts`, twice, thirty seconds apart. Our own Execute for it takes 700-1,300 ms
/// against 20 ms for the long term, and the page it hands over costs the shell 6.7 s of CPU
/// against 2.3 s for a six-row answer (measured; 500 rows of the same query costs 3.6 s).
///
/// All of that is spent on an answer the user typed past before it arrived, and the window
/// says "searching" until it lands. It is why a *longer* word is less stable than a short one,
/// which is the shape of the report: the longer the word, the more room between the first
/// keystroke and the last for the early search to fire.
///
/// So a one- or two-character term gets a page sized for glancing at rather than reading. It
/// is not a worse answer to a question anyone asked: Everyfind ranks, so the rows kept are the
/// best ones, and by the third character the full page is back.
fn rows_to_show(term: &str) -> usize {
    match term.chars().count() {
        0 | 1 => 200.min(max_rows()),
        2 => 1_000.min(max_rows()),
        _ => max_rows(),
    }
}

/// How many rows to ask the daemon for, given the term.
///
/// [`FETCH_ROWS`] over-fetches so the page still fills after the excludes, the crawl scope and
/// the same-name limit have taken their share. A short term needs the same headroom over a
/// much smaller page, not the same absolute number: measured on this volume, `t` costs the
/// daemon 302 ms for one row and 518 ms for thirty thousand, so the over-fetch is worth about
/// 200 ms on precisely the search that is already the slow one.
fn fetch_rows(term: &str) -> u32 {
    let show = rows_to_show(term) as u32;
    // The same ratio the full page is given (30,000 asked for, 5,000 shown), so the filters
    // have the same room to drop rows whatever the term is.
    (show.saturating_mul(6)).min(FETCH_ROWS)
}

/// The page cap in force: the `MaxRows` value if one was set, else [`MAX_ROWS`].
fn max_rows() -> usize {
    match MAX_ROWS_SET.load(Ordering::Relaxed) {
        0 => MAX_ROWS as usize,
        n => n,
    }
}

/// How many rows on that page may carry the same name.
///
/// Trimming the page is not enough on its own: Everyfind ranks exact name matches first, so a
/// one-character search fills the page with them. Measured on this volume, thirty-six
/// directories are named exactly `a`, so a fifty-row page for "a" was thirty-six rows reading
/// "a" and fourteen of anything else, which is how a substring search comes to look like an
/// exact-match one. Keeping a few of each name and moving on spends those rows on the varied
/// list Explorer's own engine would have shown, and changes nothing about *which* files match.
///
/// Only where the page is a sample of the answer rather than the answer; see [`is_sampled`].
/// A search that has narrowed at all shows every hit: the ten copies of `readme.md` you
/// searched for are the answer, not repetition to be thinned.
///
/// This used to trigger a thousand hits in, which was far too eager and was the real reason a
/// page looked capped. Measured: `json` under one home directory matches 40,386 names, and
/// keeping three of each left **five hundred rows**; the row cap never even came into it, and
/// raising the cap changed nothing. Somebody who types `json` wants the list, not a sample of
/// it.
fn max_per_name(total_hits: u64) -> usize {
    if is_sampled(total_hits) {
        3
    } else {
        usize::MAX
    }
}

/// Whether there are so many hits that a page can only ever be a sample.
///
/// Half a million, which on this volume is the one- and two-character mark: `t` matches 1.3
/// million names, `ta` 165 thousand, `tai` ten thousand, `json` ninety thousand. Only the
/// first is thinned.
///
/// The bar is this high because thinning was a fix for a *fifty-row* page, where three dozen
/// directories all named `a` really did crowd out everything else. A page of a thousand rows
/// is a different thing: the same three dozen are 3% of it, and thinning at the old bar of a
/// thousand hits was cutting `json` down to five hundred rows to protect against a problem it
/// did not have.
fn is_sampled(total_hits: u64) -> bool {
    total_hits > 500_000
}

/// Whether the search is still wide enough that the near hits should be preferred to the deep
/// ones when choosing which rows survive the cut. A lower bar than [`is_sampled`], because
/// this only reorders candidates, never drops one.
fn is_wide(total_hits: u64) -> bool {
    total_hits > 1_000
}

/// Build the daemon query for `term` scoped to `scope`, as a leading `path:` term.
/// Mirrors the canonical `seed_query` in `ef.rs` (edge cases learned the hard way):
/// a drive root scopes to nothing (`path:C:` matches the whole volume and only
/// costs a full-path rebuild per candidate), a trailing `\` or `"` is stripped
/// (Explorer's `--in "%V"` hands a drive root over as `C:"`), and the path is
/// quoted only when it contains spaces (a seed ending in `"` would fuse with the
/// next token). The scope precedes the term so the trailing space keeps them
/// separate tokens.
fn seed_scope(scope: &str, term: &str) -> String {
    let trimmed = scope.trim_end_matches('"').trim_end_matches('\\');
    let is_drive_root = trimmed.len() == 2
        && trimmed.ends_with(':')
        && trimmed.starts_with(|c: char| c.is_ascii_alphabetic());
    if trimmed.is_empty() || is_drive_root {
        return term.to_string();
    }
    let scoped = if trimmed.contains(' ') {
        format!("path:\"{trimmed}\"")
    } else {
        format!("path:{trimmed}")
    };
    format!("{scoped} {term}")
}

/// Everyfind's paths for `term` under `scope`, or the reason there are none to be had.
/// `scope_lc` is `scope` lower-cased with no trailing separator (see [`within_scope`]).
///
/// This runs on an Explorer thread, so it is bounded and never panics: a failure becomes a
/// reason to let Windows answer, never a hung window.
fn everyfind_paths(
    term: &str,
    scope: &str,
    scope_lc: &str,
) -> Result<Vec<provider::Row>, &'static str> {
    use everyfind_ipc::{client, Request, Response, PIPE_NAME};
    use std::time::Duration;

    // The user's persistent excludes, the same `%APPDATA%\everyfind\excludes.txt` the `ef`
    // CLI honors. Windows Search never indexes the caches and build trees a whole-volume index
    // does, so without them a short query is buried under hundreds of identically named cache
    // and build directories. Reading the file per search keeps edits live.
    //
    // They are applied to the *answer*, not asked of the daemon. As query terms they become
    // `!path:` predicates, and a path predicate has to rebuild every candidate's full path to
    // test it: measured on this volume, the fourteen fragments cost 3.5 s of a 5.1 s search
    // for "a": the same search runs in 1.5 s without them, over 2.5x *more* candidates.
    // Filtering rows we already hold costs nothing and drops the same entries (of the top 500
    // for "a", 173), and the 500 we ask for leaves ample headroom above what is shown.
    let excludes: Vec<String> = everyfind_ipc::config::load_excludes()
        .into_iter()
        .map(|e| e.to_lowercase())
        .collect();
    let req = Request::Search {
        // Seed the query with the searched folder as a `path:` term so the daemon
        // ranks and applies the row limit *within* that folder. Without it we asked
        // for the global top `MAX_ROWS` and filtered to the folder afterward, which
        // starved deep folders: for a common term the whole global top-N is taken
        // by matches elsewhere on the drive, leaving little or nothing under the
        // folder (e.g. "a" returned only files literally named "a", never the ones
        // merely containing it).
        query: seed_scope(scope, term),
        case_sensitive: false,
        include_orphans: false,
        limit: fetch_rows(term),
        // A third of the page is held for hits matched *inside* a name. Ranking puts exact and
        // prefix matches first, so a short term with thousands of prefix matches fills the page
        // with them and the substring hits never arrive, measured under one home directory,
        // `a` returned one of them in five hundred rows, so the search looked prefix-only
        // exactly when the user had typed least. That is narrower than Everyfind can be, and
        // narrower than *Explorer*, which matches the start of any word in a name and would
        // have answered "a" with `-Old Audio-`.
        //
        // Unconditional, because the hit count is not known until the answer comes back, and
        // it costs nothing when the search has narrowed: a set that fits in the page is
        // returned whole, so there is nothing for the reservation to take room from.
        reserve_elsewhere: RESERVE_ELSEWHERE,
    };
    // A timeout, not a wait: if the daemon is wedged, fall back rather than hang an Explorer
    // thread for ever.
    //
    // Thirty seconds, not five. Five is inside the range a legitimate search takes: `t` under
    // one home directory is 1.87 million hits and was measured between 2.0 and 5.5 seconds, and
    // typing a word walks through every prefix of it. Timing out there is the worst of both:
    // the search is handed to Windows, whose own engine is slower still on the same folder (it
    // is the reason this DLL exists), so a slow answer becomes no answer and the window sits on
    // "searching". The deadline is for a daemon that has stopped answering, and thirty seconds
    // says that without ever accusing a busy one.
    let t_daemon = now_ms();
    let (results, building, total_hits) =
        match client::request(PIPE_NAME, &req, Duration::from_secs(30)) {
            Ok(Response::Search {
                results,
                building,
                total_hits,
            }) => (results, building, total_hits),
            _ => return Err("daemon unreachable"),
        };
    // The daemon says so when its index is not ready yet, and the empty result that comes
    // with it means "ask again later", not "nothing matches". Taking it at face value blanks
    // every Explorer search for the twenty-odd seconds of an initial build, and for however
    // long a re-enumeration takes after the journal wraps.
    if building {
        return Err("daemon is still building its index");
    }
    // Windows Search scopes results to the searched folder; match that so the
    // view shows what the breadcrumb promises.
    let show = rows_to_show(term);
    let per_name = max_per_name(total_hits);

    // Leave out what Windows leaves out, when Windows would have. Its crawl scope excludes the
    // recycle bin, `AppData` and the rest of the noise a whole-volume index turns up and its
    // own search never shows, along with whatever the user excluded themselves in Indexing
    // Options. But only where the index is what Explorer would have used: a search of the drive
    // root or of `C:\Windows` is one Explorer answers by walking, honouring no exclusion at
    // all, and hiding half the volume from it would be narrower than the Explorer we stand in
    // for. Searching *inside* an excluded folder is the same case and falls out of the same
    // test: the folder is not indexed either, so nothing under it is hidden.
    let daemon_ms = now_ms() - t_daemon;
    let t_filter = now_ms();

    let rules = crawl_rules();
    let like_windows = rules.indexes(scope_lc);

    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut kept: Vec<everyfind_ipc::SearchHit> = results
        .into_iter()
        .filter(|h| {
            let lc = h.path.to_lowercase();
            if !within_scope(&lc, scope_lc) || excludes.iter().any(|e| lc.contains(e)) {
                return false;
            }
            if like_windows && rules.hides(&lc) {
                return false;
            }
            let leaf = lc.rsplit(['\\', '/']).next().unwrap_or(&lc).to_string();
            let n = seen.entry(leaf).or_insert(0);
            *n += 1;
            *n <= per_name
        })
        .collect();

    // A wide search keeps the near hits in preference to the deep ones. Everyfind ranks an
    // exact name match above everything else wherever it lives, which is right for a term you
    // have finished typing and wrong for one you have not: searching "a" under a home directory
    // kept fifteen folders literally named "a", six to eleven levels down inside build and
    // decompiler output, in place of `AppData` and `anaconda3` sitting directly in it. Depth
    // brings the near ones into the page, and the sort is stable, so Everyfind's own ranking
    // still orders each level. This decides *which* rows survive the cut, not what the finished
    // list looks like: see [`in_explorer_order`].
    if is_wide(total_hits) {
        kept.sort_by_key(|h| h.path.bytes().filter(|&b| b == b'\\' || b == b'/').count());
    }
    kept.truncate(show);
    let filter_ms = now_ms() - t_filter;
    let t_order = now_ms();
    let kept = in_explorer_order(kept);
    let order_ms = now_ms() - t_order;
    // Where the time went, not just how much of it there was. The three phases scale with
    // different things: the daemon with the hit count, the filter with `FETCH_ROWS`, the
    // ordering with the rows actually shown, so one total tells you nothing about which knob
    // to turn.
    log(&format!(
        "ROWS: {total_hits} hits -> showing {} (cap {show}, crawl scope {} rules, filtered={like_windows})          | daemon {daemon_ms}ms + filter {filter_ms}ms + order {order_ms}ms",
        kept.len(),
        rules.len()
    ));
    if logging_rows() {
        // One write, not one per row. `log` opens, appends and closes the file each call, so a
        // five-thousand-row page was five thousand file opens on an Explorer thread, measured
        // at about three seconds, which is more than everything this function actually does and
        // made every `--debug` timing useless for deciding what to make faster.
        let mut dump = String::with_capacity(kept.len() * 64);
        for (i, row) in kept.iter().enumerate() {
            use std::fmt::Write;
            let _ = writeln!(dump, "ROW {i:5}: {}", row.path);
        }
        dump.pop(); // `log` adds the final newline
        log(&dump);
    }
    Ok(kept)
}

/// Lay the page out the way Explorer's own search lays it out: folders first, then by date
/// modified, newest first.
///
/// Measured against both genuine engines rather than assumed. With the shim transparent, a
/// search of a fixture whose modified times had been set to contradict its creation order came
/// back 2030, 2029, 2028, 2027, 2026 -- date modified, newest first -- with its one directory
/// ahead of every file despite being the oldest thing in the set. The indexed engine agrees by
/// having no opinion at all: every row of the same query carried `System.Search.Rank` 368, the
/// same constant, so what the user sees there is the view's order too, not the engine's.
/// Explorer has no notion of a best match; it never did.
///
/// Everyfind still decides *which* rows arrive here, and that is where its ranking earns its
/// keep. The sort is stable, so that ranking breaks every tie -- including for rows whose date
/// cannot be read, which sink to the bottom in rank order rather than floating to the top.
///
/// The date costs one metadata call per row, and that call is the only part of a page whose
/// cost grows with its size, so it is the one part that decides how large a page can be. It
/// is spread across the cores for exactly that reason: measured over ten thousand rows, 2 s
/// serially against 177 ms on eight threads, and over fifty thousand, 20 s against 1 s. The
/// work is one blocking syscall per row with nothing shared between them, which is the shape
/// that divides cleanly; the threads are scoped, so this returns only when they all have.
///
/// `symlink_metadata` rather than `metadata` so a reparse point is dated as itself. A
/// whole-volume index turns up dead junctions, and following one to time out is not worth a
/// date nobody asked for, and it matches what Explorer's own listing reports, which comes
/// from `FindFirstFile` and describes the link rather than its target.
/// How long the dates get before the page goes out without them.
///
/// Ten times the worst honest case rather than a guess: the same measurement that sizes the
/// page puts ten thousand rows at 177 ms across eight threads, and a page is capped at
/// [`MAX_ROWS`]. So this only ever fires for a path that is not going to answer at all.
const ORDER_BUDGET: std::time::Duration = std::time::Duration::from_millis(1_500);

fn in_explorer_order(rows: Vec<everyfind_ipc::SearchHit>) -> Vec<provider::Row> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // One chunk per thread rather than one task per row: the syscall is short enough that
    // handing them out individually would cost more in coordination than it saves.
    let chunk = rows.len().div_ceil(threads).max(1);
    let mut seen: Vec<Option<std::fs::Metadata>> = (0..rows.len()).map(|_| None).collect();
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<Option<std::fs::Metadata>>)>();
    let mut chunks = 0usize;
    for (ci, part) in rows.chunks(chunk).enumerate() {
        // The worker takes its own copy of the paths so it owns everything it touches. That
        // is what lets this function return without it; see the deadline below.
        let paths: Vec<String> = part.iter().map(|h| h.path.clone()).collect();
        let tx = tx.clone();
        if std::thread::Builder::new()
            .name("ef-stat".into())
            .spawn(move || {
                let got: Vec<_> = paths
                    .iter()
                    .map(|p| std::fs::symlink_metadata(p).ok())
                    .collect();
                let _ = tx.send((ci, got));
            })
            .is_ok()
        {
            chunks += 1;
        }
    }
    drop(tx);

    // Collected with a deadline, not joined.
    //
    // `symlink_metadata` has no timeout and the paths come from a whole-volume index, so some
    // of them are junctions into an offline share, a removed drive, or a cloud folder whose
    // provider is not answering. One of those blocks the redirector for its own timeout,
    // tens of seconds, and joining meant the page waited for it. On an Explorer thread that
    // is the window sitting on "searching" with every row already in hand but undeliverable.
    //
    // A row whose date did not arrive keeps `None`, which is a case this already has: it
    // sorts to the bottom in rank order, exactly as a row whose stat *failed* does. So the
    // deadline costs ordering quality for a few rows and never costs the page.
    let deadline = std::time::Instant::now() + ORDER_BUDGET;
    let mut arrived = 0usize;
    while arrived < chunks {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok((ci, got)) => {
                let at = ci * chunk;
                for (i, m) in got.into_iter().enumerate() {
                    if let Some(slot) = seen.get_mut(at + i) {
                        *slot = m;
                    }
                }
                arrived += 1;
            }
            // Every sender is gone: a worker panicked rather than answering. Its rows keep
            // their blanks, which is what a panicked chunk always contributed.
            Err(_) => break,
        }
    }
    if arrived < chunks {
        log(&format!(
            "ORDER: {}/{chunks} chunks dated within {ORDER_BUDGET:?}; the rest sort undated \
             (a path that does not stat back: an offline junction, a disconnected drive)",
            arrived
        ));
    }
    let mut dated: Vec<(everyfind_ipc::SearchHit, Option<std::fs::Metadata>)> =
        rows.into_iter().zip(seen).collect();
    dated.sort_by(|a, b| {
        let at = |m: &Option<std::fs::Metadata>| m.as_ref().and_then(|m| m.modified().ok());
        folders_then_newest((a.0.is_dir, at(&a.1)), (b.0.is_dir, at(&b.1)))
    });
    // The metadata read for the date rides on to the row, so `fill` does not read it again.
    dated
        .into_iter()
        .map(|(h, meta)| provider::Row::seen(h.path, meta))
        .collect()
}

/// The comparison [`in_explorer_order`] sorts by, kept apart from the filesystem so it can be
/// checked against a set of dates rather than against a machine.
///
/// A missing date orders last: `None` is below every `Some`, and the descending sense of the
/// comparison leaves it at the bottom.
fn folders_then_newest(
    a: (bool, Option<std::time::SystemTime>),
    b: (bool, Option<std::time::SystemTime>),
) -> std::cmp::Ordering {
    b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1))
}

#[cfg(test)]
mod tests {
    use super::{
        fetch_rows, folders_then_newest, in_explorer_order, max_per_name, rows_to_show, seed_scope,
        servable_by, within_scope, FETCH_ROWS, MAX_ROWS,
    };

    /// Every row must come back carrying **its own** metadata.
    ///
    /// The dates are gathered in chunks across the cores and written back by chunk index, so a
    /// row and its date are only together because the arithmetic says so. Get that wrong and
    /// nothing fails loudly: the page still fills, with the wrong dates on the wrong files
    /// and an order derived from them. This gives every file a different size and insists each
    /// row's metadata reports the size belonging to its own path, at row counts either side of
    /// the chunk boundary.
    #[test]
    fn each_row_keeps_its_own_metadata() {
        for count in [1usize, 7, 64, 257] {
            let dir = std::env::temp_dir().join(format!("ef-order-test-{count}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            let hits: Vec<everyfind_ipc::SearchHit> = (0..count)
                .map(|i| {
                    let p = dir.join(format!("f{i:04}.bin"));
                    // Size i+1, so a row's size names the row.
                    std::fs::write(&p, vec![b'x'; i + 1]).expect("write");
                    everyfind_ipc::SearchHit {
                        path: p.to_string_lossy().into_owned(),
                        is_dir: false,
                    }
                })
                .collect();

            let ordered = in_explorer_order(hits);

            assert_eq!(ordered.len(), count, "{count}: rows lost");
            for row in &ordered {
                let want: u64 = row
                    .path
                    .rsplit(['\\', '/'])
                    .next()
                    .and_then(|leaf| leaf.strip_prefix('f'))
                    .and_then(|n| n.strip_suffix(".bin"))
                    .and_then(|n| n.parse::<u64>().ok())
                    .map(|i| i + 1)
                    .expect("a test file name");
                let meta = row
                    .meta
                    .as_ref()
                    .unwrap_or_else(|| panic!("{count}: {} came back with no metadata", row.path));
                assert_eq!(
                    meta.len(),
                    want,
                    "{count}: {} was given another row's metadata",
                    row.path
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Sort a labelled set the way the page is sorted, so a test reads as an order.
    fn laid_out(rows: &[(&'static str, bool, Option<u64>)]) -> Vec<&'static str> {
        let mut rows: Vec<_> = rows
            .iter()
            .map(|&(name, is_dir, secs)| {
                let at = secs.map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
                (name, is_dir, at)
            })
            .collect();
        rows.sort_by(|a, b| folders_then_newest((a.1, a.2), (b.1, b.2)));
        rows.into_iter().map(|(name, _, _)| name).collect()
    }

    /// The order Explorer's own search produced for a fixture built to tell the rules apart:
    /// the directory was the *oldest* thing in it and still came first, and the five files came
    /// back strictly newest-first, ignoring name, depth and path length alike.
    #[test]
    fn the_page_is_laid_out_the_way_explorer_lays_one_out() {
        let measured = laid_out(&[
            ("Virtual Insanity.tja", false, Some(2026)),
            ("insanity_virtual_reversed.txt", false, Some(2027)),
            ("Virtual Insanity.ogg", false, Some(2028)),
            ("zzz_virtual_insanity_last.wav", false, Some(2029)),
            ("Jamiroquai - Virtual Insanity.mp4", false, Some(2030)),
            ("Virtual Insanity", true, Some(2025)),
        ]);
        assert_eq!(
            measured,
            [
                "Virtual Insanity",
                "Jamiroquai - Virtual Insanity.mp4",
                "zzz_virtual_insanity_last.wav",
                "Virtual Insanity.ogg",
                "insanity_virtual_reversed.txt",
                "Virtual Insanity.tja",
            ]
        );
    }

    /// A date that cannot be read must not promote a row. Rows share a date here, and one has
    /// none at all: the first two keep the order Everyfind ranked them in, and the undated row
    /// goes last rather than to the top.
    #[test]
    fn an_unreadable_date_sinks_and_ranking_breaks_the_ties() {
        assert_eq!(
            laid_out(&[
                ("ranked first", false, Some(100)),
                ("ranked second", false, Some(100)),
                ("no date", false, None),
            ]),
            ["ranked first", "ranked second", "no date"]
        );
    }

    #[test]
    fn scope_boundary_falls_on_a_separator() {
        assert!(within_scope(r"c:\work", r"c:\work"), "the folder itself");
        assert!(within_scope(r"c:\work\a.txt", r"c:\work"), "a child");
        assert!(
            within_scope(r"c:\work\deep\a.txt", r"c:\work"),
            "a descendant"
        );
        // The bug a bare prefix test has: same characters, different directory.
        assert!(!within_scope(r"c:\workspace\a.txt", r"c:\work"));
        assert!(!within_scope(r"c:\working", r"c:\work"));
        assert!(
            !within_scope(r"d:\work\a.txt", r"c:\work"),
            "another volume"
        );
        assert!(
            within_scope(r"c:\anything", ""),
            "an empty scope holds everything"
        );
    }

    /// Measured hit counts from this volume, so the thresholds are pinned to real searches
    /// rather than to round numbers: `t` -> 1.3M, `ta` -> 165k, `tai` -> 10k, `taikonauts` -> 35.
    #[test]
    fn a_page_is_sized_by_the_term_not_by_the_hit_count() {
        let all = MAX_ROWS as usize;
        // How many matched never changes the page: `json` has ninety thousand hits and every
        // one of them is wanted.
        for term in ["tai", "json", "readme", "taikonauts"] {
            assert_eq!(
                rows_to_show(term),
                all,
                "{term:?}: a page costs rows, not hits"
            );
        }
        // A term still being typed does. Explorer debounces, so an early prefix is searched
        // while the box already holds the whole word. Measured live: `t` five thousand rows,
        // issued twice while the box read `taikonauts`.
        assert!(rows_to_show("t") < all, "a one-character page is a glance");
        assert!(rows_to_show("ta") < all);
        assert!(
            rows_to_show("t") <= rows_to_show("ta") && rows_to_show("ta") <= rows_to_show("tai"),
            "the page must not shrink as the term grows"
        );
        // And we stop asking the daemon for rows we will not show.
        for term in ["t", "ta", "tai"] {
            assert!(
                fetch_rows(term) as usize >= rows_to_show(term),
                "{term:?}: asking for fewer rows than the page holds"
            );
            assert!(fetch_rows(term) <= FETCH_ROWS);
        }
        assert!(
            fetch_rows("t") < FETCH_ROWS,
            "the over-fetch is worth ~200 ms on exactly the slowest search"
        );
        assert!(
            (FETCH_ROWS as usize) > all,
            "the answer must survive the excludes and the same-name limit and still fill a page"
        );
    }

    /// A search that has narrowed shows every hit it found; only one so wide that the page can
    /// only be a sample spends its rows on variety. `wfn:a` counts 36 names of exactly "a" on
    /// this volume, which is what filled a fifty-row page before this existed.
    #[test]
    fn only_a_sampled_search_thins_repeated_names() {
        assert_eq!(max_per_name(37), usize::MAX, "'taikonauts': show them all");
        assert_eq!(max_per_name(1_000), usize::MAX);
        assert_eq!(
            max_per_name(91_413),
            usize::MAX,
            "'json': ninety thousand hits, and every one of them wanted"
        );
        assert_eq!(max_per_name(165_000), usize::MAX, "'ta': two characters in");
        assert_eq!(max_per_name(1_312_391), 3, "'t': one character in");
        assert_eq!(max_per_name(3_733_096), 3, "'a'");
    }

    #[test]
    fn only_the_daemons_own_volume_is_servable() {
        assert!(servable_by(r"c:\users\me", 'C'));
        assert!(servable_by(r"c:\", 'c'), "case does not matter");
        assert!(
            !servable_by(r"d:\data", 'C'),
            "the daemon indexes one volume"
        );
        assert!(!servable_by("", 'C'), "no folder named");
        assert!(
            !servable_by(r"\\server\share\x", 'C'),
            "a UNC path is on no drive letter"
        );
    }

    #[test]
    fn seed_scope_narrows_to_the_folder_but_not_to_a_drive_root() {
        assert_eq!(
            seed_scope(r"C:\Users\me", "notes"),
            r"path:C:\Users\me notes"
        );
        assert_eq!(
            seed_scope(r"C:\Program Files", "exe"),
            "path:\"C:\\Program Files\" exe",
            "a space in the path has to be quoted"
        );
        // A drive root scopes to nothing, so seeding it only costs a path rebuild per hit.
        assert_eq!(seed_scope(r"C:\", "notes"), "notes");
        assert_eq!(seed_scope("C:", "notes"), "notes");
        assert_eq!(seed_scope("", "notes"), "notes");
    }
}
