//! Swap the engine behind Explorer's own search box.
//!
//! A search folder gets its rows from an OLE DB provider, `Search.CollatorDSO`
//! (`{9E175B8B-...}`, normally `tquery.dll`). That CLSID is registered per-machine
//! but nothing pins it per-user, so an `HKCU\...\CLSID` entry shadows it for this
//! account only: reversible, no elevation. We point it at `ef_search_engine.dll`,
//! which forwards every part of the OLE DB conversation to the real `tquery.dll`
//! except the one `Execute` call, whose rowset it replaces with Everyfind's
//! results. The breadcrumb, the view, and the search box itself are never
//! touched, so nothing about the window's behaviour changes but the rows.
//!
//! Install writes the key; uninstall deletes it. Because COM reads the registry
//! when it activates the provider, a search run after uninstall is native again.

use std::path::{Path, PathBuf};

use crate::wide;
use anyhow::{anyhow, Context, Result};

/// The engines Explorer answers a search with. Which one it picks depends on
/// whether the folder happens to be in the Windows Search index: indexed folders
/// go to the data source, everywhere else (drive roots, `C:\Windows`, anything
/// the user never added to the index) goes to the filesystem-walk engine. Both
/// are replaced, so coverage does not depend on that distinction.
const ENGINES: &[(&str, &str)] = &[
    (
        "{9E175B8B-F52A-11D8-B9A5-505054503030}",
        "Windows Search Data Source",
    ),
    (
        "{1685D4AB-A51B-4AF1-A4E5-CEE87002431D}",
        "Search Grep Provider",
    ),
    (
        "{1C0F439D-7C29-4BDE-8952-4EEB6A49E048}",
        "Search Grep Resolver",
    ),
];
/// File name of the provider DLL, both where we look for it and where we stage it.
const DLL_NAME: &str = "ef_search_engine.dll";

/// Per-user directory the active DLL is staged into.
fn install_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("LOCALAPPDATA is not set"))?;
    Ok(base.join("everyfind"))
}

/// A loaded DLL cannot be overwritten (an Explorer may still have the previous
/// build mapped), so each distinct build is staged under its own content-hashed
/// name. Identical content reuses the same file; a rebuild gets a fresh one.
fn staged_name_for(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("ef_search_engine_{:016x}.dll", h.finish())
}

/// The machine type out of a PE image's file header, or `None` if these bytes are not one.
///
/// `MZ`, then the offset at 0x3C, then `PE\0\0`, then the field. Reading it by hand rather than
/// through a crate because this is the whole of what is needed and the layout has not moved
/// since 1993.
fn machine_of(bytes: &[u8]) -> Option<u16> {
    if bytes.get(..2)? != b"MZ" {
        return None;
    }
    let at = u32::from_le_bytes(bytes.get(0x3C..0x40)?.try_into().ok()?) as usize;
    if bytes.get(at..at + 4)? != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(
        bytes.get(at + 4..at + 6)?.try_into().ok()?,
    ))
}

fn machine_name(m: u16) -> &'static str {
    match m {
        0x014C => "x86",
        0x8664 => "x64",
        0xAA64 => "ARM64",
        _ => "unknown",
    }
}

