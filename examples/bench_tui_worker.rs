//! M4 latency diagnosis: measure the TUI IPC worker in isolation (no terminal).
//!
//! Splits the `ef -i` key->render budget into "worker round trip" vs "terminal loop": this drives
//! `tui::worker::worker_loop` against a **running daemon** and times send-job -> reply, for single
//! settled queries and for coalesced bursts (simulating brisk typing). If these are ~20 ms then
//! the ~200 ms seen in the TUI is in the terminal loop, not the worker.
//!
//! ```text
//! cargo run --release --example bench_tui_worker
//! ```
//! (a daemon must be running: `efd --foreground --volume C:`)

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use everyfind::tui::worker::{worker_loop, Job, Reply};
use everyfind::tui::Msg;

fn pct(sorted: &[u128], p: f64) -> u128 {
    let i = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[i]
}

fn report(label: &str, mut total: Vec<u128>, mut rt: Vec<u128>) {
    total.sort_unstable();
    rt.sort_unstable();
    println!(
        "{label:<26} n={:<3} send->reply p50={:>4}ms p95={:>4}ms max={:>4}ms | worker rt_ms p50={:>3} max={:>3}",
        total.len(),
        pct(&total, 0.50),
        pct(&total, 0.95),
        *total.last().unwrap(),
        pct(&rt, 0.50),
        *rt.last().unwrap(),
    );
}

fn main() {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (tx, rx) = mpsc::channel::<Msg>();
    thread::spawn(move || {
        worker_loop(
            job_rx,
            tx,
            Duration::from_millis(2000),
            String::new(),
            Default::default(),
        )
    });

    let mut gen = 0u64;
    let recv_reply = |rx: &mpsc::Receiver<Msg>, want: u64| -> u128 {
        loop {
            match rx.recv().unwrap() {
                Msg::Reply(Reply::Search { gen: g, rt_ms, .. }) if g == want => {
                    return rt_ms as u128
                }
                Msg::Reply(Reply::Disconnected(m)) => {
                    eprintln!("daemon unreachable: {m}");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
    };

    // Warm up (first connect / page-in).
    for q in ["warmup", "k"] {
        gen += 1;
        job_tx
            .send(Job::Search {
                gen,
                query: q.into(),
            })
            .unwrap();
        recv_reply(&rx, gen);
    }

    // (1) Single settled query: one job, wait for its reply. Mirrors a keystroke after a pause.
    let queries = [
        "kernel32", "k", "ke", "ker", "e", "a", "system32", "notepad", "dll", "exe",
    ];
    let (mut total, mut rts) = (Vec::new(), Vec::new());
    for round in 0..40 {
        let q = queries[round % queries.len()];
        gen += 1;
        let t0 = Instant::now();
        job_tx
            .send(Job::Search {
                gen,
                query: q.into(),
            })
            .unwrap();
        let rt = recv_reply(&rx, gen);
        total.push(t0.elapsed().as_millis());
        rts.push(rt);
    }
    report("single settled query", total, rts);

    // (2) Coalesced burst: fire 6 jobs back-to-back (like typing "kernel"), then time from the
    // LAST send to the reply for the last gen (the worker coalesces to it).
    let (mut total, mut rts) = (Vec::new(), Vec::new());
    for _ in 0..20 {
        let prefixes = ["k", "ke", "ker", "kern", "kerne", "kernel"];
        let mut last_gen = gen;
        let mut t_last = Instant::now();
        for (i, q) in prefixes.iter().enumerate() {
            gen += 1;
            last_gen = gen;
            if i == prefixes.len() - 1 {
                t_last = Instant::now();
            }
            job_tx
                .send(Job::Search {
                    gen,
                    query: (*q).into(),
                })
                .unwrap();
        }
        let rt = recv_reply(&rx, last_gen);
        total.push(t_last.elapsed().as_millis());
        rts.push(rt);
    }
    report("coalesced burst (6)", total, rts);
}
