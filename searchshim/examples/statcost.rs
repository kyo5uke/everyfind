//! What does a big page cost?
//!
//! Explorer's own search view orders results by date modified, newest first: measured, not
//! assumed. Everyfind's index does not store that date, so matching the order means asking
//! the filesystem once per row we are about to hand over, and that is what decides how many
//! rows a page can hold. Whether a larger cap is affordable is a number, not an opinion.
//!
//! Reports, per page size: the daemon's own time, one serial stat pass, and one spread over
//! the machine's cores, the obvious fix if the serial number is the problem.
//!
//! Usage: `cargo run --release --example statcost -- <term> [scope]`

use everyfind_ipc::{client, Request, Response, SearchHit, PIPE_NAME};
use std::time::{Duration, Instant};

/// Page sizes worth knowing about: today's cap, and the ones a user with a common term
/// would actually want.
const PAGES: [usize; 5] = [1_000, 5_000, 10_000, 50_000, 100_000];

fn stat_serial(rows: &[SearchHit]) -> usize {
    rows.iter()
        .filter(|h| {
            std::fs::symlink_metadata(&h.path)
                .and_then(|m| m.modified())
                .is_ok()
        })
        .count()
}

/// The same work split across the cores, which is what the shim would do if the serial pass
/// turned out to be the thing standing between the user and a page worth having.
fn stat_parallel(rows: &[SearchHit], threads: usize) -> usize {
    let chunk = rows.len().div_ceil(threads.max(1));
    std::thread::scope(|s| {
        let handles: Vec<_> = rows
            .chunks(chunk.max(1))
            .map(|part| s.spawn(move || stat_serial(part)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).sum()
    })
}

fn main() {
    let term = std::env::args().nth(1).unwrap_or_else(|| "a".into());
    let scope = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "C:\\Users".into());
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("term {term:?} under {scope:?}, {cores} cores\n");

    for want in PAGES {
        let req = Request::Search {
            query: format!("path:\"{scope}\" {term}"),
            case_sensitive: false,
            include_orphans: false,
            limit: want as u32,
            reserve_elsewhere: (want / 10) as u32,
        };
        let t = Instant::now();
        let rows = match client::request(PIPE_NAME, &req, Duration::from_secs(60)) {
            Ok(Response::Search { results, .. }) => results,
            other => {
                println!("{want:6}: no results ({other:?})");
                continue;
            }
        };
        let daemon = t.elapsed();

        let t = Instant::now();
        let ok = stat_serial(&rows);
        let serial = t.elapsed();

        let t = Instant::now();
        stat_parallel(&rows, cores);
        let parallel = t.elapsed();

        println!(
            "{:6} asked -> {:6} rows | daemon {:>8.0?} | stat serial {:>8.0?} | stat x{cores} {:>8.0?} | {ok} dated",
            want,
            rows.len(),
            daemon,
            serial,
            parallel
        );
    }
}
