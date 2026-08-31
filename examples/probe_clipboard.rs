//! Everyfind M4 probe P16: clipboard write ownership contract (Ctrl+Y).
//!
//! The TUI's Ctrl+Y copies the selected path to the clipboard as `CF_UNICODETEXT`, built by
//! hand over `windows-sys` (no new crate). This probe pins the
//! **ownership contract** before that code is written.
//!
//! Write path: `OpenClipboard` -> `EmptyClipboard` -> `GlobalAlloc(GMEM_MOVEABLE)` ->
//! `GlobalLock`/copy/`GlobalUnlock` -> `SetClipboardData` -> `CloseClipboard`. **After
//! `SetClipboardData` succeeds the system OWNS the HGLOBAL, so we must NOT free it**; on
//! `SetClipboardData` FAILURE we must `GlobalFree` it ourselves.
//!
//! Read path: `OpenClipboard` -> `GetClipboardData` -> `GlobalLock`/read/`GlobalUnlock` ->
//! `CloseClipboard`. The handle from `GetClipboardData` is clipboard-owned; do not free it.
//!
//! The probe writes a Japanese + emoji + space string, then reads it back and asserts equality.
//! Survival of the exact bytes after we deliberately DID NOT free the buffer is the proof that
//! ownership transferred. Non-elevated is fine; it touches no volume.
//!
//! ```text
//! cargo run --example probe_clipboard
//! ```

use std::ptr;

use windows_sys::Win32::Foundation::{GetLastError, GlobalFree};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};

// Hand-defined constants (repo practice, like the raw FSCTL codes). Stable documented values.
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

/// The string we round-trip: Japanese + an emoji + a space (the awkward cases the TUI must
/// handle for real filenames).
const SAMPLE: &str = "テスト検索 🔍 日本語 with space.txt";

fn main() {
    println!("== P16: clipboard CF_UNICODETEXT ownership contract ==");
    println!("sample: {SAMPLE:?} ({} chars)", SAMPLE.chars().count());

    match write_clipboard(SAMPLE) {
        Ok(()) => println!("write: SetClipboardData succeeded; HGLOBAL NOT freed (system owns it)"),
        Err(e) => {
            eprintln!("write FAILED: {e}");
            std::process::exit(1);
        }
    }

    match read_clipboard() {
        Ok(got) => {
            println!("read back: {got:?}");
            if got == SAMPLE {
                println!("\nRESULT: PASS - round trip identical; ownership transfer confirmed.");
                println!("Contract for tui/clipboard.rs:");
                println!("  * after SetClipboardData OK -> do NOT GlobalFree (system owns it)");
                println!("  * on SetClipboardData failure -> GlobalFree the HGLOBAL");
                println!(
                    "  * OpenClipboard(NULL owner) works from a console app; retry on contention"
                );
                println!("  * buffer = (utf16 len + 1) * 2 bytes, NUL-terminated, GMEM_MOVEABLE");
            } else {
                eprintln!("\nRESULT: FAIL - read back does not match the written value");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("read FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// Open the clipboard with a short retry (another process may briefly hold it).
fn open_clipboard() -> Result<(), String> {
    for attempt in 0..20 {
        // SAFETY: NULL owner is valid for a console app with no window.
        if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
            return Ok(());
        }
        let e = unsafe { GetLastError() };
        std::thread::sleep(std::time::Duration::from_millis(5));
        if attempt == 19 {
            return Err(format!(
                "OpenClipboard failed after retries (GetLastError={e})"
            ));
        }
    }
    unreachable!()
}

/// Write `text` as `CF_UNICODETEXT`, honoring the ownership contract.
fn write_clipboard(text: &str) -> Result<(), String> {
    // UTF-16 + NUL terminator.
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    open_clipboard()?;

    // From here, every early return must CloseClipboard.
    let result = (|| {
        if unsafe { EmptyClipboard() } == 0 {
            return Err(format!("EmptyClipboard failed (GetLastError={})", unsafe {
                GetLastError()
            }));
        }

        // SAFETY: allocate a moveable global block for the clipboard to take ownership of.
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if hmem.is_null() {
            return Err("GlobalAlloc failed".into());
        }

        // SAFETY: lock to get a writable pointer; copy the wide string in.
        let dst = unsafe { GlobalLock(hmem) } as *mut u16;
        if dst.is_null() {
            unsafe { GlobalFree(hmem) };
            return Err("GlobalLock failed".into());
        }
        unsafe { ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len()) };
        unsafe { GlobalUnlock(hmem) };

        // SAFETY: hand the block to the clipboard. On success the SYSTEM owns hmem; do NOT free.
        let set = unsafe { SetClipboardData(CF_UNICODETEXT, hmem) };
        if set.is_null() {
            // Failure: ownership did NOT transfer; we must free it.
            let e = unsafe { GetLastError() };
            unsafe { GlobalFree(hmem) };
            return Err(format!("SetClipboardData failed (GetLastError={e})"));
        }
        Ok(())
    })();

    unsafe { CloseClipboard() };
    result
}

/// Read back `CF_UNICODETEXT`. The returned handle is clipboard-owned; do not free it.
fn read_clipboard() -> Result<String, String> {
    open_clipboard()?;
    let result = (|| {
        // SAFETY: query the current CF_UNICODETEXT handle (clipboard-owned).
        let h = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if h.is_null() {
            return Err(format!(
                "GetClipboardData(CF_UNICODETEXT) returned NULL (GetLastError={})",
                unsafe { GetLastError() }
            ));
        }
        // SAFETY: lock, read the NUL-terminated wide string, unlock.
        let p = unsafe { GlobalLock(h) } as *const u16;
        if p.is_null() {
            return Err("GlobalLock on the clipboard handle failed".into());
        }
        let mut len = 0usize;
        // SAFETY: walk to the NUL terminator (clipboard text is always NUL-terminated).
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(p, len) };
        let s = String::from_utf16_lossy(slice);
        unsafe { GlobalUnlock(h) };
        Ok(s)
    })();
    unsafe { CloseClipboard() };
    result
}
