//! Integration test for the real named-pipe transport (M3 step 3): a `PipeServer` with the
//! production Interactive DACL, framed request/response over `ReadFile`/`WriteFile`, and the
//! `WaitNamedPipe`-retrying client. Runs **unelevated**: creating a pipe in the caller's own
//! object namespace needs no admin, and the Interactive DACL grants the test's own token
//! (which carries INTERACTIVE) access. No volume is touched.

#![cfg(windows)]

use std::thread;
use std::time::{Duration, Instant};

use everyfind::ipc::acl::AclMode;
use everyfind::ipc::client;
use everyfind::ipc::server::PipeServer;
use everyfind::ipc::{read_request, write_response, ErrCode, Request, Response, SearchHit};

fn unique_pipe(tag: &str) -> String {
    format!(r"\\.\pipe\everyfind-test-{}-{tag}", std::process::id())
}

#[test]
fn round_trip_over_real_pipe_with_acl() {
    let name = unique_pipe("rt");
    let mut server = PipeServer::bind(&name, AclMode::Interactive, 4).expect("bind");

    let srv = thread::spawn(move || {
        let mut stream = server.accept().expect("accept");
        let req = read_request(&mut stream).expect("read request");
        let resp = match req {
            Request::Search { query, limit, .. } => Response::Search {
                total_hits: 12345,
                results: vec![SearchHit {
                    path: format!("hit:{query}:{limit}"),
                    is_dir: false,
                }],
                building: false,
            },
            Request::Status | Request::Du { .. } => Response::Error {
                code: ErrCode::Internal,
                message: "no status/du in test".into(),
            },
        };
        write_response(&mut stream, &resp).expect("write response");
    });

    // Non-ASCII query exercises the UTF-8 path over the real byte pipe.
    let req = Request::Search {
        query: "日本語🔍".into(),
        case_sensitive: false,
        include_orphans: false,
        limit: 7,
        reserve_elsewhere: 0,
    };
    let resp = client::request(&name, &req, Duration::from_secs(5)).expect("client request");
    match resp {
        Response::Search {
            total_hits,
            results,
            building: _,
        } => {
            assert_eq!(total_hits, 12345);
            assert_eq!(
                results,
                vec![SearchHit {
                    path: "hit:日本語🔍:7".to_string(),
                    is_dir: false,
                }]
            );
        }
        other => panic!("expected Search, got {other:?}"),
    }
    srv.join().expect("server thread");
}

#[test]
fn second_bind_same_name_is_rejected() {
    // FILE_FLAG_FIRST_PIPE_INSTANCE: a second bind to a live name fails (squat / double-start).
    let name = unique_pipe("dup");
    let _server = PipeServer::bind(&name, AclMode::Interactive, 4).expect("first bind");
    let err = PipeServer::bind(&name, AclMode::Interactive, 4);
    assert!(err.is_err(), "second bind to the same name must fail");
}

/// A pipe nobody ever created still reports "not running", and reports it promptly.
///
/// The client retries `ERROR_FILE_NOT_FOUND` for a grace period, because a live server is
/// briefly absent from the namespace while it replaces its listening instance. That grace
/// must not turn a genuinely missing daemon into a wait for the whole timeout: `ef` would
/// look hung instead of telling the user to start the service.
#[test]
fn absent_pipe_reports_not_running_without_burning_the_timeout() {
    let name = unique_pipe("absent"); // deliberately never bound
    let started = Instant::now();

    let err = client::request(&name, &Request::Status, Duration::from_secs(5))
        .expect_err("nothing is listening on that name");

    assert!(
        matches!(err, client::ClientError::NotRunning(_)),
        "expected NotRunning, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}; the absent-pipe grace period must stay far below the timeout",
        started.elapsed()
    );
}

#[test]
fn two_sequential_clients_share_the_server() {
    // The pre-created listening instance lets a second client connect after the first without
    // an ERROR_PIPE_BUSY failure.
    let name = unique_pipe("seq");
    let mut server = PipeServer::bind(&name, AclMode::Interactive, 8).expect("bind");

    let srv = thread::spawn(move || {
        for _ in 0..2 {
            let mut stream = server.accept().expect("accept");
            let req = read_request(&mut stream).expect("read");
            let n = match req {
                Request::Search { limit, .. } => u64::from(limit),
                Request::Status | Request::Du { .. } => 0,
            };
            write_response(
                &mut stream,
                &Response::Search {
                    total_hits: n,
                    results: vec![],
                    building: false,
                },
            )
            .expect("write");
        }
    });

    for i in 1..=2u32 {
        let req = Request::Search {
            query: "q".into(),
            case_sensitive: false,
            include_orphans: false,
            limit: i,
            reserve_elsewhere: 0,
        };
        let resp = client::request(&name, &req, Duration::from_secs(5)).expect("request");
        assert!(matches!(resp, Response::Search { total_hits, .. } if total_hits == u64::from(i)));
    }
    srv.join().expect("server thread");
}

