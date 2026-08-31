//! Self-test for the shim, run before it is ever registered.
//!
//! Loads the built `searchshim.dll` by path exactly the way COM would, calls its
//! `DllGetClassObject` for `CLSID_SearchFolder` asking for `IClassFactory`, and
//! checks it forwards to the real DLL and returns a live object. If this prints
//! `hr=0x00000000 ptr=non-null` the forward works and it is safe to let Explorer
//! load it; if it does not, we find out here instead of in the user's shell.
//!
//!   cargo run --example probe -- target\release\searchshim.dll

use std::ffi::c_void;

use windows_sys::core::GUID;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;

const CLSID_SEARCH_FOLDER: GUID = GUID {
    data1: 0x0473_1B67,
    data2: 0xD933,
    data3: 0x450A,
    data4: [0x90, 0xE6, 0x4A, 0xCD, 0x2E, 0x94, 0x08, 0xFE],
};
const IID_ICLASS_FACTORY: GUID = GUID {
    data1: 0x0000_0001,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe <path-to-dll>");
    unsafe {
        let h = LoadLibraryW(wide(&path).as_ptr());
        assert!(!h.is_null(), "could not load {path}");
        let proc =
            GetProcAddress(h, c"DllGetClassObject".as_ptr() as *const u8).expect("no export");
        let get: DllGetClassObjectFn = std::mem::transmute(proc);

        let mut ppv: *mut c_void = std::ptr::null_mut();
        let hr = get(&CLSID_SEARCH_FOLDER, &IID_ICLASS_FACTORY, &mut ppv);
        println!(
            "DllGetClassObject(CLSID_SearchFolder, IClassFactory) -> hr=0x{:08X} ptr={}",
            hr as u32,
            if ppv.is_null() { "null" } else { "non-null" }
        );
        if hr == 0 && !ppv.is_null() {
            println!("forward OK - the shim returns the real class factory");
        } else {
            println!("forward FAILED - do not register this");
        }
    }
}
