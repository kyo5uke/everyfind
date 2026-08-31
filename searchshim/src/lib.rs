//! Everyfind's answer behind Explorer's own search box.
//!
//! An HKCU `InprocServer32` override tells COM that `Search.CollatorDSO` lives in this DLL, so
//! explorer.exe loads it and asks *us* for the search data source. We load the genuine
//! `tquery.dll`: the installer recorded its path in a sibling `RealDll` value, because the
//! override erased the original default, and forward the whole conversation to it: the class
//! factory, the data source, the session, the command. Every object the shell gets back is the
//! real one.
//!
//! One call is answered differently. `ICommand::Execute` returns our rowset instead of the
//! genuine provider's, and no native query is ever run. That is the entire substitution: the
//! breadcrumb, the view, the search box, the navigation and the item behaviour are all Windows'
//! own, and only the rows are Everyfind's.
//!
//! This file is the conversation: which vtable slots are listened to, what is read out of each,
//! and the `DllGetClassObject` that starts it. The rest lives next door.
//!
//! - [`com`]: reaching into somebody else's vtable, and getting back out intact.
//! - [`answer`]: what to answer with (scope, filters, page size, order). Knows nothing of COM.
//! - [`provider`]: the rowset itself, and the properties each row carries.
//! - [`crawlscope`]: what Windows keeps out of a search, read from Windows.
//!
//! It began as pure measurement (forward everything, log what happens) and the `Swap=off`
//! setting still restores exactly that, which is how every claim in these files about what
//! Explorer does was checked against Explorer doing it.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

mod answer;
mod com;
mod crawlscope;
mod provider;

use answer::{answer_for, Answer, FETCH_ROWS, MAX_ROWS_SET};
use com::{
    install, looks_like_com, mem_readable, orig_of, qi, read_wide, read_wide_in_region, release,
    restore_patches, vslot,
};

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;

/// `E_UNEXPECTED`, returned only if the real server cannot be reached, which
/// would mean the shim is registered but broken. Better a clean failure than a
/// hang.
const E_UNEXPECTED: i32 = 0x8000_FFFFu32 as i32;
/// `S_FALSE` from `DllCanUnloadNow` means "keep me loaded", the safe default for
/// a shim that does not track the objects it forwarded.
const S_FALSE: i32 = 1;

/// Whether diagnostics are on.
///
/// `log` already discards cheaply, but *reaching* it is not always free: some callers build a
/// message by walking memory the caller owns. Those check this first, so a normal install does
/// no diagnostic work at all on an Explorer thread.
pub(crate) fn debugging() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

pub(crate) fn log(line: &str) {
    if !DEBUG.load(Ordering::Relaxed) {
        return;
    }
    // Logging must never change behavior. That is a stronger claim than "swallow every error",
    // and it was not true: this opens, appends and closes the file per call, and callers on the
    // row path call it *per row*. Fetching a page then ran at the speed of the filesystem,
    // and because two Explorer threads append to the same file, their lines interleaved
    // mid-write, so the record of the stall was itself corrupted. Serialising here costs a
    // lock a diagnostic build can afford and keeps each line whole.
    static FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
    let mut guard = com::held(&FILE);
    if guard.is_none() {
        let path = std::env::temp_dir().join("searchshim.log");
        *guard = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
    }
    if let Some(f) = guard.as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

/// Whether to log the per-row calls (`GetRowsAt`, `GetRowFromHROW`).
///
/// Off even under `Debug`, unless `Debug` names it. A five-thousand-row page is five thousand
/// of those, and they are the reason a diagnostic build was measurably slower at the one thing
/// the user notices, which turned "Everyfind is slow sometimes" into a real report that was
/// really about the diagnostics. The interesting lines (`ROWS`, `TIMING`, `SITE`, `QUERY`) are
/// per *search*, and stay.
pub(crate) fn logging_rows() -> bool {
    ROW_LOG.load(Ordering::Relaxed)
}

fn fmt_guid(g: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7],
    )
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A string value under this CLSID's `InprocServer32` key, where the installer
/// leaves out-of-band settings for the shim (`RealDll`, `Deny`).
fn reg_str(clsid: &GUID, name: &str) -> Option<String> {
    let subkey = format!(r"Software\Classes\CLSID\{}\InProcServer32", fmt_guid(clsid));
    let sub = wide(&subkey);
    let val = wide(name);
    let mut buf = [0u16; 1024];
    let mut cb = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut cb,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    // `RegGetValueW` reports the size it wrote, which can exceed the string: it guarantees
    // termination and pads to do so (a 30-character value comes back as 66 bytes, two more
    // than the string plus one NUL needs). Trimming one unit would leave NULs embedded in
    // the path we then hand to LoadLibraryW. Cut at the first NUL; that is where the
    // string ends by definition.
    let units = &buf[..(cb as usize / 2).min(buf.len())];
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

/// The original DLL the installer recorded for this CLSID.
fn recorded_real_dll(clsid: &GUID) -> Option<String> {
    reg_str(clsid, "RealDll")
}

/// Load the genuine server for `clsid`: the recorded original DLL if present,
/// else windows.storage.dll as a reasonable default for the search classes.
/// Loaded by full path, so the load never consults the registry and never comes
/// back to us.
fn real_server(clsid: &GUID) -> (HMODULE, String) {
    if let Some(path) = recorded_real_dll(clsid) {
        let h = unsafe { LoadLibraryW(wide(&path).as_ptr()) };
        if !h.is_null() {
            return (h, path);
        }
    }
    let windir = std::env::var("windir").unwrap_or_else(|_| r"C:\Windows".into());
    let path = format!(r"{windir}\System32\Windows.Storage.Search.dll");
    (unsafe { LoadLibraryW(wide(&path).as_ptr()) }, path)
}

pub(crate) fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ======================================================================
// OLE DB logging wrapper for the Windows Search data source (CollatorDSO).
//
// We forward `DllGetClassObject` to the real tquery.dll (transparent), then walk
// the OLE DB object graph the search folder drives: factory -> data source ->
// session -> command -> rowset, patching one vtable slot at each step to log and
// forward. Nothing the search returns changes; the point is to read the exact
// conversation (the query text, and whether the displayed rows really come from a
// rowset produced here) before deciding how to substitute results.
// ======================================================================

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    }
}

