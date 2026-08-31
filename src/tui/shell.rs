//! Ctrl+E (reveal in Explorer) and Ctrl+O (open with the associated program) via `ShellExecuteW`:
//! no `cmd.exe`, so no shell-quoting injection. The path is
//! passed as a single wide string.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::UI::Shell::ShellExecuteW;

use crate::wide;

/// `SW_SHOWNORMAL` (hand-defined, repo practice).
const SW_SHOWNORMAL: i32 = 1;

/// Reveal `path` in Explorer with the file selected: `explorer.exe /select,"<path>"`.
///
/// The path is wrapped in `"..."` inside lpParameters, an **explorer parse requirement** for paths
/// containing spaces (refinement #5 / P17), not shell injection: `ShellExecuteW` hands the string
/// straight to explorer, no `cmd.exe`. Windows filenames cannot contain `"`, so quoting is safe.
pub fn reveal_in_explorer(path: &str) -> io::Result<()> {
    let op = wide("open");
    let file = wide("explorer.exe");
    let params = wide(&format!("/select,\"{path}\""));
    exec(op.as_ptr(), file.as_ptr(), params.as_ptr())
}

/// Open `path` with its associated program: the same as double-clicking it in Explorer
/// (refinement #6: a `.exe` launches; no confirmation dialog, since an explicit keypress is the intent).
pub fn open_with_associated(path: &str) -> io::Result<()> {
    let op = wide("open");
    let file = wide(path);
    exec(op.as_ptr(), file.as_ptr(), ptr::null())
}

fn exec(op: *const u16, file: *const u16, params: *const u16) -> io::Result<()> {
    // SAFETY: valid NUL-terminated wide strings; NULL hwnd/dir are valid; SW_SHOWNORMAL is a
    // documented show command.
    let hinst = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            op,
            file,
            params,
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value > 32 on success (legacy HINSTANCE-as-status convention).
    if hinst as isize > 32 {
        Ok(())
    } else {
        let code = unsafe { GetLastError() };
        Err(io::Error::other(format!(
            "ShellExecuteW failed (ret={}, GetLastError={code})",
            hinst as isize
        )))
    }
}