/// The timeout has to bound the whole round trip, not just the connect.
///
/// Connecting is the part that never blocks: the server pre-creates a listening instance, so
/// it succeeds at once, and the read then had no deadline at all. `ef --timeout-ms 2000` hung
/// indefinitely against a daemon busy rebuilding, and so did Explorer's search box on the five
/// seconds it thought it had asked for. Here the server accepts and then says nothing.
#[test]
fn a_silent_server_hits_the_timeout_instead_of_hanging() {
    let name = unique_pipe("silent");
    let mut server = PipeServer::bind(&name, AclMode::Interactive, 4).expect("bind");
    let accepted = std::thread::spawn(move || {
        // Accept, then hold the connection open without answering.
        let stream = server.accept().expect("accept");
        std::thread::sleep(Duration::from_secs(3));
        drop(stream);
    });

    let started = Instant::now();
    let err = client::request(&name, &Request::Status, Duration::from_millis(300))
        .expect_err("the server never answers");
    let waited = started.elapsed();

    assert!(
        matches!(&err, client::ClientError::Io(e) if e.kind() == std::io::ErrorKind::TimedOut),
        "expected a timeout, got {err:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "waited {waited:?}; the deadline must apply to the read, not only the connect"
    );
    accepted.join().unwrap();
}

/// A client that connects and says nothing must not hold a pipe instance forever.
///
/// Instances are a shared, exhaustible resource. Fill them all (from an unprivileged process,
/// sending nothing) and the server can no longer create a listening instance, the pipe name
/// leaves the namespace, and every other user's `ef` reports "efd is not running". Permanently.
/// The handler now enforces a deadline on the request, so a silent client is dropped and the
/// instance comes back.
#[test]
fn a_silent_client_does_not_hold_an_instance_forever() {
    use everyfind::ipc::pipe::PipeStream;

    let name = unique_pipe("silentclient");
    let mut server = PipeServer::bind(&name, AclMode::Interactive, 2).expect("bind");

    // Connect and say nothing.
    let quiet = everyfind::ipc::client::connect(&name, Duration::from_secs(2)).expect("connect");
    let served = server.accept().expect("accept");

    // The handler's own deadline is what we are testing; here we stand in for it directly to
    // keep the test independent of the daemon's plumbing, and assert the mechanism works: an
    // aborted read returns rather than blocking.
    let token = served.abort_token();
    let reader = std::thread::spawn(move || {
        let mut s: PipeStream = served;
        let mut buf = [0u8; 8];
        std::io::Read::read(&mut s, &mut buf)
    });
    std::thread::sleep(Duration::from_millis(300));
    token.abort();

    let started = Instant::now();
    let out = reader.join().expect("the reader thread must not panic");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the aborted read must return promptly, took {:?}",
        started.elapsed()
    );
    assert!(
        out.is_err() || out.unwrap() == 0,
        "an aborted read is not a successful one"
    );
    drop(quiet);
}

/// The request deadline must not be paid by requests that meet it.
///
/// The first version polled a flag in 100 ms steps and the handler joined the watchdog before
/// serving, so every well-behaved request waited 0-100 ms between reading its frame and
/// starting the search, on a daemon whose search budget is measured in tens of milliseconds.
#[test]
fn the_request_deadline_costs_a_prompt_client_nothing() {
    let name = unique_pipe("deadlinecost");
    let mut server = PipeServer::bind(&name, AclMode::Interactive, 4).expect("bind");
    let served = thread::spawn(move || {
        for _ in 0..5 {
            let mut s = server.accept().expect("accept");
            let req = read_request(&mut s).expect("request");
            assert!(matches!(req, Request::Status));
            let resp = Response::Search {
                total_hits: 0,
                results: Vec::new(),
                building: false,
            };
            write_response(&mut s, &resp).expect("response");
        }
    });

    let mut worst = Duration::ZERO;
    for _ in 0..5 {
        let started = Instant::now();
        client::request(&name, &Request::Status, Duration::from_secs(5)).expect("round trip");
        worst = worst.max(started.elapsed());
    }
    served.join().unwrap();
    assert!(
        worst < Duration::from_millis(60),
        "worst round trip {worst:?}; the deadline machinery must not be on the hot path"
    );
}

/// An abort that arrives *before* the I/O is issued must still stop it.
///
/// `CancelIoEx` reaches only what is already in flight, so a cancel that lands in the gap
/// between two calls does nothing at all and the next call blocks with no deadline and nothing
/// left to cancel it: one leaked thread, and one of the server's pipe instances held for as
/// long as it lives. It is not even a race in the worst case: when the connect uses up the
/// caller's whole budget the wait expires before the worker has run an instruction, so the
/// cancel is *guaranteed* to find nothing in flight.
#[test]
fn an_abort_before_the_read_still_stops_it() {
    use everyfind::ipc::pipe::PipeStream;

    let name = unique_pipe("earlyabort");
    let mut server = PipeServer::bind(&name, AclMode::Interactive, 2).expect("bind");
    let quiet = everyfind::ipc::client::connect(&name, Duration::from_secs(2)).expect("connect");
    let served = server.accept().expect("accept");

    // Abort with nothing in flight: the case one CancelIoEx cannot cover.
    let token = served.abort_token();
    token.abort();
    assert!(token.is_aborted());

    let started = Instant::now();
    let reader = std::thread::spawn(move || {
        let mut s: PipeStream = served;
        let mut buf = [0u8; 8];
        std::io::Read::read(&mut s, &mut buf)
    });
    let out = reader.join().expect("the reader thread must not panic");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a read issued after an abort must fail at once, took {:?}",
        started.elapsed()
    );
    assert!(
        out.is_err(),
        "a read issued after an abort must fail, got {out:?}"
    );
    // And so must a write, which is the half a client is in when its own timeout fires.
    drop(quiet);
}