/// The engines whose OLE DB conversation we follow.
///
/// Explorer answers a search with one of two: the indexed data source, or a
/// filesystem walk where the index does not reach. Only the first is followed
/// here. The second is registered too (so it is forwarded through this DLL and
/// the hook point stays ready), but following it as well made both engines share
/// this shim's per-search state and the query capture picked up the wrong one,
/// its results are still Windows'.
const SEARCH_ENGINES: &[GUID] = &[
    // Search.CollatorDSO (tquery.dll): indexed locations.
    guid(
        0x9E17_5B8B,
        0xF52A,
        0x11D8,
        [0xB9, 0xA5, 0x50, 0x50, 0x54, 0x50, 0x30, 0x30],
    ),
];

const IID_ICLASSFACTORY: GUID = guid(0x0000_0001, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_IUNKNOWN: GUID = guid(0x0000_0000, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
// OLE DB core IIDs (all ...-2A1C-11CE-ADE5-00AA0044773D).
const OLEDB_TAIL: [u8; 8] = [0xAD, 0xE5, 0x00, 0xAA, 0x00, 0x44, 0x77, 0x3D];
const IID_IDBINITIALIZE: GUID = guid(0x0C73_3A8B, 0x2A1C, 0x11CE, OLEDB_TAIL);
const IID_IDBCREATESESSION: GUID = guid(0x0C73_3A5D, 0x2A1C, 0x11CE, OLEDB_TAIL);
const IID_IDBCREATECOMMAND: GUID = guid(0x0C73_3A1D, 0x2A1C, 0x11CE, OLEDB_TAIL);
const IID_ICOMMANDTEXT: GUID = guid(0x0C73_3A27, 0x2A1C, 0x11CE, OLEDB_TAIL);
const IID_ICOMMAND: GUID = guid(0x0C73_3A63, 0x2A1C, 0x11CE, OLEDB_TAIL);
/// Whether this command implements ICommandText. The indexed engine's does and
/// its GetCommandText is worth asking; the filesystem-walk engine's does not, and
/// reading slot 6 there would run off the end of the vtable.
static HAS_CMDTEXT: AtomicBool = AtomicBool::new(false);
// The three undocumented interfaces the search folder actually drives the rowset
// with. `Deny` (a REG_SZ on the shim's key) makes the shim answer E_NOINTERFACE
// for them, to find out whether the folder falls back to the public IRowset path.
const IID_HOTFETCH: GUID = guid(0x0C73_3AAF, 0x2A1C, 0x11CE, OLEDB_TAIL);
const IID_WSPRIVATE: GUID = guid(
    0x4281_1652,
    0x079D,
    0x481B,
    [0x87, 0xA2, 0x09, 0xA6, 0x9E, 0xCC, 0x5F, 0x44],
);
// IRowsetLocate is a *separate* vtable on the same object, so patching IRowset's
// GetNextRows never sees calls made through this pointer, which is why the
// public row-fetch path looked dead.
const IID_IROWSETLOCATE: GUID = guid(0x0C73_3A7D, 0x2A1C, 0x11CE, OLEDB_TAIL);
// One real IRowsetInfo (schema/capabilities are query-independent), obtained by
// running the native query exactly once and reused forever. This is what lets a
// swapped search skip the native index query entirely on every keystroke.
const DENY_HOTFETCH: usize = 1;
const DENY_NOTIFY: usize = 2;
const DENY_WSPRIVATE: usize = 4;
static DENY: AtomicUsize = AtomicUsize::new(0);
// `Swap` (REG_SZ on the shim's key) replaces the real rowset with ours:
// 'fixed' = a few hardcoded files (mechanism test), 'Everyfind' = real results.
const SWAP_OFF: usize = 0;
const SWAP_FIXED: usize = 1;
const SWAP_EVERYFIND: usize = 2;
static SWAP: AtomicUsize = AtomicUsize::new(0);
// Diagnostics (the vtable-logging harness and the %TEMP% log) are off unless the
// `Debug` value is set on the shim's key, so a real install is silent and lean.
static DEBUG: AtomicBool = AtomicBool::new(false);
// Per-row logging, enabled only by `Debug=rows`; see `logging_rows`.
static ROW_LOG: AtomicBool = AtomicBool::new(false);
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const CONNECT_E_NOCONNECTION: i32 = 0x8004_0200u32 as i32;

// IConnectionPointContainer: the folder QIs this on the rowset to subscribe to
// async row-available notifications. FindConnectionPoint's riid names the sink IF.
const IID_ICPC: GUID = guid(
    0xB196_B284,
    0xBAB4,
    0x101A,
    [0xB6, 0x9C, 0x00, 0xAA, 0x00, 0x34, 0x1D, 0x07],
);

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// Saved (slot address, original pointer) for each patched vtable slot, so unload
// can put them all back.
// Which slot of a vtable each hook sits in, so a hook can find the function it
// replaced (see `install` / `orig_of`). No "have we done this yet" flags: the
// slot table itself makes every installation idempotent, and a process that
// meets both search engines needs each of them wrapped, not just the first.

type QiFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type InitFn = unsafe extern "system" fn(*mut c_void) -> i32;
type CreateSubFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> i32;
type SetCmdFn = unsafe extern "system" fn(*mut c_void, *const GUID, *const u16) -> i32;
type ExecFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const GUID,
    *mut c_void,
    *mut isize,
    *mut *mut c_void,
) -> i32;
type GetNextFn =
    unsafe extern "system" fn(*mut c_void, usize, isize, isize, *mut usize, *mut *mut usize) -> i32;
type GetDataFn = unsafe extern "system" fn(*mut c_void, usize, usize, *mut c_void) -> i32;
type GetRowFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    usize,
    *const GUID,
    *mut *mut c_void,
) -> i32;
type GetUrlFn = unsafe extern "system" fn(*mut c_void, usize, *mut *mut u16) -> i32;
type GetRowsAtFn = unsafe extern "system" fn(
    *mut c_void,
    usize,
    usize,
    usize,
    *const u8,
    isize,
    isize,
    *mut usize,
    *mut *mut usize,
) -> i32;
static GETDATA_COUNT: AtomicUsize = AtomicUsize::new(0);
static EXEC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Patch the class factory's `CreateInstance` (vtable slot 4) to begin the walk.
unsafe fn wrap_factory(factory: *mut c_void) {
    // IClassFactory: QueryInterface(0) AddRef(1) Release(2) CreateInstance(3) LockServer(4).
    install(factory, 3, hook_createinstance as *const () as usize);
    log("OLEDB: patched IClassFactory::CreateInstance(slot 3)");
}

