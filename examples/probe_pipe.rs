//! Everyfind M3 probe P14: named-pipe DACL / squatting / remote rejection.
//!
//! Verifies the pipe access-control mechanics the daemon depends on,
//! before the real server is written. Run in an ELEVATED terminal:
//!
//! ```text
//! cargo run --example probe_pipe
//! ```
//!
//! Questions:
//!   (i)   default DACL of a NULL-security-descriptor pipe (what the OS grants)
//!   (ii)  our SDDL (interactive / admins) applies; read the DACL back and confirm
//!   (iii) a local client connects, and (v) the SHRUNKEN IU mask (0x12018b, no
//!         FILE_APPEND_DATA) still permits read/write, while GENERIC_WRITE (which maps to
//!         FILE_APPEND_DATA) is DENIED (a concrete `ef` client requirement)
//!   (iv)  PIPE_REJECT_REMOTE_CLIENTS rejects a `\\<host>\pipe\..` connection
//!   (vi)  a token matching only the IU ACE cannot create another instance (squatting), and
//!         FILE_FLAG_FIRST_PIPE_INSTANCE makes the server detect a pre-existing squatter
//!
//! Read-only w.r.t. any volume: this only creates our own pipes in the object namespace.
//! Note: allocations from Convert*/GetSecurityInfo are intentionally leaked; the probe is a
//! one-shot process that exits immediately.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};

// --- CreateFile / pipe / access constants (stable documented values, defined locally to
// keep the FFI surface minimal, matching examples/probe.rs). ---
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_WRITE_DATA: u32 = 0x0000_0002;
/// What a correct pipe client must request: read the response + write the request, nothing
/// that the shrunken IU mask withholds (no APPEND, no WRITE_EA).
const CLIENT_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA; // 0x0012008b
const OPEN_EXISTING: u32 = 3;

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_PIPE_CONNECTED: u32 = 535;

const SDDL_REVISION_1: u32 = 1;
const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
const GROUP_SECURITY_INFORMATION: u32 = 0x0000_0002;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const SE_KERNEL_OBJECT: i32 = 6;

/// The production interactive-mode DACL: SYSTEM + Administrators full; INTERACTIVE gets
/// the explicit no-append mask 0x12018b.
const SDDL_INTERACTIVE: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12018b;;;IU)";
/// The admins-mode DACL.
const SDDL_ADMINS: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";
/// Isolates the INTERACTIVE grant: only the shrunken IU ACE, no SYSTEM/Administrators. A
/// principal matching this (a non-admin interactive user, or us via our token's IU) gets
/// exactly the rights a real non-elevated `ef` would, used to prove (v)/(vi) in-process.
const SDDL_IU_ONLY: &str = "D:(A;;0x12018b;;;IU)";

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Read a NUL-terminated wide string into an owned `String`.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated UTF-16 buffer.
unsafe fn wstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0isize;
    while *p.offset(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len as usize))
}

/// Build a self-relative SECURITY_DESCRIPTOR from an SDDL string. Leaks the allocation.
fn sd_from_sddl(sddl: &str) -> Option<*mut c_void> {
    let w = wide(sddl);
    let mut psd: *mut c_void = ptr::null_mut();
    // SAFETY: `w` is a valid NUL-terminated wide string; `psd` receives the allocated SD.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &mut psd,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        println!("  ConvertStringSD FAILED for {sddl:?}, err {}", unsafe {
            GetLastError()
        });
        None
    } else {
        Some(psd)
    }
}

/// Read the OWNER+GROUP+DACL of an open handle back as an SDDL string.
fn read_back_sddl(h: *mut c_void) -> Option<String> {
    let mut psd: *mut c_void = ptr::null_mut();
    let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: valid handle; only ppSecurityDescriptor is requested (others null).
    let rc = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            info,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut psd,
        )
    };
    if rc != 0 {
        println!("  GetSecurityInfo FAILED, rc {rc}");
        return None;
    }
    let mut sddl_ptr: *mut u16 = ptr::null_mut();
    // SAFETY: `psd` is a valid SD from GetSecurityInfo; `sddl_ptr` receives an allocated string.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            psd,
            SDDL_REVISION_1,
            info,
            &mut sddl_ptr,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        println!("  ConvertSDToString FAILED, err {}", unsafe {
            GetLastError()
        });
        return None;
    }
    // SAFETY: on success `sddl_ptr` is a NUL-terminated wide string.
    Some(unsafe { wstr_to_string(sddl_ptr) })
}

