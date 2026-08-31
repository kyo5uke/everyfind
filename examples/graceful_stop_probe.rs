//! Live verification of the `efd --foreground` graceful stop (M3 acceptance): spawn `efd` in a
//! new process group, wait until it is serving, deliver a **Ctrl-Break** (the reliable console
//! signal; the handler keeps the process alive to save, unlike Ctrl-Close), and confirm it
//! wrote a fresh snapshot and exited. This exercises the exact `SetConsoleCtrlHandler` ->
//! `request_shutdown` -> final-save path (the same save the service STOP uses).
//!
//! ```text
//! cargo run --release --example graceful_stop_probe -- C: %TEMP%\ef-graceful.snap
//! ```
//! Requires an elevated terminal (efd opens `\\.\C:`).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use everyfind::ipc::client;
use everyfind::ipc::{Request, PIPE_NAME};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CTRL_BREAK_EVENT: u32 = 1;

extern "system" {
    fn GenerateConsoleCtrlEvent(dwctrlevent: u32, dwprocessgroupid: u32) -> i32;
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let volume = args.first().cloned().unwrap_or_else(|| "C:".into());
    let snap = args.get(1).cloned().unwrap_or_else(|| {
        std::env::temp_dir()
            .join("ef-graceful.snap")
            .to_string_lossy()
            .into()
    });

    let efd = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("efd.exe"))
        })
        .expect("locate efd.exe");

    println!(
        "spawning: {} --foreground --volume {volume} --snapshot {snap}",
        efd.display()
    );
    let mut child = Command::new(&efd)
        .args(["--foreground", "--volume", &volume, "--snapshot", &snap])
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .expect("spawn efd");

    // Wait until the daemon answers on the pipe (index acquired + listening).
    let ready_deadline = Instant::now() + Duration::from_secs(120);
    let mut ready = false;
    while Instant::now() < ready_deadline {
        if client::request(PIPE_NAME, &Request::Status, Duration::from_millis(500)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(ready, "efd never became ready");
    println!("daemon is serving.");

    let before = snapshot_mtime(&snap);

    // Deliver Ctrl-Break to the child's process group.
    println!("sending Ctrl-Break to process group {} ...", child.id());
    // SAFETY: FFI call with the child's process-group id (it is the group leader).
    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    assert!(ok != 0, "GenerateConsoleCtrlEvent failed");

    // Wait for the child to exit (it should save then quit).
    let t0 = Instant::now();
    let stop_deadline = t0 + Duration::from_secs(30);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                println!("efd exited with {status} after {:?}", t0.elapsed());
                break;
            }
            None if Instant::now() >= stop_deadline => {
                let _ = child.kill();
                panic!("efd did not exit within 30s of Ctrl-Break");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    let after = snapshot_mtime(&snap);
    let saved = match (before, after) {
        (None, Some(_)) => true,
        (Some(b), Some(a)) => a > b,
        _ => false,
    };
    println!("snapshot written on graceful stop: {saved}");
    assert!(saved, "expected a fresh snapshot after the graceful stop");
    println!("PASS: Ctrl-Break -> graceful save -> exit");
}

fn snapshot_mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(Path::new(path))
        .ok()
        .and_then(|m| m.modified().ok())
}