// IClassFactory::CreateInstance(pUnkOuter, riid, ppvObject)
unsafe extern "system" fn hook_createinstance(
    this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> i32 {
    let f: CreateSubFn = std::mem::transmute(orig_of(this, 3));
    let hr = f(this, outer, riid, ppv);
    // IDBCreateSession is only exposed after IDBInitialize::Initialize, so we
    // hook Initialize and walk on once the data source is live.
    if hr == 0 && !ppv.is_null() && !(*ppv).is_null() {
        let init = qi(*ppv, &IID_IDBINITIALIZE);
        if !init.is_null() {
            // IDBInitialize: QI(0) AddRef(1) Release(2) Initialize(3) Uninitialize(4).
            install(init, 3, hook_initialize as *const () as usize);
            release(init);
            log("OLEDB: patched IDBInitialize::Initialize");
        } else {
            log("OLEDB: (created object has no IDBInitialize)");
        }
    }
    hr
}

// IDBInitialize::Initialize(); after it succeeds the session interface exists.
unsafe extern "system" fn hook_initialize(this: *mut c_void) -> i32 {
    let f: InitFn = std::mem::transmute(orig_of(this, 3));
    let hr = f(this);
    if hr == 0 {
        let s = qi(this, &IID_IDBCREATESESSION);
        if !s.is_null() {
            install(s, 3, hook_createsession as *const () as usize);
            release(s);
            log("OLEDB: patched IDBCreateSession::CreateSession");
        } else {
            log("OLEDB: (still no IDBCreateSession after Initialize)");
        }
    }
    hr
}

// IDBCreateSession::CreateSession(pUnkOuter, riid, ppDBSession)
unsafe extern "system" fn hook_createsession(
    this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    ppsess: *mut *mut c_void,
) -> i32 {
    let f: CreateSubFn = std::mem::transmute(orig_of(this, 3));
    let hr = f(this, outer, riid, ppsess);
    if hr == 0 && !ppsess.is_null() && !(*ppsess).is_null() {
        let c = qi(*ppsess, &IID_IDBCREATECOMMAND);
        if !c.is_null() {
            install(c, 3, hook_createcommand as *const () as usize);
            release(c);
            log("OLEDB: patched IDBCreateCommand::CreateCommand");
        } else {
            log("OLEDB: (session has no IDBCreateCommand)");
        }
    }
    hr
}

// IDBCreateCommand::CreateCommand(pUnkOuter, riid, ppCommand)
unsafe extern "system" fn hook_createcommand(
    this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    ppcmd: *mut *mut c_void,
) -> i32 {
    let f: CreateSubFn = std::mem::transmute(orig_of(this, 3));
    let hr = f(this, outer, riid, ppcmd);
    if hr == 0 && !ppcmd.is_null() && !(*ppcmd).is_null() {
        // The query interface and the site both arrive through the command's QI,
        // whatever kind of command this is.
        install(*ppcmd, 0, hook_cmd_qi as *const () as usize);
        // Execute is slot 4 of ICommand. ICommandText only extends that with
        // GetCommandText(6)/SetCommandText(7), and the filesystem-walk engine's
        // command implements the plain interface, so fall back to it.
        let ct = qi(*ppcmd, &IID_ICOMMANDTEXT);
        let cmd = if ct.is_null() {
            qi(*ppcmd, &IID_ICOMMAND)
        } else {
            HAS_CMDTEXT.store(true, Ordering::SeqCst);
            ct
        };
        if !cmd.is_null() {
            install(cmd, 4, hook_execute as *const () as usize);
            if !ct.is_null() {
                install(cmd, 7, hook_setcommandtext as *const () as usize);
            }
            release(cmd);
            log(&format!(
                "OLEDB: patched Execute(4) on {}",
                if ct.is_null() {
                    "ICommand"
                } else {
                    "ICommandText"
                }
            ));
        } else {
            log("OLEDB: (command has neither ICommandText nor ICommand)");
        }
    }
    hr
}

static CQI_COUNT: AtomicUsize = AtomicUsize::new(0);

// The private interface the shell sets the query on, before Execute. Capturing
// the query here means a swap never has to run the native query to learn it.
const IID_QUERYIF: GUID = guid(
    0x0D96_FC4E,
    0x2EE9,
    0x47A4,
    [0x94, 0x59, 0x67, 0x77, 0x1A, 0xD5, 0x7B, 0xA3],
);
const IID_ICONDITION: GUID = guid(
    0x0FC9_88D4,
    0xC935,
    0x4B97,
    [0xA9, 0x73, 0x46, 0x28, 0x2E, 0xA1, 0x75, 0xC8],
);
// The current ICondition the shell handed the query interface (AddRef'd), per thread.
//
// Per thread, and not behind a process-wide lock, because of what has to happen when it is
// replaced: the old one is `Release`d, and `Release` on an object owned by another apartment
// is a marshalled call that returns only when that apartment pumps. Held under a shared mutex
// that call is a deadlock with no way out, and the way out is the one thing the shell cannot
// do, because the apartment it is waiting for is usually the one blocked in *our* `Execute`.
//
// Caught live, and it is what "処理中 that never finishes" actually was: `png` typed after
// `po`, the log stopping dead after `SetProperties` with no `COND` line after it, explorer.exe
// idle at 0.00 s of CPU over three seconds and still Responding; every search in the process
// blocked on `CONDITION.lock()`, for a lock whose holder was inside a COM call that would
// never return. Not slow. Stopped, permanently, until Explorer restarts.
//
// A thread-local removes the sharing rather than guarding it: the shell sets the condition on
// a command and executes that command on the same thread, so nothing crosses. It also settles
// the other half of S7 (two windows can no longer read each other's term) and if some shell
// ever does split the two across threads, the cost is a term we do not find and the
// `search-ms:` URL fallback in `query_for` answers instead. A worse answer, not a stopped one.
thread_local! {
    static CONDITION: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

unsafe fn addref(obj: *mut c_void) {
    let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vslot(obj, 1));
    f(obj);
}

/// Remember the latest `ICondition` the shell sets. It arrives as the 4th
/// argument of one method (measured), so we look only there, validated as a COM
/// pointer first, instead of QueryInterfacing every argument of every call.
unsafe fn scan_for_condition(a4: usize) {
    if a4 == 0 || !looks_like_com(a4) {
        return;
    }
    let cond = qi(a4 as *mut c_void, &IID_ICONDITION);
    if cond.is_null() {
        return;
    }
    // Swap first, release after: nothing here may be inside anything another thread waits on,
    // and `Release` can block for as long as the owning apartment takes to pump.
    let previous = CONDITION.with(|c| c.replace(cond as usize)); // kept AddRef'd until replaced
    if previous != 0 {
        release(previous as *mut c_void);
    }
}

/// The current query (term, scope) read from the captured condition, taking a ref
/// so a concurrent update cannot free it mid-walk.
/// The search this command is about to run: the term the shell set on it, and the folder it
/// was started from.
///
/// The folder has two possible readings and needs both. *This* command's own site is the one
/// that is right per window: one Explorer process serves several windows, so a scope
/// remembered process-wide can be a search old. But the site is frequently not there at all
/// (measured: `QueryInterface(IObjectWithSite)` on the command returns null), and then the
/// only reading left is [`SCOPE`], captured from the property sets. Preferring the first and
/// falling back to the second is not belt-and-braces; removing the fallback once turned every
/// Explorer search into a Windows search.
unsafe fn query_for(cmd: *mut c_void) -> Option<(String, String)> {
    // Preferred: the condition the shell set on the command, which is the query itself rather
    // than a rendering of it. Read first, but not required; see the fallback below.
    let condition_term = captured_term();

    // Preferred: the site this very command was given. It answers both halves: where the
    // search started and, when the URL spelled them, what was typed, and it answers them as
    // one value, so nothing that arrives here belongs to another window's search.
    let mut from_site = None;
    let ows = qi(cmd, &IID_IOBJECTWITHSITE);
    if !ows.is_null() {
        // IObjectWithSite: SetSite(3), GetSite(4).
        let get_site: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32 =
            std::mem::transmute(*vslot(ows, 4));
        let mut site: *mut c_void = std::ptr::null_mut();
        if get_site(ows, &IID_IUNKNOWN, &mut site) == 0 && !site.is_null() {
            from_site = provider::scope_from_site(site);
            release(site);
        }
        release(ows);
    }

    // Fallback: what the command's properties carried.
    let scope = from_site.as_ref().map(|s| s.scope.clone()).or_else(|| {
        SCOPE
            .lock()
            .ok()
            .map(|g| {
                g.trim_start_matches("file:")
                    .replace('/', "\\")
                    .trim_end_matches('\\')
                    .to_string()
            })
            .filter(|s| !s.is_empty())
    });
    // An unknown folder is not a failure. The shell frequently names none. Measured: no site
    // on the command, no location in the condition tree, nothing recognisable in the property
    // sets, and the search is then simply volume-wide, which is what this integration has
    // always done. Treating "no folder" as "cannot answer" turns every search into a Windows
    // search, which is exactly what it did until this line was written.
    let scope = scope.unwrap_or_default();
    if scope.is_empty() {
        log("QUERY: no folder named for this search; searching the whole volume");
    }

    // The one place this integration depends on something undocumented is the private
    // interface the condition arrives through: its IID and the slot the shell sets it on were
    // found by measurement, and nothing promises another Windows spells them the same way.
    // The `search-ms:` URL is the second reading of the same search; it names both the folder
    // and the terms, it is a documented format, and it was already read for the folder a few
    // lines up. Falling back to it costs nothing and turns the single undocumented dependency
    // into a pair, either of which is enough to answer.
    let term = match condition_term {
        Some(t) => t,
        None => match from_site.and_then(|s| s.term) {
            Some(t) => {
                log(&format!("QUERY: term from the search URL = '{t}'"));
                t
            }
            None => {
                // Last resort: the term captured at the most recent SetSite (see TERM). On
                // builds where the command's own IObjectWithSite is null this is the only
                // reading left, and without it every Explorer search there silently defers to
                // Windows.
                match TERM.lock().ok().map(|g| g.clone()).filter(|t| !t.trim().is_empty()) {
                    Some(t) => {
                        log(&format!("QUERY: term from the SetSite capture = '{t}'"));
                        t
                    }
                    None => {
                        log("QUERY: no term in the condition, the search URL, or the site");
                        return None;
                    }
                }
            }
        },
    };
    Some((term, scope))
}

/// Report the host once, for the log.
///
/// This used to gate the substitution: only `explorer.exe` was answered, on the reasoning that
/// the CLSID override is per *user* and every program that queries Windows Search reaches this
/// DLL. That was the wrong test, and it broke other programs. Declaring the drive in the crawl
/// scope changes which engine Windows picks *for everyone*, so a file dialog's search box now
/// goes to the indexed engine, and being turned away here left it waiting on a catalog that
/// holds nothing for those paths. Measured: a folder dialog's search hung and returned
/// nothing, where before it walked the filesystem and answered.
///
/// The real distinction is not which program is asking but *how*. The shell's search UI hands
/// the term over as an `ICondition` on a private interface; a program driving OLE DB itself
/// sets SQL text instead and never builds one. So requiring a captured condition, which
/// `query_for` already does, is the test, and it costs nothing to make.
fn report_host() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let mut buf = [0u16; 260];
        let n = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        } as usize;
        let exe = String::from_utf16_lossy(&buf[..n.min(buf.len())]).to_lowercase();
        let host = exe.rsplit(char::from(92)).next().unwrap_or("").to_string();
        log(&format!("HOST: {host}"));
    });
}