/// The head of `path`, enough to reach the PE header and no more.
fn pe_head(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// The process that will load the DLL. Its own image says which machine type it can load,
/// which is the only definition of "the right architecture" that matters here.
fn host_process() -> PathBuf {
    let dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    dir.join("explorer.exe")
}

/// Refuse a DLL explorer.exe could not load.
///
/// This matters more than it looks. Measured by pointing the registration at a DLL that does
/// not exist: explorer.exe survives, but the search stops working altogether; a `search-ms:`
/// navigation opens no window at all, and nothing falls back to Windows. A DLL of the wrong
/// machine type fails to load for the same reason and would land in the same state. An x64
/// release unpacked on an ARM64 machine is exactly how that happens, so it is checked once,
/// here, where the answer is still "Everyfind is not installed" rather than "search is broken".
///
/// Only refuses when both machine types are known: an unreadable explorer.exe is a reason to
/// stay quiet, not to block an install that is probably fine.
fn refuse_wrong_architecture(dll: &[u8]) -> Result<()> {
    let host = host_process();
    let (Some(ours), Some(theirs)) = (
        machine_of(dll),
        pe_head(&host).as_deref().and_then(machine_of),
    ) else {
        return Ok(());
    };
    if ours != theirs {
        return Err(anyhow!(
            "this build is {} and {} is {}; it could not be loaded, \
             and an engine that cannot load leaves Explorer with no search at all. \
             Install the {} build.",
            machine_name(ours),
            host.display(),
            machine_name(theirs),
            machine_name(theirs),
        ));
    }
    Ok(())
}

/// True for any DLL we staged (the prefix in our own directory).
fn is_ours(path: &str) -> bool {
    let lower = path.to_lowercase();
    install_dir()
        .ok()
        .map(|d| lower.starts_with(&d.to_string_lossy().to_lowercase()))
        .unwrap_or(false)
        && lower.contains("ef_search_engine")
}

/// The DLL that really implements `clsid`, read from the machine's own COM
/// registration. Asking the registry rather than hardcoding a path keeps this
/// correct across Windows versions, drive layouts and 32/64-bit differences.
fn real_server_of(clsid: &str) -> Option<String> {
    let subkey = format!(r"SOFTWARE\Classes\CLSID\{clsid}\InprocServer32");
    reg::read_str_in(
        windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
        &subkey,
        "",
    )
    .filter(|s| !s.trim().is_empty())
}

mod reg {
    use super::{wide, Result};
    use anyhow::anyhow;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    };

    /// Create (or open) a key under HKCU, making any missing parents on the way.
    pub fn create(subkey: &str) -> Result<HKEY> {
        let mut h: HKEY = std::ptr::null_mut();
        let st = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(subkey).as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut h,
                std::ptr::null_mut(),
            )
        };
        if st != ERROR_SUCCESS {
            return Err(anyhow!("creating HKCU\\{subkey} failed (win32 {st})"));
        }
        Ok(h)
    }

    /// Set a string value on an open key; `name` empty means the default value.
    pub fn set_str(h: HKEY, name: &str, value: &str) -> Result<()> {
        let data = wide(value);
        let st = unsafe {
            windows_sys::Win32::System::Registry::RegSetValueExW(
                h,
                if name.is_empty() {
                    std::ptr::null()
                } else {
                    wide(name).as_ptr()
                },
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if st != ERROR_SUCCESS {
            return Err(anyhow!("writing value '{name}' failed (win32 {st})"));
        }
        Ok(())
    }

    pub fn close(h: HKEY) {
        unsafe { RegCloseKey(h) };
    }

    /// Remove a value; absent is fine.
    pub fn del_value(h: HKEY, name: &str) {
        unsafe {
            windows_sys::Win32::System::Registry::RegDeleteValueW(h, wide(name).as_ptr());
        }
    }

    /// Delete a key and everything under it. Missing is success (already gone).
    pub fn delete_tree(subkey: &str) -> Result<()> {
        let st = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(subkey).as_ptr()) };
        if st != ERROR_SUCCESS && st != ERROR_FILE_NOT_FOUND {
            return Err(anyhow!("deleting HKCU\\{subkey} failed (win32 {st})"));
        }
        Ok(())
    }

    /// Read a string value from HKCU, or `None` if the key/value is absent.
    pub fn read_str(subkey: &str, name: &str) -> Option<String> {
        read_str_in(HKEY_CURRENT_USER, subkey, name)
    }

    /// Read a string value from any hive.
    pub fn read_str_in(hive: HKEY, subkey: &str, name: &str) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut cb = (buf.len() * 2) as u32;
        let st = unsafe {
            RegGetValueW(
                hive,
                wide(subkey).as_ptr(),
                if name.is_empty() {
                    std::ptr::null()
                } else {
                    wide(name).as_ptr()
                },
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut cb,
            )
        };
        let _ = (KEY_READ, KEY_WRITE); // keep imports honest across cfgs
        if st != ERROR_SUCCESS {
            return None;
        }
        Some(decode_sz(&buf[..(cb as usize / 2).min(buf.len())]))
    }

    /// A `REG_SZ` payload as a Rust string.
    ///
    /// `RegGetValueW` reports the size it *wrote*, which can exceed the string: it guarantees
    /// termination and pads to do so (measured: a 30-character value comes back as 66 bytes,
    /// two more than the 62 the string plus one NUL needs). Trimming a fixed single character
    /// therefore leaves NULs embedded in the result, which then travel into the `RealDll` value
    /// we write back and into everything that compares or prints it. Cut at the first NUL
    /// instead: that is where the string ends by definition.
    pub fn decode_sz(units: &[u16]) -> String {
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }
}

