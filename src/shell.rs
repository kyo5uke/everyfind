//! Explorer right-click integration (`ef shell install` / `uninstall`).
//!
//! Adds "Search here with Everyfind" to the context menu of folders, folder
//! backgrounds and drives. Selecting it launches `ef --in "<folder>"`, the
//! TUI seeded with a `path:"..." ` scope, so typing immediately narrows within
//! that folder.
//!
//! **Not part of the search-box integration, and not installed by default.** It belongs to
//! the CLI: nothing is loaded into explorer.exe, nothing answers Explorer's search box, and
//! all it does is start `ef.exe` with a folder. It is kept for that reason: the three
//! routes that *did* hand Explorer results from outside (the Federated Search connector,
//! the namespace extension, the watcher that followed Explorer around) were removed once
//! the engine swap in [`crate::search_engine`] worked, and this is not one of them.
//!
//! Deliberately boring plumbing:
//! - **Per-user** (`HKCU\Software\Classes\...`): no elevation, no other users
//!   affected, uninstall removes exactly what install wrote.
//! - **Classic context-menu keys only.** On Windows 11 the entry appears
//!   under "Show more options" (Shift+F10). The modern top-level menu
//!   requires a packaged (MSIX) `IExplorerCommand`, out of scope for a
//!   zip-distributed tool, and explorer.exe injection is off the table by
//!   principle (see the P14 security posture: we do not run code inside
//!   other processes).

use anyhow::{anyhow, Context, Result};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::wide;

/// The three classic context-menu surfaces: a folder item, a folder's
/// background, and a drive root. `%V` expands to the folder path on all of
/// them (verbatim, no short names).
const MENU_KEYS: [&str; 3] = [
    r"Software\Classes\Directory\shell\everyfind",
    r"Software\Classes\Directory\Background\shell\everyfind",
    r"Software\Classes\Drive\shell\everyfind",
];

const MENU_TEXT: &str = "Search here with Everyfind";

/// Owned HKEY that always closes.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn create_key(path: &str) -> Result<Key> {
    let mut h: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut h,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(anyhow!(
            "RegCreateKeyExW({path}) failed (win32 error {status})"
        ));
    }
    Ok(Key(h))
}

/// Set a REG_SZ value; `None` = the key's default value.
fn set_sz(key: &Key, name: Option<&str>, value: &str) -> Result<()> {
    let data = wide(value);
    let name_w = name.map(wide);
    let pname = name_w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
    let status = unsafe {
        RegSetValueExW(
            key.0,
            pname,
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(anyhow!(
            "RegSetValueExW({}) failed (win32 error {status})",
            name.unwrap_or("<default>")
        ));
    }
    Ok(())
}

/// Register the context-menu entries, pointing at the current `ef.exe`.
/// Idempotent: re-running after moving the binary refreshes the paths.
pub fn install() -> Result<()> {
    let exe = std::env::current_exe().context("locating ef.exe")?;
    let exe = exe.to_string_lossy();
    let command = format!("\"{exe}\" --in \"%V\"");
    for base in MENU_KEYS {
        let key = create_key(base)?;
        set_sz(&key, None, MENU_TEXT)?;
        set_sz(&key, Some("Icon"), &format!("\"{exe}\""))?;
        let cmd = create_key(&format!(r"{base}\command"))?;
        set_sz(&cmd, None, &command)?;
    }
    Ok(())
}

/// Remove everything `install` wrote. Missing keys are fine (already gone).
pub fn uninstall() -> Result<()> {
    for base in MENU_KEYS {
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(base).as_ptr()) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(anyhow!(
                "RegDeleteTreeW({base}) failed (win32 error {status})"
            ));
        }
    }
    Ok(())
}