/// The term from the condition the shell last set, taking a ref so a concurrent update
/// cannot free it mid-walk. The condition tree carries the term only; the folder is the
/// site's job (see [`query_for`]).
fn captured_term() -> Option<String> {
    let raw = CONDITION.with(|c| c.get());
    if raw == 0 {
        return None;
    }
    // Our own ref for the walk. Nothing else touches this thread's slot, but walking the tree
    // calls back into the shell, and the shell can reach our hooks again on this same thread,
    // which would replace the pointer and release it under us.
    unsafe { addref(raw as *mut c_void) };
    let r = unsafe { provider::extract_query(raw as *mut c_void) };
    unsafe { release(raw as *mut c_void) };
    r.map(|(term, _scope)| term)
}

const IID_IOBJECTWITHSITE: GUID = guid(
    0xFC48_01A3,
    0x2BA9,
    0x11CF,
    [0xA2, 0x29, 0x00, 0xAA, 0x00, 0x3D, 0x73, 0x52],
);
const IID_ICOMMANDPROPERTIES: GUID = guid(0x0C73_3A79, 0x2A1C, 0x11CE, OLEDB_TAIL);

/// The folder the user searched from.
///
/// This is where it comes from, and it is the only place it comes from. Measured, after an
/// attempt to take it from the command's site instead turned every search into a Windows
/// search: the command has **no `IObjectWithSite`** (`QueryInterface` for it returns null at
/// Execute time), the condition tree carries the term and the filter flags but no location,
/// and `ICommandText::GetCommandText` only has an answer *after* Execute, which is the native
/// query this whole design exists to avoid running. What is left is the property sets the
/// shell hands the command, which do carry it.
static SCOPE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The typed term, captured at [`hook_setsite`] and read back at Execute as a last resort.
///
/// Process-wide for the same reason [`SCOPE`] is: on builds where `query_for`'s own
/// `QueryInterface(IObjectWithSite)` returns null (measured on 26200), the term is only ever in
/// hand at SetSite. It cannot be keyed to the executing command - the object SetSite is called
/// on and the `ICommandText` Execute runs on do not share a COM identity here (measured: their
/// `IUnknown` pointers differ), so there is nothing stable to key by. It is therefore a plain
/// latest-value, updated only when a SetSite actually names a term (never cleared by a term-less
/// folder navigation, which does not run a content query anyway), so the value standing at
/// Execute is the most recent search the shell set up.
static TERM: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// `IObjectWithSite::SetSite(pUnkSite)` on the command, where the searched folder comes from.
///
/// It has to be taken here, when the shell hands the site over, and not at Execute time by
/// asking the command for its own site: `QueryInterface(IObjectWithSite)` on the interface
/// Execute is called through returns null (measured), even though the same query against the
/// command object itself succeeds. So the site is read while we are holding it.
unsafe extern "system" fn hook_setsite(this: *mut c_void, site: *mut c_void) -> i32 {
    if !site.is_null() && looks_like_com(site as usize) {
        let named = com::guard("SetSite", None, || {
            provider::dump_site(site);
            provider::scope_from_site(site)
        });
        match named {
            // The folder is kept process-wide (see SCOPE). The term is too, but only when this
            // site actually named one: a term-less folder navigation must not wipe the standing
            // search term (it runs no content query), and the next real search overwrites it.
            Some(found) => {
                log(&format!("SITE: scope = '{}'", found.scope));
                if let Ok(mut g) = SCOPE.lock() {
                    *g = found.scope;
                }
                if let Some(t) = found.term.as_ref().filter(|t| !t.trim().is_empty()) {
                    if let Ok(mut g) = TERM.lock() {
                        *g = t.clone();
                    }
                }
            }
            None => log("SITE: the site named no folder"),
        }
    }
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32 =
        std::mem::transmute(orig_of(this, 3));
    f(this, site)
}