fn clsid_key(clsid: &str) -> String {
    format!(r"Software\Classes\CLSID\{clsid}")
}

/// Marking locations as indexed, so every folder is answered by the engine we
/// replaced.
///
/// Explorer picks its engine by asking Windows Search whether the folder is in
/// the crawl scope. Only the indexed engine is ours, so folders outside the scope
/// (drive roots, `C:\Windows`, anything never added) would still answer with
/// Windows' own filesystem walk. Adding a scope *rule* changes that answer.
///
/// It does not make Windows index anything: the crawler visits *roots*, and this
/// adds none (measured: with `C:\` in scope the catalog did not grow and the
/// indexer used no CPU). The rules are per-user and removed on uninstall.
mod scope {
    use super::*;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Search::{CSearchManager, ISearchManager};

    /// The crawl scope speaks URLs: `file:///C:\`.
    fn url_for(path: &str) -> String {
        format!("file:///{path}")
    }

    fn with_scope<T>(
        f: impl FnOnce(&windows::Win32::System::Search::ISearchCrawlScopeManager) -> Result<T>,
    ) -> Result<T> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let mgr: ISearchManager = CoCreateInstance(&CSearchManager, None, CLSCTX_ALL)
                .context("opening Windows Search")?;
            let catalog = mgr
                .GetCatalog(&HSTRING::from("SystemIndex"))
                .context("opening the SystemIndex catalog")?;
            let scope = catalog
                .GetCrawlScopeManager()
                .context("opening the crawl scope")?;
            f(&scope)
        }
    }

    pub fn is_included(path: &str) -> bool {
        with_scope(|s| {
            let u = HSTRING::from(url_for(path));
            Ok(unsafe { s.IncludedInCrawlScope(PCWSTR(u.as_ptr())) }
                .map(|b| b.as_bool())
                .unwrap_or(false))
        })
        .unwrap_or(false)
    }

    /// Apply one rule change to each path, and save if any of them took.
    ///
    /// `add` and `remove` differ by one method call and were otherwise the same fifteen lines,
    /// which is the shape where a later fix gets made once and forgotten once. Saving only on
    /// success is the part worth not forgetting: `SaveAll` is what makes the change outlive the
    /// process, and calling it after a run that changed nothing rewrites the crawl scope for no
    /// reason.
    fn each(
        paths: &[String],
        op: impl Fn(&windows::Win32::System::Search::ISearchCrawlScopeManager, PCWSTR) -> bool,
    ) -> Result<usize> {
        with_scope(|s| {
            let mut n = 0;
            for p in paths {
                let u = HSTRING::from(url_for(p));
                if op(s, PCWSTR(u.as_ptr())) {
                    n += 1;
                }
            }
            if n > 0 {
                unsafe { s.SaveAll() }.ok();
            }
            Ok(n)
        })
    }

    pub fn add(paths: &[String]) -> Result<usize> {
        each(paths, |s, url| {
            unsafe { s.AddUserScopeRule(url, true, false, 0) }.is_ok()
        })
    }

    pub fn remove(paths: &[String]) -> Result<usize> {
        each(paths, |s, url| unsafe { s.RemoveScopeRule(url) }.is_ok())
    }
}

/// Find the DLL to install: an explicit path if given, otherwise the one shipped
/// next to `ef.exe`.
fn source_dll(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(anyhow!("no DLL at {}", p.display()))
        };
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(DLL_NAME)));
    match beside {
        Some(p) if p.is_file() => Ok(p),
        _ => Err(anyhow!(
            "could not find {DLL_NAME} next to ef.exe; pass --dll <path> to the built provider"
        )),
    }
}

/// What an install did to Windows' crawl scope.
///
/// Reported rather than assumed, because getting it wrong is silent. Declaring a drive
/// "indexed" only changes **which engine Explorer asks**; it does not make Windows index
/// anything. So declaring a drive Everyfind cannot serve is the worst of both: Explorer stops
/// walking it (which used to work, slowly) and asks an engine that hands the search straight
/// back to a catalog with nothing in it for that drive. The window comes back empty and
/// confident, on a drive that used to answer.
pub enum Scoped {
    /// `--no-scope`: the scope was deliberately left as it was.
    Skipped,
    /// The daemon's drive already answers with the indexed engine; nothing to add.
    AlreadyIncluded(char),
    /// Declared indexed, so every folder on it asks the engine we replaced.
    Added(char),
    /// The daemon did not say which volume it serves, so nothing was declared.
    UnknownVolume,
}