/// Create a named-pipe server instance. `sddl = None` passes a NULL security descriptor
/// (OS default DACL). Returns the handle or the `GetLastError` code.
fn create_pipe(
    name: &str,
    sddl: Option<&str>,
    first_instance: bool,
    reject_remote: bool,
    max_instances: u32,
) -> Result<*mut c_void, u32> {
    let wname = wide(name);
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first_instance {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let mut pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT;
    if reject_remote {
        pipe_mode |= PIPE_REJECT_REMOTE_CLIENTS;
    }

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    };
    let sa_ptr: *const SECURITY_ATTRIBUTES = match sddl {
        Some(s) => match sd_from_sddl(s) {
            Some(psd) => {
                sa.lpSecurityDescriptor = psd;
                &sa
            }
            None => return Err(unsafe { GetLastError() }),
        },
        None => ptr::null(),
    };

    // SAFETY: valid wide name and (optional) SECURITY_ATTRIBUTES; buffer sizes are constants.
    let h = unsafe {
        CreateNamedPipeW(
            wname.as_ptr(),
            open_mode,
            pipe_mode,
            max_instances,
            4096,
            4096,
            0,
            sa_ptr,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(h)
    }
}

/// Open a pipe as a client (CreateFileW) with the given desired access. Returns the handle
/// or the `GetLastError` code.
fn open_client(path: &str, access: u32) -> Result<*mut c_void, u32> {
    let wp = wide(path);
    // SAFETY: valid NUL-terminated wide path; share mode 0 (exclusive), OPEN_EXISTING.
    let h = unsafe {
        CreateFileW(
            wp.as_ptr(),
            access,
            0,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(h)
    }
}

fn err_name(e: u32) -> &'static str {
    match e {
        0 => "SUCCESS",
        ERROR_ACCESS_DENIED => "ERROR_ACCESS_DENIED(5)",
        53 => "ERROR_BAD_NETPATH(53)",
        ERROR_PIPE_BUSY => "ERROR_PIPE_BUSY(231)",
        ERROR_PIPE_CONNECTED => "ERROR_PIPE_CONNECTED(535)",
        _ => "other",
    }
}

// ---- (i) default DACL of a NULL-SD pipe ----
fn probe_default_dacl(base: &str) {
    println!("\n== P14(i): default DACL of a NULL-security-descriptor pipe ==");
    let name = format!(r"{base}-default");
    match create_pipe(&name, None, true, false, 1) {
        Ok(h) => {
            match read_back_sddl(h) {
                Some(s) => println!("  OS default SDDL = {s}"),
                None => println!("  (could not read back)"),
            }
            unsafe { CloseHandle(h) };
        }
        Err(e) => println!("  create FAILED, err {} ({})", e, err_name(e)),
    }
}

// ---- (ii) our SDDL applies (read back) ----
fn probe_our_dacl(base: &str) {
    println!("\n== P14(ii): our SDDL applied, read back ==");
    for (label, sddl) in [("interactive", SDDL_INTERACTIVE), ("admins", SDDL_ADMINS)] {
        let name = format!(r"{base}-{label}");
        println!("  --- {label}: requested {sddl}");
        match create_pipe(&name, Some(sddl), true, true, 1) {
            Ok(h) => {
                match read_back_sddl(h) {
                    Some(s) => println!("  read back      {s}"),
                    None => println!("  (could not read back)"),
                }
                unsafe { CloseHandle(h) };
            }
            Err(e) => println!("  create FAILED, err {} ({})", e, err_name(e)),
        }
    }
}

// ---- (iii)+(v) shrunken IU mask: correct access round-trips, GENERIC_WRITE denied ----
fn probe_iu_mask(base: &str) {
    println!("\n== P14(iii)+(v): shrunken IU mask (0x12018b) read/write ==");

    // (v-a) GENERIC_READ|GENERIC_WRITE must be DENIED; GENERIC_WRITE maps to FILE_APPEND_DATA
    // which the mask withholds.
    {
        let name = format!(r"{base}-overask");
        match create_pipe(&name, Some(SDDL_IU_ONLY), true, true, 1) {
            Ok(server) => {
                match open_client(&name, GENERIC_READ | GENERIC_WRITE) {
                    Ok(h) => {
                        println!("  UNEXPECTED: GENERIC_READ|GENERIC_WRITE connected (should be denied)");
                        unsafe { CloseHandle(h) };
                    }
                    Err(e) => println!(
                        "  GENERIC_READ|GENERIC_WRITE -> {} ({})  [expect ACCESS_DENIED: over-asks APPEND]",
                        e,
                        err_name(e)
                    ),
                }
                unsafe { CloseHandle(server) };
            }
            Err(e) => println!("  create FAILED, err {} ({})", e, err_name(e)),
        }
    }

    // (iii)+(v-b) correct access FILE_GENERIC_READ|FILE_WRITE_DATA round-trips.
    {
        let name = format!(r"{base}-rw");
        let server = match create_pipe(&name, Some(SDDL_IU_ONLY), true, true, 1) {
            Ok(h) => h,
            Err(e) => {
                println!("  create FAILED, err {} ({})", e, err_name(e));
                return;
            }
        };
        let client_name = name.clone();
        let client = std::thread::spawn(move || -> Result<Vec<u8>, u32> {
            let h = open_client(&client_name, CLIENT_ACCESS)?;
            let msg = b"ping";
            let mut written = 0u32;
            // SAFETY: valid handle; writing a small local buffer.
            unsafe {
                WriteFile(
                    h,
                    msg.as_ptr(),
                    msg.len() as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            let mut buf = [0u8; 16];
            let mut read = 0u32;
            // SAFETY: valid handle; reading into a local buffer.
            unsafe {
                ReadFile(
                    h,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            };
            unsafe { CloseHandle(h) };
            Ok(buf[..read as usize].to_vec())
        });

        // SAFETY: valid server handle; blocking connect (client is connecting concurrently).
        let connected = unsafe { ConnectNamedPipe(server, ptr::null_mut()) };
        let cerr = unsafe { GetLastError() };
        if connected == 0 && cerr != ERROR_PIPE_CONNECTED {
            println!(
                "  ConnectNamedPipe FAILED, err {} ({})",
                cerr,
                err_name(cerr)
            );
        }
        let mut inbuf = [0u8; 16];
        let mut got = 0u32;
        // SAFETY: valid connected handle; read the client's request.
        unsafe {
            ReadFile(
                server,
                inbuf.as_mut_ptr(),
                inbuf.len() as u32,
                &mut got,
                ptr::null_mut(),
            )
        };
        let reply = b"pong";
        let mut w = 0u32;
        // SAFETY: valid connected handle; write the reply.
        unsafe {
            WriteFile(
                server,
                reply.as_ptr(),
                reply.len() as u32,
                &mut w,
                ptr::null_mut(),
            )
        };

        match client.join() {
            Ok(Ok(resp)) => println!(
                "  round trip OK: server saw {:?}, client got {:?}  [shrunken IU mask permits R/W]",
                String::from_utf8_lossy(&inbuf[..got as usize]),
                String::from_utf8_lossy(&resp),
            ),
            Ok(Err(e)) => println!("  client FAILED, err {} ({})", e, err_name(e)),
            Err(_) => println!("  client thread panicked"),
        }
        unsafe { CloseHandle(server) };
    }
}

// ---- (iv) reject-remote ----
fn probe_reject_remote(base: &str) {
    println!("\n== P14(iv): PIPE_REJECT_REMOTE_CLIENTS ==");
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".to_string());
    let short = base.trim_start_matches(r"\\.\pipe\");

    for (label, reject) in [
        ("reject_remote=ON", true),
        ("reject_remote=OFF (control)", false),
    ] {
        let name = format!(r"{base}-remote-{}", if reject { "on" } else { "off" });
        let short_name = format!("{short}-remote-{}", if reject { "on" } else { "off" });
        let remote_path = format!(r"\\{host}\pipe\{short_name}");
        match create_pipe(&name, Some(SDDL_INTERACTIVE), true, reject, 1) {
            Ok(server) => {
                match open_client(&remote_path, CLIENT_ACCESS) {
                    Ok(h) => {
                        println!("  {label}: remote {remote_path} -> CONNECTED");
                        unsafe { CloseHandle(h) };
                    }
                    Err(e) => {
                        println!("  {label}: remote {remote_path} -> {} ({})", e, err_name(e))
                    }
                }
                unsafe { CloseHandle(server) };
            }
            Err(e) => println!("  {label}: create FAILED, err {} ({})", e, err_name(e)),
        }
    }
    println!("  (expect: ON -> ACCESS_DENIED; OFF -> CONNECTED or a network error if SMB pipe access is off)");
}

// ---- (vi) squatting: create-instance denied for IU-only token; FIRST_PIPE_INSTANCE detect ----
fn probe_squatting(base: &str) {
    println!("\n== P14(vi): squatting defenses ==");

    // (vi-a) A token matching only the IU ACE cannot add another instance. Instance #1 is
    // created WITHOUT the first-instance flag and with max=2 so instance-count is not the
    // limiter; only the DACL's (absent) FILE_CREATE_PIPE_INSTANCE right for IU decides.
    {
        let name = format!(r"{base}-inst");
        match create_pipe(&name, Some(SDDL_IU_ONLY), false, true, 2) {
            Ok(first) => {
                // Second create of the same name; SD arg is ignored for additional instances,
                // access is checked against instance #1's IU-only DACL.
                match create_pipe(&name, None, false, true, 2) {
                    Ok(second) => {
                        println!(
                            "  UNEXPECTED: 2nd instance created (IU should lack create rights)"
                        );
                        unsafe { CloseHandle(second) };
                    }
                    Err(e) => println!(
                        "  2nd instance (IU-only DACL) -> {} ({})  [expect ACCESS_DENIED]",
                        e,
                        err_name(e)
                    ),
                }
                unsafe { CloseHandle(first) };
            }
            Err(e) => println!("  create #1 FAILED, err {} ({})", e, err_name(e)),
        }
    }

    // (vi-b) FILE_FLAG_FIRST_PIPE_INSTANCE: once an instance exists, a create WITH the flag
    // fails; this is how the real server detects a pre-existing squatter and aborts.
    {
        let name = format!(r"{base}-first");
        match create_pipe(
            &name,
            Some(SDDL_INTERACTIVE),
            true,
            true,
            PIPE_UNLIMITED_INSTANCES,
        ) {
            Ok(first) => {
                match create_pipe(&name, Some(SDDL_INTERACTIVE), true, true, PIPE_UNLIMITED_INSTANCES)
                {
                    Ok(second) => {
                        println!("  UNEXPECTED: 2nd FIRST_PIPE_INSTANCE create succeeded");
                        unsafe { CloseHandle(second) };
                    }
                    Err(e) => println!(
                        "  2nd create w/ FIRST_PIPE_INSTANCE -> {} ({})  [expect ACCESS_DENIED: squat detected]",
                        e,
                        err_name(e)
                    ),
                }
                unsafe { CloseHandle(first) };
            }
            Err(e) => println!("  create #1 FAILED, err {} ({})", e, err_name(e)),
        }
    }
}

fn main() {
    let base = format!(r"\\.\pipe\everyfind-probe-{}", std::process::id());
    println!("Everyfind probe P14 - named-pipe ACL / squatting / remote rejection");
    println!("base pipe name: {base}-*");

    probe_default_dacl(&base);
    probe_our_dacl(&base);
    probe_iu_mask(&base);
    probe_reject_remote(&base);
    probe_squatting(&base);

    println!("\ndone.");
}
