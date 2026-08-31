//! Self-built clipboard write (Ctrl+Y) as `CF_UNICODETEXT`, over `windows-sys`: no new crate
//! The ownership contract is pinned by probe **P16**
//! (`examples/probe_clipboard.rs`): after `SetClipboardData` succeeds the system owns the
//! `HGLOBAL` (must NOT be freed); on failure we free it ourselves.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{GetLastError, GlobalFree};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

/// Copy `text` to the clipboard as Unicode text. Fire-and-forget: the clipboard is a shared OS
/// resource, so another app can overwrite it immediately (P16); this only guarantees the value
/// is placed. Retries `OpenClipboard` briefly on contention.
pub fn set_unicode_text(text: &str) -> io::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    open_clipboard()?;
    let result = write_locked(&wide, bytes);
    // SAFETY: we opened the clipboard above; close it regardless of the write outcome.
    unsafe { CloseClipboard() };
    result
}

fn write_locked(wide: &[u16], bytes: usize) -> io::Result<()> {
    // SAFETY: clear the clipboard before taking ownership of a fresh block.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(last_error("EmptyClipboard"));
    }
    // SAFETY: a moveable global block for the clipboard to own.
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if hmem.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "GlobalAlloc failed",
        ));
    }
    // SAFETY: lock to copy the wide string in, then unlock.
    let dst = unsafe { GlobalLock(hmem) } as *mut u16;
    if dst.is_null() {
        unsafe { GlobalFree(hmem) };
        return Err(last_error("GlobalLock"));
    }
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        GlobalUnlock(hmem);
    }
    // SAFETY: hand the block to the clipboard. On success the SYSTEM owns hmem; do NOT free it
    // (P16 contract); on failure ownership did not transfer, so we free it.
    let set = unsafe { SetClipboardData(CF_UNICODETEXT, hmem) };
    if set.is_null() {
        let e = last_error("SetClipboardData");
        unsafe { GlobalFree(hmem) };
        return Err(e);
    }
    Ok(())
}

fn open_clipboard() -> io::Result<()> {
    for attempt in 0..20 {
        // SAFETY: NULL owner is valid for a console app with no window.
        if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
            return Ok(());
        }
        if attempt == 19 {
            return Err(last_error("OpenClipboard"));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    unreachable!()
}

fn last_error(what: &str) -> io::Error {
    let code = unsafe { GetLastError() };
    io::Error::other(format!("{what} failed (GetLastError={code})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::DataExchange::GetClipboardData;

    /// Round-trips through the real clipboard; `#[ignore]` so it never runs in a normal
    /// `cargo test` (it mutates a shared OS resource; P16 is the sanctioned validation). Run
    /// explicitly: `cargo test -- --ignored clipboard_round_trip`.
    #[test]
    #[ignore]
    fn clipboard_round_trip() {
        let sample = "テスト 🔍 clipboard.rs";
        set_unicode_text(sample).unwrap();
        open_clipboard().unwrap();
        let got = unsafe {
            let h = GetClipboardData(CF_UNICODETEXT);
            assert!(!h.is_null());
            let p = GlobalLock(h) as *const u16;
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            GlobalUnlock(h);
            CloseClipboard();
            s
        };
        assert_eq!(got, sample);
    }
}
