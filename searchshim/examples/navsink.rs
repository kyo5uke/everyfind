//! Does the in-box search fire `DWebBrowserEvents2::BeforeNavigate2` we can see
//! from OUTSIDE Explorer?
//!
//! If it does, a proper sink could cancel that navigation and send the window to
//! our NSE instead: the option-2 result (real in-place takeover, real items)
//! with none of injection's cost. PowerShell cannot sink these events on a
//! late-bound `__ComObject`, so this hand-rolls the `IDispatch` sink and advises
//! it on the window's connection point, then logs every `Invoke` while a search
//! is driven into the box from a separate script.
//!
//! Flow: open a folder window, find its `IWebBrowser2`, advise, announce the
//! target HWND, then pump messages ~18s logging dispids. The driver script waits
//! for the HWND line, types + Enter, and reads the log.
//!
//!   cargo run --example navsink -- "C:\Users\me\dev"

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, IConnectionPoint, IConnectionPointContainer, CLSCTX_ALL,
};
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

const S_OK: i32 = 0;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x0000_0000_0000_0000_C000_0000_0000_0046);
const IID_IDISPATCH: GUID = GUID::from_u128(0x0002_0400_0000_0000_C000_0000_0000_0046);
// DIID_DWebBrowserEvents2
const DIID_DWBE2: GUID = GUID::from_u128(0x34A7_15A0_6587_11D0_924A_0020_AFC7_AC4D);

const DISPID_BEFORENAVIGATE2: i32 = 250;
const DISPID_NAVIGATECOMPLETE2: i32 = 252;
const DISPID_DOCUMENTCOMPLETE: i32 = 259;

