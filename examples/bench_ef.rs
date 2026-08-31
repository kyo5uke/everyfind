//! End-to-end latency harness for the `ef` client.
//!
//! hyperfine 1.20's `--shell=none` (`-N`) does not tokenize the command string on Windows, and
//! its shell mode adds ~55 ms of `cmd.exe` startup, so neither measures the client honestly. This
//! spawns the real `ef.exe` via `std::process::Command` (a direct `CreateProcess`, **no shell**)
//! in a loop and reports the distribution. That is the true process-start + connect + search +
//! response + display time a user sees.
//!
//! ```text
//! cargo run --release --example bench_ef -- 100 kernel32 -n 5
//! ```
//! (A warm `efd` daemon must be running on the target volume.)

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use everyfind::ipc::client;
use everyfind::ipc::{Request, PIPE_NAME};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // `--inproc <runs> <query>`: measure the pure pipe round trip (connect + daemon search +
    // response), excluding the ef.exe process start, to isolate daemon-side latency.
    if args.first().map(String::as_str) == Some("--inproc") {
        let runs: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200);
        let query = args.get(2).cloned().unwrap_or_else(|| "kernel32".into());
        bench_inproc(runs, &query);
        return;
    }

    // `--search-bench <snapshot> <query>`: load the index in-process and time the current
    // parallel per-entry scan vs. the experimental single-buffer memmem.
    if args.first().map(String::as_str) == Some("--search-bench") {
        let snap = args
            .get(1)
            .cloned()
            .expect("usage: --search-bench <snapshot> <query>");
        let query = args.get(2).cloned().unwrap_or_else(|| "kernel32".into());
        bench_search(&snap, &query);
        return;
    }

    // `--concurrency <clients> <files> <tempdir>`: N clients hammer the daemon with searches
    // while `files` are created then deleted in `tempdir` (on the watched volume), forcing the
    // watch thread to apply a large USN burst under the write lock. Records the worst-case
    // client search latency and any errors (M3 concurrency acceptance).
    if args.first().map(String::as_str) == Some("--concurrency") {
        let clients: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
        let files: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10_000);
        let tempdir = args
            .get(3)
            .cloned()
            .expect("usage: --concurrency <clients> <files> <dir>");
        bench_concurrency(clients, files, &tempdir);
        return;
    }

    let runs: usize = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            eprintln!("usage: bench_ef <runs> <ef args...>");
            std::process::exit(2)
        });
    args.remove(0);
    let ef_args = args;

    // `ef.exe` sits one directory up from target/release/examples/.
    let exe = std::env::current_exe().expect("current_exe");
    let ef = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/release")
        .join("ef.exe");

    let run = || {
        Command::new(&ef)
            .args(&ef_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn ef")
    };

    // Warm up (pipe first-connect, page-ins).
    for _ in 0..8 {
        run();
    }

    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let status = run();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if !status.success() {
            eprintln!("warning: ef exited with {status}");
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    report(&format!("ef {ef_args:?} (process, no shell)"), &mut times);
}

/// Measure the pipe round trip directly (connect + request + response), no process spawn.
fn bench_inproc(runs: usize, query: &str) {
    let req = Request::Search {
        query: query.to_string(),
        case_sensitive: false,
        include_orphans: false,
        limit: 5,
        reserve_elsewhere: 0,
    };
    let call = || {
        client::request(PIPE_NAME, &req, Duration::from_secs(5)).expect("request");
    };
    for _ in 0..8 {
        call();
    }
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        call();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report(
        &format!("round-trip {query:?} (connect+search+response)"),
        &mut times,
    );
}

/// Compare the parallel per-entry scan vs. the single-buffer memmem on a loaded snapshot.
fn bench_search(snapshot: &str, query: &str) {
    use everyfind::snapshot;

    let loaded = snapshot::load(std::path::Path::new(snapshot)).expect("load snapshot");
    let index = loaded.index;
    let n = index.entries().len();

    // Entry ids sorted ascending by fold_off (built once; a fresh index is already sorted,
    // but a mutated one is not; this models the map the real switch would maintain).
    let mut by_fold_off: Vec<u32> = (0..n as u32).collect();
    by_fold_off.sort_by_key(|&id| index.entries()[id as usize].fold_off);

    // Correctness: both methods must return the same set of matches.
    let mut a = index.search(query, true, false);
    let mut b = index.search_contig(query, false, &by_fold_off);
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "search_contig disagrees with search for {query:?}");
    println!(
        "index {n} entries; query {query:?} -> {} hits (methods agree)",
        a.len()
    );

    let bench = |label: &str, f: &dyn Fn() -> usize| {
        for _ in 0..5 {
            f();
        }
        let mut times = Vec::with_capacity(60);
        for _ in 0..60 {
            let t = Instant::now();
            let _ = f();
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|x, y| x.partial_cmp(y).unwrap());
        report(label, &mut times);
    };
    bench("A parallel per-entry scan (current)", &|| {
        index.search(query, true, false).len()
    });
    bench("B single-buffer memmem (contig)", &|| {
        index.search_contig(query, false, &by_fold_off).len()
    });
}

/// N clients search concurrently while a USN burst (create + delete `files`) is applied.
fn bench_concurrency(clients: usize, files: usize, tempdir: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    let dir = std::path::PathBuf::from(tempdir);
    std::fs::create_dir_all(&dir).expect("create tempdir");

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let errors = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..clients)
        .map(|_| {
            let stop = Arc::clone(&stop);
            let errors = Arc::clone(&errors);
            let requests = Arc::clone(&requests);
            thread::spawn(move || {
                let req = Request::Search {
                    query: "kernel32".into(),
                    case_sensitive: false,
                    include_orphans: false,
                    limit: 5,
                    reserve_elsewhere: 0,
                };
                let mut times = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    match client::request(PIPE_NAME, &req, Duration::from_secs(30)) {
                        Ok(_) => {
                            times.push(t.elapsed().as_secs_f64() * 1000.0);
                            requests.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                times
            })
        })
        .collect();

    // Generate the USN burst: create then delete `files` in the watched dir.
    let t0 = Instant::now();
    for i in 0..files {
        let _ = std::fs::write(dir.join(format!("efload_{i:06}.tmp")), b"x");
    }
    for i in 0..files {
        let _ = std::fs::remove_file(dir.join(format!("efload_{i:06}.tmp")));
    }
    let churn = t0.elapsed();
    // Let the daemon drain the burst (a few poll intervals of the default 1s).
    thread::sleep(Duration::from_secs(4));
    stop.store(true, Ordering::Relaxed);

    let mut all: Vec<f64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "concurrency: {clients} clients, {files} files created+deleted ({} ops) in {churn:.1?}",
        files * 2
    );
    println!(
        "  requests ok = {}, errors = {}",
        requests.load(Ordering::Relaxed),
        errors.load(Ordering::Relaxed),
    );
    report(
        "  client search latency (8 concurrent, during burst)",
        &mut all,
    );
}

fn report(label: &str, times: &mut [f64]) {
    let runs = times.len();
    let pct = |p: usize| times[(runs * p / 100).min(runs - 1)];
    println!(
        "{label}  (n={runs}):\n  min {:.1} / median {:.1} / p90 {:.1} / p95 {:.1} / max {:.1} ms",
        times[0],
        pct(50),
        pct(90),
        pct(95),
        times[runs - 1],
    );
}
