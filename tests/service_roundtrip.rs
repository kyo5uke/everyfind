//! Windows service install/uninstall round-trip (M3 step 6). **`#[ignore]` by default**: it
//! touches the SCM + registry + `%ProgramData%`, needs an **elevated** terminal, and takes
//! ~30 s (it enumerates C:). Run it explicitly before trusting the service in daily use:
//!
//! ```text
//! cargo test --test service_roundtrip -- --ignored --nocapture --test-threads=1
//! ```
//!
//! It proves: install registers the service + creates the secured data dir; a STOP is honored
//! **mid-enumeration** (not just after ~30 s); a second start reaches Running and serves a
//! query over the pipe; stop writes a snapshot; and uninstall removes the service **and** the
//! data directory: no trace.

#![cfg(windows)]

use std::time::{Duration, Instant};

use everyfind::ipc::acl::AclMode;
use everyfind::ipc::client;
use everyfind::ipc::{Request, Response, PIPE_NAME};
use everyfind::service;

/// Path to the built `efd.exe` (Cargo sets this for integration tests). `service::install`
/// would look next to the *test* binary, so we install with the real daemon path.
const EFD_EXE: &str = env!("CARGO_BIN_EXE_efd");

#[test]
#[ignore = "touches SCM/registry/ProgramData; elevated; ~30s. Run with --ignored --test-threads=1"]
fn service_lifecycle_roundtrip() {
    // Clean slate in case a previous run left the service behind.
    let _ = service::uninstall();
    assert!(
        !service::is_installed(),
        "precondition: service not installed"
    );

    // --- install ---
    service::install_with_exe(EFD_EXE.as_ref(), "C:", AclMode::Interactive)
        .expect("install should succeed (elevated?)");
    assert!(service::is_installed(), "service registered");
    assert!(service::data_dir().exists(), "secured data dir created");

    // --- STOP during enumeration (refinement #4) ---
    // First start has no snapshot -> a ~30 s C: enumeration begins. Stop a couple seconds in and
    // require it to actually stop promptly (the enum cancel flag, not a 30 s wait).
    service::start().expect("start #1");
    std::thread::sleep(Duration::from_secs(2)); // land inside the enumeration
    let t0 = Instant::now();
    service::stop().expect("stop during enumeration");
    let stop_elapsed = t0.elapsed();
    assert!(
        service::wait_until_stopped(Duration::from_secs(10)),
        "stopped after mid-enum STOP"
    );
    assert!(
        stop_elapsed < Duration::from_secs(20),
        "stop during enumeration took {stop_elapsed:?}; cancellation not honored"
    );

    // --- second start reaches Running and serves a query ---
    service::start().expect("start #2");
    assert!(
        service::wait_until_running(Duration::from_secs(120)),
        "service should reach Running (enumeration completes)"
    );
    match client::request(PIPE_NAME, &Request::Status, Duration::from_secs(5)) {
        Ok(Response::Status(s)) => assert!(s.entries > 0, "status reports a populated index"),
        other => panic!("expected a Status response from the service, got {other:?}"),
    }

    // --- stop writes a snapshot ---
    service::stop().expect("stop #2");
    assert!(
        service::wait_until_stopped(Duration::from_secs(90)),
        "service stops (after the final save)"
    );
    assert!(
        service::default_snapshot_path().exists(),
        "a snapshot is written on stop"
    );

    // --- uninstall removes the service AND the data directory (no trace) ---
    service::uninstall().expect("uninstall");
    assert!(!service::is_installed(), "service registration removed");
    assert!(
        !service::data_dir().exists(),
        "data directory removed (no trace)"
    );
}
