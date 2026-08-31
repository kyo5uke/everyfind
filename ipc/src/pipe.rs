//! Low-level named-pipe transport shared by [`super::server`] and [`super::client`].
//!
//! [`PipeStream`] wraps a connected pipe handle and implements [`std::io::Read`] /
//! [`std::io::Write`] so the framing functions in [`super`] work over a real pipe unchanged.
//! FFI constants are defined locally (stable documented values) to keep the `windows-sys`
//! surface minimal, matching `examples/probe.rs`.

use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::CancelIoEx;

// --- pipe / access / error constants ---
pub(crate) const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
pub(crate) const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
pub(crate) const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
pub(crate) const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
pub(crate) const PIPE_WAIT: u32 = 0x0000_0000;
pub(crate) const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
pub(crate) const OPEN_EXISTING: u32 = 3;

/// The client's desired access (P14(v), locked): read the response + write the request, and
/// nothing the no-append INTERACTIVE mask (`0x12018b`) withholds. **Never `GENERIC_WRITE`**:
/// it maps to `FILE_APPEND_DATA` = `FILE_CREATE_PIPE_INSTANCE`, which the DACL denies.
pub const CLIENT_PIPE_ACCESS: u32 = 0x0012_008b; // FILE_GENERIC_READ | FILE_WRITE_DATA

pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
pub(crate) const ERROR_BROKEN_PIPE: u32 = 109;
/// What a `CancelIoEx`'d call returns. Reported for I/O issued *after* an abort too, so the
/// two are indistinguishable to a caller, which is the point.
pub(crate) const ERROR_OPERATION_ABORTED: u32 = 995;
pub(crate) const ERROR_PIPE_BUSY: u32 = 231;
pub(crate) const ERROR_PIPE_CONNECTED: u32 = 535;

/// Encode a `&str` as a NUL-terminated wide string for the Win32 `*W` APIs.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The kernel handle behind a [`PipeStream`], closed exactly once when the last owner drops.
///
/// Shared rather than owned outright so that an [`AbortToken`] cannot outlive the handle it
/// cancels. Without that, a caller that gave up waiting could call `CancelIoEx` in the instant
/// after the worker finished and closed it, on a handle number the kernel may already have
/// handed to something else.
struct OwnedHandle(HANDLE);

// SAFETY: a pipe handle is a kernel object identified by an integer; the value is meaningful in
// any thread of this process, and `OwnedHandle` exposes no operation on it, only `AbortToken`
// does, and `CancelIoEx` is documented as callable from any thread. Ownership is what this type
// carries: the handle is closed exactly once, when the last share drops.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own this handle and, by construction, nothing else holds it.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A connected named-pipe endpoint. Owns the handle and closes it on drop.
pub struct PipeStream {
    handle: HANDLE,
    /// Keeps the handle alive for as long as any [`AbortToken`] does.
    owner: std::sync::Arc<OwnedHandle>,
    /// Set by [`AbortToken::abort`]; checked before every read and write. See [`AbortToken`].
    aborted: std::sync::Arc<AtomicBool>,
}

// SAFETY: a pipe handle is a kernel object owned exclusively by this stream; transferring
// that ownership to another thread (e.g. a per-connection worker) is sound. It is not `Sync`
// (no `&`-shared concurrent I/O).
unsafe impl Send for PipeStream {}

impl PipeStream {
    /// Wrap an already-connected pipe handle.
    pub(crate) fn from_handle(handle: HANDLE) -> Self {
        Self {
            handle,
            owner: std::sync::Arc::new(OwnedHandle(handle)),
            aborted: std::sync::Arc::new(AtomicBool::new(false)),
        }
    }

    /// `Err(ERROR_OPERATION_ABORTED)` once this stream has been aborted, the same error a
    /// cancelled `ReadFile` returns, so callers need no second case for it.
    fn check_aborted(&self) -> io::Result<()> {
        if self.aborted.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED as i32));
        }
        Ok(())
    }

    /// A token another thread can use to abort this stream's blocking I/O.
    ///
    /// The read is a blocking `ReadFile` on a `PIPE_WAIT` handle with no deadline, which is
    /// fine against a client that speaks and fatal against one that does not: a handler blocked
    /// forever holds a pipe instance, and enough of them hold all of them. The server then
    /// cannot create a listening instance, the pipe name leaves the namespace, and every other
    /// user is told "efd is not running", permanently, from an unprivileged process. A
    /// deadline needs somebody outside the blocked thread to enforce it, and this is the handle
    /// they enforce it with.
    pub fn abort_token(&self) -> AbortToken {
        AbortToken(
            std::sync::Arc::clone(&self.owner),
            std::sync::Arc::clone(&self.aborted),
        )
    }
}

/// Aborts a [`PipeStream`]'s I/O from another thread. See [`PipeStream::abort_token`].
///
/// Carries a flag as well as the handle, because `CancelIoEx` alone cancels only what is
/// *already* in flight. A cancel that lands in the gap between one call returning and the next
/// being issued does nothing at all, and the call issued a microsecond later then blocks with
/// no deadline and nothing left to cancel it: the exact leak the token exists to prevent,
/// reached by losing a race instead of by not trying. Worse, it is not even a race in the
/// common case: when the connect used up the caller's whole budget the wait expires before the
/// worker has run its first instruction, so the cancel is *guaranteed* to find nothing.
///
/// The flag closes that: it is set before the cancel and checked before every read and write,
/// so I/O issued after an abort fails immediately instead of blocking.
#[derive(Clone)]
pub struct AbortToken(std::sync::Arc<OwnedHandle>, std::sync::Arc<AtomicBool>);

// SAFETY: the token holds a share of the handle's ownership, so the handle stays open for as
// long as the token exists; it can never name a closed or recycled one, which is what made the
// first version's "the watchdog is joined first" argument load-bearing and fragile.
// `CancelIoEx` is documented as callable from any thread for a handle the process owns, and
// cancelling I/O that has already completed is a no-op returning ERROR_NOT_FOUND.
unsafe impl Send for AbortToken {}
unsafe impl Sync for AbortToken {}

impl AbortToken {
    /// Cancel any blocking read or write on the stream. The blocked call fails with
    /// `ERROR_OPERATION_ABORTED`, which every caller treats as a dead connection.
    pub fn abort(&self) {
        // Flag first, then cancel: I/O issued after this point must find the flag already set,
        // or it is the one that blocks forever.
        self.1.store(true, Ordering::SeqCst);
        // SAFETY: a live handle this process owns; a null overlapped cancels every request on it.
        unsafe { CancelIoEx(self.0 .0, std::ptr::null()) };
    }

    /// Whether [`abort`](Self::abort) has been called.
    pub fn is_aborted(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check_aborted()?;
        let mut read = 0u32;
        // SAFETY: valid handle; `buf` is writable for `buf.len()` bytes.
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            // A closed peer surfaces as a broken pipe; report EOF, as a socket would.
            if e == ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        Ok(read as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check_aborted()?;
        let mut written = 0u32;
        // SAFETY: valid handle; `buf` is readable for `buf.len()` bytes.
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // WriteFile already delivers to the pipe buffer; the reader sees it without an
        // explicit flush. (FlushFileBuffers would block until the peer drains, which is unwanted.)
        Ok(())
    }
}

/// A raw `c_void` pointer alias for readability at the FFI boundary.
pub(crate) type Sd = *mut c_void;
