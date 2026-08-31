//! Named-pipe client: connect (retrying on a busy pipe via `WaitNamedPipeW`) and run one
//! request/response round trip. This is the `ef` hot path: no logging, no allocation beyond
//! the single frame each way.

use std::io;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

use super::pipe::{
    wide, PipeStream, CLIENT_PIPE_ACCESS, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, OPEN_EXISTING,
};
use super::{read_response, write_request, Request, Response, WireError};

/// How long `ERROR_FILE_NOT_FOUND` is read as "the server is between listening instances"
/// rather than "there is no server".
///
/// The two are indistinguishable at the call: a pipe whose instances are all connected is
/// absent from the namespace until the server creates the next listening one, and a
/// `CreateFileW` landing in that gap fails with the same code a never-created pipe returns.
/// The gap is microseconds, but it is hit often enough to matter (measured against a live
/// server: about one connect in ten, which is why
/// `tests/ipc_pipe.rs::two_sequential_clients_share_the_server` was intermittently red).
/// A daemon that really is not running still reports so, just this much later.
const ABSENT_GRACE: Duration = Duration::from_millis(250);

/// Pause between retries inside [`ABSENT_GRACE`]. The gap it covers is far shorter than this,
/// so one pause is normally enough and the loop stays cheap.
const RETRY_PAUSE: Duration = Duration::from_millis(2);

/// Connect to `pipe_name`, retrying with `WaitNamedPipeW` while the pipe is busy until
/// `timeout` elapses. Opens with [`CLIENT_PIPE_ACCESS`] (`0x12008b`), never `GENERIC_WRITE`.
pub fn connect(pipe_name: &str, timeout: Duration) -> io::Result<PipeStream> {
    let wname = wide(pipe_name);
    let deadline = Instant::now() + timeout;
    let absent_after = Instant::now() + ABSENT_GRACE.min(timeout);
    loop {
        // SAFETY: valid NUL-terminated wide name; exclusive share; OPEN_EXISTING.
        let h = unsafe {
            CreateFileW(
                wname.as_ptr(),
                CLIENT_PIPE_ACCESS,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(PipeStream::from_handle(h));
        }

        let e = unsafe { GetLastError() };
        // Absent, but perhaps only for this instant; see `ABSENT_GRACE`. Retry briefly
        // before believing it, so a request that arrives while the server is replacing its
        // listening instance is not reported as "the daemon is not running".
        if e == ERROR_FILE_NOT_FOUND && Instant::now() < absent_after {
            std::thread::sleep(RETRY_PAUSE);
            continue;
        }
        if e != ERROR_PIPE_BUSY {
            // Neither busy nor a momentary gap: a real failure.
            return Err(io::Error::from_raw_os_error(e as i32));
        }

        // Busy: all instances are in use. Wait for one to free up, bounded by the deadline.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "pipe busy: no free instance before timeout",
            ));
        }
        let wait_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: valid wide name; a nonzero timeout in ms.
        let _ = unsafe { WaitNamedPipeW(wname.as_ptr(), wait_ms) };
        // Loop and retry CreateFileW; the deadline check above bounds the total wait.
    }
}

/// Errors from a client round trip.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon's pipe does not exist; it is probably not running.
    #[error("efd is not running (pipe {0} not found)")]
    NotRunning(String),
    /// Transport / connect failure.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Framing / protocol failure.
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Connect, send `req`, and read one [`Response`], all within `timeout`.
///
/// The whole round trip, not just the connect. It used to be just the connect, and the connect
/// is the part that never blocks: a listening instance is always pre-created, so it succeeds
/// immediately and then the read waited with no deadline at all. Every caller and the CLI's own
/// help called this a response timeout: `ef --timeout-ms 2000` would hang indefinitely against
/// a daemon busy rebuilding its index, and so would Explorer's search box, five seconds being
/// what it thought it had asked for.
///
/// The round trip runs on a worker thread so the wait can be abandoned. A pipe handle whose
/// reader is gone is closed by the worker when it finally returns, and the daemon's handler
/// sees a broken pipe, the same thing it sees when a client is killed, which it already
/// tolerates.
pub fn request(pipe_name: &str, req: &Request, timeout: Duration) -> Result<Response, ClientError> {
    let started = std::time::Instant::now();
    let mut stream = match connect(pipe_name, timeout) {
        Ok(s) => s,
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
            return Err(ClientError::NotRunning(pipe_name.to_string()));
        }
        Err(e) => return Err(ClientError::Io(e)),
    };

    // Taken before the stream moves to the worker. Giving up on the wait has to also stop the
    // work: without this the worker stayed blocked in `read_response` holding one of the
    // daemon's sixty-four pipe instances, and a long-lived client that times out repeatedly
    // (the Explorer shim, the TUI worker) exhausted them; the pipe then leaves the namespace
    // and everyone is told "efd is not running". The same failure the daemon's own request
    // deadline exists to prevent, reached from the other end.
    let abort = stream.abort_token();
    let left = timeout.saturating_sub(started.elapsed());
    let (tx, rx) = std::sync::mpsc::channel();
    let req = req.clone();
    std::thread::Builder::new()
        .name("ef-ipc".into())
        .spawn(move || {
            let out = write_request(&mut stream, &req)
                .map_err(ClientError::from)
                .and_then(|()| read_response(&mut stream).map_err(ClientError::from));
            // The receiver may be gone; dropping `stream` here is what tells the daemon.
            let _ = tx.send(out);
        })
        .map_err(ClientError::Io)?;

    match rx.recv_timeout(left) {
        Ok(result) => result,
        Err(_) => {
            // The abort has to actually take, not merely be attempted. `CancelIoEx` reaches
            // only I/O that is already in flight; the flag inside the token covers I/O issued
            // after it, and this loop covers the sliver between a worker's check of that flag
            // and its syscall. Bounded, and only ever reached on a request that has already
            // failed: a tenth of a second at the end of a wait measured in seconds.
            for _ in 0..ABORT_TRIES {
                abort.abort();
                if rx.recv_timeout(ABORT_POLL).is_ok() {
                    break;
                }
            }
            Err(ClientError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the daemon did not answer within {} ms",
                    timeout.as_millis()
                ),
            )))
        }
    }
}

/// How many times a timed-out round trip re-cancels before giving up on being tidy. The
/// worker is a write and a read away from noticing; more than a couple of passes means it is
/// wedged somewhere no cancel reaches, and waiting longer helps nobody.
const ABORT_TRIES: usize = 5;

/// How long to give the worker to notice each cancel.
const ABORT_POLL: Duration = Duration::from_millis(20);