/// `ICommandProperties::SetProperties(cPropertySets, rgPropertySets)`.
///
/// Kept for what it shows in the log, not for what it finds: measured, the shell passes one
/// property set of four values here (two booleans and two GUID strings) and no location at
/// all. This is not where the scope comes from (see [`hook_setsite`]).
unsafe extern "system" fn hook_setproperties(
    this: *mut c_void,
    csets: u32,
    rgsets: *const u8,
) -> i32 {
    // DBPROPSET { DBPROP* rgProperties; ULONG cProperties; GUID guidPropertySet }
    // DBPROP   { DBPROPID; DBPROPOPTIONS; DBPROPSTATUS; DBID colid; VARIANT vValue }
    // Both strides are the real x64 layout, not guesses: DBPROPSET is ptr(8) + ULONG(4) +
    // pad(4) + GUID(16); DBPROP is three ULONGs padded to 16, then DBID (GUID 16 + eKind 4 +
    // pad 4 + union 8 = 32), then VARIANT (24). The VARIANT therefore starts at 48, its type
    // tag is the first u16 there, and its pointer payload is eight bytes further in.
    const SET_STRIDE: usize = 32;
    const PROP_STRIDE: usize = 72;
    const VALUE_OFF: usize = 48;
    const VT_BSTR: u16 = 8;
    const VT_LPWSTR: u16 = 31;

    if !rgsets.is_null() && csets > 0 && csets < 32 {
        for i in 0..csets as usize {
            let set = rgsets.add(i * SET_STRIDE);
            if !mem_readable(set as usize, SET_STRIDE) {
                break;
            }
            let props = *(set as *const usize);
            let cprops = *(set.add(8) as *const u32);
            let g = &*(set.add(16) as *const GUID);
            if props == 0 || cprops == 0 || cprops > 64 {
                continue;
            }
            if debugging() {
                log(&format!(
                    "PROPS: set {} with {cprops} properties",
                    fmt_guid(g)
                ));
            }
            for j in 0..cprops as usize {
                let p = props + j * PROP_STRIDE;
                if !mem_readable(p, PROP_STRIDE) {
                    break;
                }
                let id = *(p as *const u32);
                let vt = *((p + VALUE_OFF) as *const u16);
                let payload = *((p + VALUE_OFF + 8) as *const usize);
                let text = if (vt == VT_BSTR || vt == VT_LPWSTR) && payload > 0x1_0000 {
                    read_wide_in_region(payload)
                } else {
                    None
                };
                if debugging() {
                    log(&format!(
                        "PROPS:   id={id} vt={vt} value={:?}",
                        text.as_deref().unwrap_or("")
                    ));
                }
                // A string that names a location is the scope, whichever property carries it;
                // which one that is, is not documented anywhere we could find.
                if let Some(s) = text {
                    if s.len() > 3
                        && s.len() < 400
                        && (s.starts_with("file:") || s.contains(":\\") || s.contains(":/"))
                    {
                        log(&format!("PROPS: candidate scope '{s}'"));
                        if let Ok(mut g) = SCOPE.lock() {
                            *g = s;
                        }
                    }
                }
            }
        }
    }
    let f: unsafe extern "system" fn(*mut c_void, u32, *const u8) -> i32 =
        std::mem::transmute(orig_of(this, 4));
    f(this, csets, rgsets)
}