/// The volume the daemon is serving, if it is running and answers promptly.
///
/// The install has to ask, because everyfind indexes **one** volume and only that one can be
/// declared. There is no local answer to fall back on: the drive `ef.exe` happens to sit on is
/// not necessarily the one `efd` was installed for.
fn served_drive() -> Option<char> {
    match crate::ipc::client::request(
        crate::ipc::PIPE_NAME,
        &crate::ipc::Request::Status,
        std::time::Duration::from_secs(2),
    ) {
        Ok(crate::ipc::Response::Status(s)) => Some(s.drive),
        _ => None,
    }
}

/// Install the override: stage the DLL to a stable path and point the CLSID at it.
/// `debug` turns on the DLL's diagnostic log (off for normal use).
pub fn install(dll: Option<&Path>, debug: bool, cover_all: bool) -> Result<(PathBuf, Scoped)> {
    let src = source_dll(dll)?;
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let bytes = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
    refuse_wrong_architecture(&bytes)?;
    let dst = dir.join(staged_name_for(&bytes));
    // Only write if this exact build is not already staged (a loaded copy of it
    // can't be overwritten, and doesn't need to be).
    if !dst.exists() {
        std::fs::write(&dst, &bytes).with_context(|| format!("staging {}", dst.display()))?;
    }
    // Best-effort tidy of older builds; skip any an Explorer still has mapped.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p != dst
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("ef_search_engine"))
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    // Shadow every engine, each forwarding to whatever really implements it on
    // this machine. An engine we cannot resolve is skipped rather than shadowed
    // with a broken forward.
    let mut shadowed = 0usize;
    for (clsid, label) in ENGINES {
        let Some(real) = real_server_of(clsid) else {
            continue;
        };
        let key = clsid_key(clsid);
        let root = reg::create(&key)?;
        let r = reg::set_str(root, "", label);
        reg::close(root);
        r?;

        let inproc = reg::create(&format!(r"{key}\InprocServer32"))?;
        let write = (|| {
            reg::set_str(inproc, "", &dst.to_string_lossy())?;
            reg::set_str(inproc, "ThreadingModel", "Both")?;
            reg::set_str(inproc, "RealDll", &real)?;
            reg::set_str(inproc, "Swap", "everyfind")?;
            // Set *or clear*: installing without --debug must leave no diagnostics
            // behind from an earlier one.
            if debug {
                reg::set_str(inproc, "Debug", "1")?;
            } else {
                reg::del_value(inproc, "Debug");
            }
            Ok::<(), anyhow::Error>(())
        })();
        reg::close(inproc);
        write?;
        shadowed += 1;
    }
    if shadowed == 0 {
        return Err(anyhow!(
            "none of the search engines are registered on this machine"
        ));
    }

    // Make every folder on the volume we serve take the engine we replaced, and remember what
    // we marked so uninstall can put it back exactly.
    //
    // **The volume we serve, not every fixed drive.** Marking them all is what the first
    // version did, and on a machine with a second drive it is a regression rather than a
    // feature: `D:` stops being walked by Explorer and starts being asked of a catalog that
    // has nothing for it, so a search that used to find files finds none and says so plainly.
    // A drive Everyfind cannot answer for is better left exactly as Windows had it.
    let scoped = if !cover_all {
        Scoped::Skipped
    } else {
        match served_drive() {
            None => Scoped::UnknownVolume,
            Some(drive) => {
                let root = format!(r"{drive}:\");
                if scope::is_included(&root) {
                    Scoped::AlreadyIncluded(drive)
                } else {
                    scope::add(std::slice::from_ref(&root))?;
                    let key = reg::create(&format!(r"{}\InprocServer32", clsid_key(ENGINES[0].0)))?;
                    let r = reg::set_str(key, "ScopeAdded", &root);
                    reg::close(key);
                    r?;
                    Scoped::Added(drive)
                }
            }
        }
    };
    Ok((dst, scoped))
}

/// Remove the override. Native Windows Search is back on the next search. The
/// staged DLL is left in place (harmless without the key, and an Explorer may
/// still have it mapped); a reinstall reuses it.
pub fn uninstall() -> Result<()> {
    // Undo the scope marks first: the record of them lives under the key we are
    // about to delete.
    let inproc = format!(r"{}\InprocServer32", clsid_key(ENGINES[0].0));
    if let Some(list) = reg::read_str(&inproc, "ScopeAdded") {
        let paths: Vec<String> = list
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if !paths.is_empty() {
            let _ = scope::remove(&paths);
        }
    }
    for (clsid, _) in ENGINES {
        reg::delete_tree(&clsid_key(clsid))?;
    }
    Ok(())
}

