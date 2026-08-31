//! The single IPC worker thread. It runs the blocking pipe
//! round trip off the UI thread and **coalesces** a burst of queued jobs to the latest search, so
//! fast typing collapses to one request: strictly one in flight (sequential, no pipeline).
//!
//! Replies are sent back into the shell's merged [`Msg`] channel; the UI thread applies a search
//! reply only if its `gen` is still the latest (stale discard lives in `AppState`).

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::ipc::client::{self, ClientError};
use crate::ipc::{Request, Response, StatusReport, PIPE_NAME};
use crate::tui::{Msg, FETCH_LIMIT};

/// A unit of work from the shell to the worker.
pub enum Job {
    /// Run a search for `query`, tagged with `gen`.
    Search { gen: u64, query: String },
    /// Refresh the daemon status (status line).
    Status,
}

/// A worker result, delivered to the shell via [`Msg::Reply`].
pub enum Reply {
    /// A completed search: the daemon response and its round-trip time.
    Search {
        gen: u64,
        resp: Response,
        rt_ms: u64,
    },
    /// A refreshed status snapshot.
    Status(StatusReport),
    /// A search failed to reach the daemon: fatal for the session (show message, exit 1).
    Disconnected(String),
}

/// The search switches the user asked for on the command line.
///
/// `-s` and `-a` are declared on the shared `SearchArgs`, honoured by the one-shot path and by
/// the daemon, and were dropped on the floor by the TUI: `ef -i -s Kernel32` matched
/// case-insensitively, exactly like `ef -i Kernel32`. They were never passed this far.
#[derive(Clone, Copy, Default)]
pub struct SearchFlags {
    pub case_sensitive: bool,
    pub include_orphans: bool,
}

/// Worker loop: block on a job, drain+coalesce the pending burst (latest search wins; a status
/// stands alone), run at most one search + one status per burst, repeat.
/// `excludes_suffix` (the persistent `!path:` excludes, possibly empty) is appended to every
/// outgoing query; the visible input box stays the user's own text.
pub fn worker_loop(
    job_rx: Receiver<Job>,
    out: Sender<Msg>,
    timeout: Duration,
    excludes_suffix: String,
    how: SearchFlags,
) {
    while let Ok(first) = job_rx.recv() {
        // Collect the burst (the blocked recv woke us; grab everything queued behind it).
        let mut jobs = vec![first];
        while let Ok(j) = job_rx.try_recv() {
            jobs.push(j);
        }

        let mut latest_search: Option<(u64, String)> = None;
        let mut want_status = false;
        for job in jobs {
            match job {
                Job::Search { gen, query } => latest_search = Some((gen, query)),
                Job::Status => want_status = true,
            }
        }

        if let Some((gen, query)) = latest_search {
            let req = Request::Search {
                query: crate::config::compose(&query, &excludes_suffix),
                case_sensitive: how.case_sensitive,
                include_orphans: how.include_orphans,
                limit: FETCH_LIMIT,
                reserve_elsewhere: 0,
            };
            let start = Instant::now();
            match client::request(PIPE_NAME, &req, timeout) {
                Ok(resp) => {
                    let rt_ms = start.elapsed().as_millis() as u64;
                    if out
                        .send(Msg::Reply(Reply::Search { gen, resp, rt_ms }))
                        .is_err()
                    {
                        return; // shell gone
                    }
                }
                Err(e) => {
                    let _ = out.send(Msg::Reply(Reply::Disconnected(describe(e))));
                    return; // fatal: the daemon is unreachable
                }
            }
        }

        if want_status {
            // A status failure is non-fatal: the status line just won't update this tick.
            if let Ok(Response::Status(report)) =
                client::request(PIPE_NAME, &Request::Status, timeout)
            {
                if out.send(Msg::Reply(Reply::Status(report))).is_err() {
                    return;
                }
            }
        }
    }
}

fn describe(e: ClientError) -> String {
    match e {
        ClientError::NotRunning(_) => {
            "Everyfind daemon (efd) is not running. Start it with: efd --foreground".to_string()
        }
        other => format!("daemon connection failed: {other}"),
    }
}