// Thunk for the one method that carries the ICondition (slot 4). Its 4th argument
// (a4) is the condition; we watch that and forward everything unchanged.
unsafe extern "system" fn qif_t4(
    this: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> usize {
    scan_for_condition(a4);
    let f: unsafe extern "system" fn(usize, usize, usize, usize, usize, usize) -> usize =
        std::mem::transmute(orig_of(this as *mut c_void, 4));
    f(this, a1, a2, a3, a4, a5)
}

// QueryInterface on the command object: reveals which interface carries the
// query (ICommandTree? a private one?), since SetCommandText is never called.
unsafe extern "system" fn hook_cmd_qi(
    this: *mut c_void,
    iid: *const GUID,
    ppv: *mut *mut c_void,
) -> i32 {
    let f: QiFn = std::mem::transmute(orig_of(this, 0));
    let hr = f(this, iid, ppv);
    if CQI_COUNT.fetch_add(1, Ordering::SeqCst) < 40 {
        log(&format!(
            "OLEDB: cmd QI {} -> hr=0x{:08X}",
            iid.as_ref().map(fmt_guid).unwrap_or_default(),
            hr as u32
        ));
    }
    // The query arrives as an ICondition argument to one of this interface's methods before
    // Execute; slot 4 is the one that carries it (measured). Goes through the slot table like
    // every other hook, so `restore_patches` puts it back on unload and a second instance of
    // the class does not patch a slot that already holds our thunk.
    if hr == 0
        && !ppv.is_null()
        && !(*ppv).is_null()
        && iid
            .as_ref()
            .map(|g| guid_eq(g, &IID_QUERYIF))
            .unwrap_or(false)
        && install(*ppv, 4, qif_t4 as *const () as usize)
    {
        log("QIF: watching {0D96FC4E} slot4 for the ICondition");
    }
    // The searched folder arrives through the command's properties; see `SCOPE`.
    if hr == 0
        && !ppv.is_null()
        && !(*ppv).is_null()
        && iid
            .as_ref()
            .map(|g| guid_eq(g, &IID_ICOMMANDPROPERTIES))
            .unwrap_or(false)
        // ICommandProperties: GetProperties(3), SetProperties(4).
        && install(*ppv, 4, hook_setproperties as *const () as usize)
    {
        log("PROPS: watching ICommandProperties::SetProperties");
    }
    // The site the shell attaches to the command leads to the folder that was searched.
    if hr == 0
        && !ppv.is_null()
        && !(*ppv).is_null()
        && iid
            .as_ref()
            .map(|g| guid_eq(g, &IID_IOBJECTWITHSITE))
            .unwrap_or(false)
        // IObjectWithSite: SetSite(3), GetSite(4).
        && install(*ppv, 3, hook_setsite as *const () as usize)
    {
        log("SITE: watching IObjectWithSite::SetSite for the scope");
    }
    hr
}

// ICommandText::SetCommandText(rguidDialect, pwszCommand)
unsafe extern "system" fn hook_setcommandtext(
    this: *mut c_void,
    dialect: *const GUID,
    pwsz: *const u16,
) -> i32 {
    log(&format!("OLEDB: *** SetCommandText: {}", read_wide(pwsz)));
    let f: SetCmdFn = std::mem::transmute(orig_of(this, 7));
    f(this, dialect, pwsz)
}

// ICommand::Execute(pUnkOuter, riid, pParams, pcRowsAffected, ppRowset)
unsafe extern "system" fn hook_execute(
    this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    pparams: *mut c_void,
    pcrows: *mut isize,
    pprowset: *mut *mut c_void,
) -> i32 {
    let f: ExecFn = std::mem::transmute(orig_of(this, 4));
    let n = EXEC_COUNT.fetch_add(1, Ordering::SeqCst);
    let mode = SWAP.load(Ordering::SeqCst);
    let want_swap = mode != SWAP_OFF && !pprowset.is_null();

    if want_swap {
        report_host();
        // The query was captured from the shell's ICondition before Execute, so we
        // read the term and scope with no native query at all.
        let t0 = now_ms();
        let answer = com::guard(
            "Execute",
            Answer::Defer("a panic while working out the answer"),
            || {
                if mode == SWAP_FIXED {
                    Answer::Rows(provider::fixed_rows())
                } else {
                    match query_for(this) {
                        Some((q, scope)) => answer_for(&q, &scope),
                        None => Answer::Defer("no search term or folder captured for this command"),
                    }
                }
            },
        );
        match answer {
            Answer::Rows(rows) => {
                // The view asks the rowset to describe itself before it will show it.
                // Our rowset answers that from what the genuine provider was measured
                // to say, so no native query has to run at all; that query was the
                // 5-second wait on the first search of a window.
                let count = rows.len();
                let code = provider::make_rowset(rows, riid as *const c_void, pprowset);
                if code >= 0 && !(*pprowset).is_null() {
                    log(&format!(
                        "TIMING n={n} total={}ms rows={count}",
                        now_ms() - t0
                    ));
                    return code;
                }
                log(&format!(
                    "SWAP: make_rowset failed code=0x{:08X}",
                    code as u32
                ));
            }
            // Deferring is the safe direction, but it is also invisible from the outside,
            // the window just fills with Windows' results. Naming the reason is the only way
            // to tell "Everyfind found nothing" apart from "Everyfind never got to look".
            Answer::Defer(why) => log(&format!("SWAP: deferring to Windows - {why}")),
        }
        // Nothing to substitute; fall through to a genuine query.
    }

    let hr = f(this, outer, riid, pparams, pcrows, pprowset);
    if hr >= 0 && !pprowset.is_null() && !(*pprowset).is_null() {
        let target = *pprowset;
        install(target, 5, hook_getnextrows as *const () as usize);
        install(target, 4, hook_getdata as *const () as usize);
        install(target, 0, hook_rowset_qi as *const () as usize);
        log("OLEDB: patched IRowset GetNextRows(5)+GetData(4)+QI(0)");
    }
    hr
}

static RQI_COUNT: AtomicUsize = AtomicUsize::new(0);

// IUnknown::QueryInterface on the rowset: log what the folder asks it for.
unsafe extern "system" fn hook_rowset_qi(
    this: *mut c_void,
    iid: *const GUID,
    ppv: *mut *mut c_void,
) -> i32 {
    let f: QiFn = std::mem::transmute(orig_of(this, 0));
    let hr = f(this, iid, ppv);
    // Denial experiment: hide an interface the folder prefers and see whether it
    // falls back to the public IRowset::GetNextRows/GetData path. If it does, a
    // replacement provider only has to implement documented OLE DB.
    if hr == 0 && !ppv.is_null() && !(*ppv).is_null() {
        if let Some(g) = iid.as_ref() {
            let deny = DENY.load(Ordering::SeqCst);
            let hit = (guid_eq(g, &IID_HOTFETCH) && deny & DENY_HOTFETCH != 0)
                || (guid_eq(g, &IID_WSPRIVATE) && deny & DENY_WSPRIVATE != 0)
                || (guid_eq(g, &IID_ICPC) && deny & DENY_NOTIFY != 0);
            if hit {
                release(*ppv);
                *ppv = std::ptr::null_mut();
                if RQI_COUNT.fetch_add(1, Ordering::SeqCst) < 40 {
                    log(&format!("OLEDB: rowset QI {} -> DENIED", fmt_guid(g)));
                }
                return E_NOINTERFACE;
            }
        }
    }
    let n = RQI_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < 40 {
        log(&format!(
            "OLEDB: rowset QI {} -> hr=0x{:08X} ptr={:?} (this={this:?})",
            iid.as_ref().map(fmt_guid).unwrap_or_default(),
            hr as u32,
            if ppv.is_null() {
                std::ptr::null_mut()
            } else {
                *ppv
            },
        ));
    }
    // Patch the interfaces the folder really drives, on *their own* vtables.
    if hr == 0 && !ppv.is_null() && !(*ppv).is_null() {
        if let Some(g) = iid.as_ref() {
            let p = *ppv;
            if guid_eq(g, &IID_HOTFETCH) {
                install(p, 3, hook_getrow_fromhrow as *const () as usize);
                install(p, 4, hook_geturl_fromhrow as *const () as usize);
                log("OLEDB: patched IGetRow::GetRowFromHROW(3)+GetURLFromHROW(4)");
            }
            if guid_eq(g, &IID_IROWSETLOCATE) {
                install(p, 5, hook_loc_getnextrows as *const () as usize);
                install(p, 9, hook_getrowsat as *const () as usize);
                log("OLEDB: patched IRowsetLocate::GetNextRows(5)+GetRowsAt(9)");
            }
        }
    }
    // When the folder gets IConnectionPointContainer, hook FindConnectionPoint on
    // it to learn which notify interface it subscribes to (the async contract).
    if hr == 0
        && !ppv.is_null()
        && !(*ppv).is_null()
        && iid.as_ref().map(|g| guid_eq(g, &IID_ICPC)).unwrap_or(false)
    {
        let cpc = *ppv;
        // IConnectionPointContainer: QI(0) AddRef(1) Release(2) EnumCPs(3) FindCP(4).
        install(cpc, 4, hook_cpc_find as *const () as usize);
        log("OLEDB: patched IConnectionPointContainer::FindConnectionPoint(4)");
    }
    hr
}

// IConnectionPointContainer::FindConnectionPoint(riid, ppCP): the riid is the
// async notify sink the folder wants (IRowsetNotify / IDBAsynchNotify / ...).
unsafe extern "system" fn hook_cpc_find(
    this: *mut c_void,
    riid: *const GUID,
    ppcp: *mut *mut c_void,
) -> i32 {
    if DENY.load(Ordering::SeqCst) & DENY_NOTIFY != 0 {
        if !ppcp.is_null() {
            *ppcp = std::ptr::null_mut();
        }
        log("OLEDB: FindConnectionPoint -> DENIED");
        return CONNECT_E_NOCONNECTION;
    }
    let f: QiFn = std::mem::transmute(orig_of(this, 4));
    let hr = f(this, riid, ppcp);
    log(&format!(
        "OLEDB: FindConnectionPoint riid={} -> hr=0x{:08X}",
        riid.as_ref().map(fmt_guid).unwrap_or_default(),
        hr as u32
    ));
    hr
}

// IRowset::GetData(hRow, hAccessor, pData): how the folder reads each column.
unsafe extern "system" fn hook_getdata(
    this: *mut c_void,
    hrow: usize,
    haccessor: usize,
    pdata: *mut c_void,
) -> i32 {
    let f: GetDataFn = std::mem::transmute(orig_of(this, 4));
    let hr = f(this, hrow, haccessor, pdata);
    let n = GETDATA_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < 3 {
        log(&format!(
            "OLEDB: GetData call #{n} hRow={hrow:#x} hr=0x{:08X}",
            hr as u32
        ));
    }
    hr
}

// IRowset::GetNextRows(hChapter, lRowsOffset, cRows, pcRowsObtained, prghRows)
unsafe extern "system" fn hook_getnextrows(
    this: *mut c_void,
    hchapter: usize,
    loffset: isize,
    crows: isize,
    pcobtained: *mut usize,
    prghrows: *mut *mut usize,
) -> i32 {
    let f: GetNextFn = std::mem::transmute(orig_of(this, 5));
    let hr = f(this, hchapter, loffset, crows, pcobtained, prghrows);
    let got = if pcobtained.is_null() { 0 } else { *pcobtained };
    log(&format!(
        "OLEDB: GetNextRows req={crows} got={got} hr=0x{:08X}",
        hr as u32
    ));
    hr
}

static GETROW_COUNT: AtomicUsize = AtomicUsize::new(0);
static GETURL_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOCFETCH_COUNT: AtomicUsize = AtomicUsize::new(0);

// IGetRow::GetRowFromHROW(pUnkOuter, hRow, riid, ppRow): turns a row handle into
// an IRow the shell can read properties from.
unsafe extern "system" fn hook_getrow_fromhrow(
    this: *mut c_void,
    outer: *mut c_void,
    hrow: usize,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> i32 {
    let f: GetRowFn = std::mem::transmute(orig_of(this, 3));
    let hr = f(this, outer, hrow, riid, ppv);
    let n = GETROW_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < 5 {
        log(&format!(
            "OLEDB: >>> GetRowFromHROW #{n} hRow={hrow:#x} riid={} hr=0x{:08X}",
            riid.as_ref().map(fmt_guid).unwrap_or_default(),
            hr as u32
        ));
    }
    // Read back what the genuine provider put on the row: the exact property set a
    // replacement has to reproduce.
    if n < 2 && hr == 0 && !ppv.is_null() && !(*ppv).is_null() {
        provider::dump_store(*ppv);
    }
    hr
}

// IGetRow::GetURLFromHROW(hRow, ppwszURL): if this is what the shell uses, a
// replacement provider only has to hand back paths.
unsafe extern "system" fn hook_geturl_fromhrow(
    this: *mut c_void,
    hrow: usize,
    ppwsz: *mut *mut u16,
) -> i32 {
    let f: GetUrlFn = std::mem::transmute(orig_of(this, 4));
    let hr = f(this, hrow, ppwsz);
    let n = GETURL_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < 5 {
        let url = if hr == 0 && !ppwsz.is_null() {
            read_wide(*ppwsz)
        } else {
            String::new()
        };
        log(&format!(
            "OLEDB: >>> GetURLFromHROW #{n} hRow={hrow:#x} hr=0x{:08X} url={url}",
            hr as u32
        ));
    }
    hr
}

// IRowset::GetNextRows reached through the IRowsetLocate vtable.
unsafe extern "system" fn hook_loc_getnextrows(
    this: *mut c_void,
    hchapter: usize,
    loffset: isize,
    crows: isize,
    pcobtained: *mut usize,
    prghrows: *mut *mut usize,
) -> i32 {
    let f: GetNextFn = std::mem::transmute(orig_of(this, 5));
    let hr = f(this, hchapter, loffset, crows, pcobtained, prghrows);
    let n = LOCFETCH_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < 8 {
        let got = if pcobtained.is_null() { 0 } else { *pcobtained };
        log(&format!(
            "OLEDB: >>> (Locate)GetNextRows #{n} req={crows} got={got} hr=0x{:08X}",
            hr as u32
        ));
    }
    hr
}

// IRowsetLocate::GetRowsAt(hRes1, hChapter, cbBookmark, pBookmark, lOffset, cRows,
// pcRowsObtained, prghRows): random-access fetch, the other way to get handles.
unsafe extern "system" fn hook_getrowsat(
    this: *mut c_void,
    hres1: usize,
    hchapter: usize,
    cbbookmark: usize,
    pbookmark: *const u8,
    loffset: isize,
    crows: isize,
    pcobtained: *mut usize,
    prghrows: *mut *mut usize,
) -> i32 {
    let f: GetRowsAtFn = std::mem::transmute(orig_of(this, 9));
    let hr = f(
        this, hres1, hchapter, cbbookmark, pbookmark, loffset, crows, pcobtained, prghrows,
    );
    let n = LOCFETCH_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < 8 {
        let got = if pcobtained.is_null() { 0 } else { *pcobtained };
        log(&format!(
            "OLEDB: >>> GetRowsAt #{n} off={loffset} req={crows} got={got} hr=0x{:08X}",
            hr as u32
        ));
    }
    hr
}

/// The COM entry point. Its signature is fixed by the ABI: COM is the caller, and the
/// contract that the pointers are valid is COM's to keep, not something this side can express
/// by marking the function `unsafe` (which would change nothing for the caller and only make
/// every `unsafe` block in the body redundant on this edition).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> i32 {
    let clsid = unsafe { rclsid.as_ref() };
    let iid = unsafe { riid.as_ref() };

    let Some(clsid) = clsid else {
        return E_UNEXPECTED;
    };
    // Settings live on whichever key we were activated through: a process may
    // only ever meet one of the search engines, so they cannot be read from one
    // designated class. Diagnostics first, before the first log line.
    if reg_str(clsid, "Debug").is_some() {
        DEBUG.store(true, Ordering::SeqCst);
    }
    let ts = now_ms();
    log(&format!(
        "{ts} DllGetClassObject clsid={} iid={}",
        fmt_guid(clsid),
        iid.map(fmt_guid).unwrap_or_default(),
    ));
    // Pick up the denial experiment's setting once, from our own key.
    if DENY.load(Ordering::SeqCst) == 0 {
        if let Some(s) = reg_str(clsid, "Deny") {
            let mut m = 0usize;
            if s.contains("hotfetch") {
                m |= DENY_HOTFETCH;
            }
            if s.contains("notify") {
                m |= DENY_NOTIFY;
            }
            if s.contains("wsprivate") {
                m |= DENY_WSPRIVATE;
            }
            if m != 0 {
                DENY.store(m, Ordering::SeqCst);
                log(&format!("OLEDB: Deny='{s}' mask={m}"));
            }
        }
        if let Some(n) = reg_str(clsid, "MaxRows").and_then(|s| s.trim().parse::<usize>().ok()) {
            // Clamped to what the daemon is asked for: a cap above the fetch can never be
            // reached and would only mislead whoever set it.
            let n = n.clamp(1, FETCH_ROWS as usize);
            MAX_ROWS_SET.store(n, Ordering::SeqCst);
            log(&format!("OLEDB: MaxRows={n}"));
        }
        if let Some(s) = reg_str(clsid, "Swap") {
            let mode = if s.contains("everyfind") {
                SWAP_EVERYFIND
            } else if s.contains("fixed") {
                SWAP_FIXED
            } else {
                SWAP_OFF
            };
            SWAP.store(mode, Ordering::SeqCst);
            log(&format!("OLEDB: Swap='{s}' mode={mode}"));
        }
    }
    let (h, path) = real_server(clsid);
    if h.is_null() {
        log(&format!("{ts}   ERROR: real server not loaded ({path})"));
        return E_UNEXPECTED;
    }
    let proc = unsafe { GetProcAddress(h, c"DllGetClassObject".as_ptr() as *const u8) };
    let Some(proc) = proc else {
        log(&format!(
            "{ts}   ERROR: DllGetClassObject export missing in {path}"
        ));
        return E_UNEXPECTED;
    };
    let real: DllGetClassObjectFn = unsafe { std::mem::transmute(proc) };
    let hr = unsafe { real(rclsid, riid, ppv) };
    log(&format!(
        "{ts}   forwarded to {path} -> hr=0x{:08X}",
        hr as u32
    ));

    // Whichever engine this is, follow its OLE DB conversation from the class
    // factory down to the rowset. Both engines are data sources of the same
    // shape, so one walk covers them; installing is idempotent, so classes that
    // share a factory vtable are wrapped exactly once.
    if hr == 0
        && SEARCH_ENGINES.iter().any(|g| guid_eq(clsid, g))
        && iid.map(|i| guid_eq(i, &IID_ICLASSFACTORY)).unwrap_or(false)
        && !ppv.is_null()
        && !unsafe { (*ppv).is_null() }
    {
        unsafe { wrap_factory(*ppv) };
    }
    hr
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    // We forwarded objects whose lifetime we do not track, so never claim we can
    // be unloaded.
    S_FALSE
}

const DLL_PROCESS_DETACH: u32 = 0;

#[no_mangle]
pub extern "system" fn DllMain(_inst: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_DETACH {
        // Put every patched vtable slot back before our code unmaps.
        restore_patches();
    }
    1
}