fn log(line: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("navsink.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

// ---- hand-rolled IDispatch / DWebBrowserEvents2 sink ---------------------

#[repr(C)]
struct IDispatchVtbl {
    query_interface: unsafe extern "system" fn(*mut Sink, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Sink) -> u32,
    release: unsafe extern "system" fn(*mut Sink) -> u32,
    get_type_info_count: unsafe extern "system" fn(*mut Sink, *mut u32) -> i32,
    get_type_info: unsafe extern "system" fn(*mut Sink, u32, u32, *mut *mut c_void) -> i32,
    get_ids_of_names: unsafe extern "system" fn(
        *mut Sink,
        *const GUID,
        *const *const u16,
        u32,
        u32,
        *mut i32,
    ) -> i32,
    invoke: unsafe extern "system" fn(
        *mut Sink,
        i32,
        *const GUID,
        u32,
        u16,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut u32,
    ) -> i32,
}

#[repr(C)]
struct Sink {
    vtbl: *const IDispatchVtbl,
    rc: AtomicU32,
}

unsafe extern "system" fn qi(this: *mut Sink, iid: *const GUID, ppv: *mut *mut c_void) -> i32 {
    let iid = &*iid;
    if *iid == IID_IUNKNOWN || *iid == IID_IDISPATCH || *iid == DIID_DWBE2 {
        *ppv = this as *mut c_void;
        (*this).rc.fetch_add(1, Ordering::SeqCst);
        S_OK
    } else {
        *ppv = std::ptr::null_mut();
        E_NOINTERFACE
    }
}
unsafe extern "system" fn add_ref(this: *mut Sink) -> u32 {
    (*this).rc.fetch_add(1, Ordering::SeqCst) + 1
}
unsafe extern "system" fn release(this: *mut Sink) -> u32 {
    let n = (*this).rc.fetch_sub(1, Ordering::SeqCst) - 1;
    if n == 0 {
        drop(Box::from_raw(this));
    }
    n
}
unsafe extern "system" fn get_type_info_count(_: *mut Sink, p: *mut u32) -> i32 {
    if !p.is_null() {
        *p = 0;
    }
    S_OK
}
unsafe extern "system" fn get_type_info(_: *mut Sink, _: u32, _: u32, _: *mut *mut c_void) -> i32 {
    E_NOTIMPL
}
unsafe extern "system" fn get_ids_of_names(
    _: *mut Sink,
    _: *const GUID,
    _: *const *const u16,
    _: u32,
    _: u32,
    _: *mut i32,
) -> i32 {
    E_NOTIMPL
}
unsafe extern "system" fn invoke(
    _this: *mut Sink,
    dispid: i32,
    _riid: *const GUID,
    _lcid: u32,
    _flags: u16,
    _params: *mut c_void,
    _result: *mut c_void,
    _except: *mut c_void,
    _arg_err: *mut u32,
) -> i32 {
    let name = match dispid {
        DISPID_BEFORENAVIGATE2 => "BeforeNavigate2",
        DISPID_NAVIGATECOMPLETE2 => "NavigateComplete2",
        DISPID_DOCUMENTCOMPLETE => "DocumentComplete",
        _ => "(other)",
    };
    log(&format!("INVOKE dispid={dispid} {name}"));
    S_OK
}

static SINK_VTBL: IDispatchVtbl = IDispatchVtbl {
    query_interface: qi,
    add_ref,
    release,
    get_type_info_count,
    get_type_info,
    get_ids_of_names,
    invoke,
};

fn make_sink() -> windows::core::IUnknown {
    let boxed = Box::new(Sink {
        vtbl: &SINK_VTBL,
        rc: AtomicU32::new(1),
    });
    let ptr = Box::into_raw(boxed);
    unsafe { windows::core::IUnknown::from_raw(ptr as *mut c_void) }
}

// --------------------------------------------------------------------------

fn main() {
    let folder = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"C:\Users\me\dev".into());

    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Open a fresh folder window to attach to.
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer.exe")
            .raw_arg(format!("/separate,\"{folder}\""))
            .spawn();
        std::thread::sleep(Duration::from_millis(3500));

        let shell: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
            Ok(s) => s,
            Err(e) => {
                log(&format!("ERROR CoCreateInstance(ShellWindows): {e}"));
                return;
            }
        };
        let leaf = folder.rsplit('\\').next().unwrap_or(&folder).to_lowercase();

        // Pick the matching window with the largest HWND (proxy for newest).
        let count = shell.Count().unwrap_or(0);
        let mut best: Option<(isize, IWebBrowser2)> = None;
        for i in 0..count {
            let idx = VARIANT::from(i);
            let Ok(disp) = shell.Item(&idx) else { continue };
            let Ok(wb) = disp.cast::<IWebBrowser2>() else {
                continue;
            };
            let url = wb.LocationURL().map(|b| b.to_string()).unwrap_or_default();
            if !url.to_lowercase().ends_with(&leaf) {
                continue;
            }
            let hwnd = wb.HWND().map(|h| h.0).unwrap_or(0);
            if best.as_ref().map(|(h, _)| hwnd > *h).unwrap_or(true) {
                best = Some((hwnd, wb));
            }
        }
        let Some((hwnd, wb)) = best else {
            log("ERROR: no matching window found");
            return;
        };
        log(&format!(
            "TARGET hwnd={hwnd} url={}",
            wb.LocationURL().map(|b| b.to_string()).unwrap_or_default()
        ));

        // Advise the sink on DWebBrowserEvents2.
        let cpc: IConnectionPointContainer = match wb.cast() {
            Ok(c) => c,
            Err(e) => {
                log(&format!("ERROR cast IConnectionPointContainer: {e}"));
                return;
            }
        };
        let cp: IConnectionPoint = match cpc.FindConnectionPoint(&DIID_DWBE2) {
            Ok(c) => c,
            Err(e) => {
                log(&format!(
                    "ERROR FindConnectionPoint(DWebBrowserEvents2): {e}"
                ));
                return;
            }
        };
        let sink = make_sink();
        let cookie = match cp.Advise(&sink) {
            Ok(c) => c,
            Err(e) => {
                log(&format!("ERROR Advise: {e}"));
                return;
            }
        };
        log(&format!("ADVISED hwnd={hwnd} cookie={cookie}"));

        // Pump messages so STA events are delivered, for ~18s.
        let deadline = Instant::now() + Duration::from_secs(18);
        let mut msg = MSG::default();
        while Instant::now() < deadline {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = cp.Unadvise(cookie);
        log("DONE");
    }
}
