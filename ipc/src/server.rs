//! Named-pipe server: create instances with the chosen DACL, reject remote clients, detect a
//! squatter at startup, and hand each connected instance to the caller as a [`PipeStream`].
//!
//! The daemon (`src/daemon.rs`) calls [`PipeServer::accept`] in a loop, spawning a worker per
//! returned stream. A byte-mode pipe (`PIPE_TYPE_BYTE | PIPE_READMODE_BYTE`) with
//! `PIPE_REJECT_REMOTE_CLIENTS`; instance #1 is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`
//! so a pre-existing squatter (or a second `efd`) makes `bind` fail loudly (P14(vi)).

use std::io;

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};

use super::acl::{self, AclMode};
use super::pipe::{
    wide, PipeStream, Sd, ERROR_PIPE_CONNECTED, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

const PIPE_BUFFER_SIZE: u32 = 8 * 1024;

/// A listening named-pipe server bound to a single name.
pub struct PipeServer {
    name: Vec<u16>,
    sd: Sd,
    max_instances: u32,
    /// A pre-created listening instance waiting for the next client (reduces the busy window).
    listening: Option<HANDLE>,
}

// SAFETY: the fields are a wide-string buffer, a process-lifetime SD pointer, and a listening
// pipe handle, all safe to move to the accept thread. Not `Sync` (single-owner accept loop).
unsafe impl Send for PipeServer {}

impl PipeServer {
    /// Bind the server to `pipe_name` (e.g. `\\.\pipe\everyfind`) with `mode`'s DACL, allowing
    /// up to `max_instances` concurrent instances. Creates instance #1 with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`; if that fails the name is already held; abort.
    pub fn bind(pipe_name: &str, mode: AclMode, max_instances: u32) -> Result<Self> {
        let sd = acl::build_sd(mode).context("building the pipe security descriptor")?;
        let mut server = Self {
            name: wide(pipe_name),
            sd,
            max_instances,
            listening: None,
        };
        let first = server.create_instance(true).with_context(|| {
            format!(
                "creating the first pipe instance for {pipe_name} \
                 (is another efd, or a squatter, already holding the name?)"
            )
        })?;
        server.listening = Some(first);
        Ok(server)
    }

    /// Block until a client connects, returning the connected [`PipeStream`]. A fresh listening
    /// instance is pre-created for the next client before returning.
    pub fn accept(&mut self) -> io::Result<PipeStream> {
        let handle = match self.listening.take() {
            Some(h) => h,
            None => self.create_instance(false).map_err(into_io)?,
        };

        // SAFETY: `handle` is a valid listening pipe instance.
        let ok = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            // A client that connected between create and ConnectNamedPipe is already attached.
            if e != ERROR_PIPE_CONNECTED {
                // SAFETY: we own `handle`; close it before surfacing the error.
                unsafe { CloseHandle(handle) };
                return Err(io::Error::from_raw_os_error(e as i32));
            }
        }

        // Pre-create the next listening instance so a client arriving in the gap is not turned
        // away with ERROR_PIPE_BUSY. Best-effort: on failure the next accept() recreates it.
        self.listening = self.create_instance(false).ok();

        Ok(PipeStream::from_handle(handle))
    }

    /// Create one named-pipe instance. `first` sets `FILE_FLAG_FIRST_PIPE_INSTANCE`.
    fn create_instance(&self, first: bool) -> Result<HANDLE> {
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let pipe_mode =
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd,
            bInheritHandle: 0,
        };

        // SAFETY: valid wide name and SECURITY_ATTRIBUTES; buffer sizes are constants.
        let h = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                open_mode,
                pipe_mode,
                self.max_instances,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(anyhow::Error::from(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32,
            )))
        } else {
            Ok(h)
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        if let Some(h) = self.listening.take() {
            // SAFETY: we own the listening handle and it is not used after drop.
            unsafe { CloseHandle(h) };
        }
    }
}

fn into_io(e: anyhow::Error) -> io::Error {
    e.downcast::<io::Error>()
        .unwrap_or_else(|e| io::Error::other(e.to_string()))
}