/// What `ef explorer engine status` reports: whether the override is present and
/// whether it points at our staged DLL.
pub struct Status {
    pub installed: bool,
    pub dll: Option<String>,
    pub points_at_ours: bool,
    pub debug: bool,
    /// How many of the engines are shadowed, and how many exist here; a partial
    /// install means some folders would still answer with Windows' own results.
    pub engines_shadowed: usize,
    pub engines_total: usize,
}

pub fn status() -> Status {
    let mut dll = None;
    let mut debug = false;
    let mut shadowed = 0usize;
    let mut total = 0usize;
    for (clsid, _) in ENGINES {
        if real_server_of(clsid).is_some() {
            total += 1;
        }
        let inproc = format!(r"{}\InprocServer32", clsid_key(clsid));
        if let Some(d) = reg::read_str(&inproc, "") {
            shadowed += 1;
            debug |= reg::read_str(&inproc, "Debug").is_some();
            if dll.is_none() {
                dll = Some(d);
            }
        }
    }
    let points_at_ours = dll.as_deref().map(is_ours).unwrap_or(false);
    Status {
        installed: shadowed > 0,
        debug,
        points_at_ours,
        dll,
        engines_shadowed: shadowed,
        engines_total: total,
    }
}

#[cfg(test)]
mod tests {
    use super::reg::decode_sz;
    use super::{machine_of, pe_head};

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// The smallest thing that answers "which machine is this image for": a DOS stub whose
    /// 0x3C field points at a PE signature, and the field right behind it.
    fn image(machine: u16) -> Vec<u8> {
        let at = 0x80usize;
        let mut b = vec![0u8; at + 8];
        b[..2].copy_from_slice(b"MZ");
        b[0x3C..0x40].copy_from_slice(&(at as u32).to_le_bytes());
        b[at..at + 4].copy_from_slice(b"PE\0\0");
        b[at + 4..at + 6].copy_from_slice(&machine.to_le_bytes());
        b
    }

    #[test]
    fn a_pe_image_says_which_machine_it_is_for() {
        assert_eq!(machine_of(&image(0x8664)), Some(0x8664), "x64");
        assert_eq!(machine_of(&image(0xAA64)), Some(0xAA64), "ARM64");
    }

    /// Anything that is not an image has to come back `None` rather than a number read out of
    /// the middle of it: the caller refuses an install on a mismatch, so a wrong answer here
    /// would block a working one.
    #[test]
    fn anything_that_is_not_an_image_says_so() {
        assert_eq!(machine_of(b"not an executable"), None);
        assert_eq!(machine_of(&[]), None);
        let mut truncated = image(0x8664);
        truncated.truncate(0x40);
        assert_eq!(machine_of(&truncated), None, "the header is past the end");
        let mut no_sig = image(0x8664);
        no_sig[0x80] = b'X';
        assert_eq!(machine_of(&no_sig), None, "the offset points at nothing");
    }

    /// The check is only as good as its reading of the real explorer.exe, so read that one.
    #[test]
    fn the_shell_reads_as_a_real_image() {
        let host = super::host_process();
        let Some(head) = pe_head(&host) else {
            return; // no explorer.exe to read (not Windows, or no rights): nothing to assert
        };
        assert!(
            machine_of(&head).is_some(),
            "{} did not read as a PE image",
            host.display()
        );
    }

    #[test]
    fn decode_sz_stops_at_the_first_nul() {
        let mut buf = utf16(r"C:\windows\system32\tquery.dll");
        // What RegGetValueW actually hands back: the string, its terminator, and padding.
        // 30 characters reported as 66 bytes = 33 units (measured on Windows 11).
        buf.extend_from_slice(&[0, 0, 0]);
        assert_eq!(buf.len(), 33);
        assert_eq!(decode_sz(&buf), r"C:\windows\system32\tquery.dll");
    }

    #[test]
    fn decode_sz_handles_exact_and_unterminated_payloads() {
        assert_eq!(decode_sz(&utf16("abc")), "abc", "no terminator at all");
        assert_eq!(decode_sz(&[]), "", "empty payload");
        assert_eq!(decode_sz(&[0]), "", "terminator only");
    }
}
