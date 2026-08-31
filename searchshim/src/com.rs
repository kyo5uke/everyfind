//! Reaching into somebody else's vtable, and getting back out.
//!
//! Everything in this module runs inside explorer.exe on a thread the shell owns, over
//! pointers the shell handed us, and a mistake here is not a wrong search result; it is the
//! desktop going away. So the dangerous parts live together, small enough to read in one
//! sitting, and every one of them is written to fail by declining rather than by faulting:
//!
//! - [`patch`] returns `None` rather than storing into a page it could not make writable.
//! - [`install`] records only what it actually redirected, and never twice, so a hook can
//!   never call itself through a shared vtable.
//! - [`looks_like_com`] refuses a pointer whose first three vtable entries are not code in a
//!   loaded module, which is what stands between a stray argument and a dereference.
//! - [`read_wide_in_region`] will not walk out of the committed region a string starts in.
//! - [`restore_patches`] puts every slot back, so unloading leaves no trace in the process.
//!
//! What is *not* here is which methods we listen to. That is the OLE DB conversation, and it
//! reads better next to the interfaces it is about.

use std::ffi::c_void;

use windows_sys::core::GUID;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Memory::{
    VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS,
    PAGE_READWRITE,
};

use windows_sys::Win32::Foundation::HMODULE;

use crate::{QiFn, ReleaseFn};

pub(crate) unsafe fn vslot(obj: *mut c_void, i: usize) -> *mut usize {
    (*(obj as *const *mut usize)).add(i)
}

/// Write `val` into vtable slot `sl`, returning what was there.
///
/// `None` when the slot could not be made writable. A vtable normally sits in a read-only
/// section, so the write only works because of the `VirtualProtect` in front of it, and if
/// that call fails (a hardened process, a page we have no rights to), storing anyway is an
/// access violation inside explorer.exe. Not patching costs a feature; faulting costs the
/// shell, so the failure has to be a return value and not an assumption.
pub(crate) unsafe fn patch(sl: *mut usize, val: usize) -> Option<usize> {
    let mut old = 0u32;
    if VirtualProtect(sl as *const c_void, 8, PAGE_READWRITE, &mut old) == 0 {
        return None;
    }
    let prev = *sl;
    *sl = val;
    // Best effort: the protection is already relaxed either way, and failing to put it back
    // is not a reason to leave the slot pointing at a hook we have not recorded.
    let mut back = 0u32;
    VirtualProtect(sl as *const c_void, 8, old, &mut back);
    Some(prev)
}

/// Every vtable slot we have redirected, as (slot address, original function).
///
/// Keyed by address rather than by a fixed list of roles, because the same hook
/// is installed on more than one object: Explorer answers a search with one of
/// two engines, and a process can meet both. A single "the original Execute"
/// global would make the second engine's hook call the first engine's function.
pub(crate) static PATCHES: std::sync::Mutex<Vec<(usize, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// Redirect slot `idx` of `obj`'s vtable to `hook`, remembering what was there.
/// Idempotent: a slot already redirected (a shared vtable, a second instance of
/// the same class) is left alone, so a hook can never end up calling itself.
pub(crate) unsafe fn install(obj: *mut c_void, idx: usize, hook: usize) -> bool {
    let sl = vslot(obj, idx);
    let mut p = held(&PATCHES);
    if p.iter().any(|(a, _)| *a == sl as usize) {
        return false;
    }
    // Record only what was actually redirected. A slot we failed to patch still holds the
    // genuine function, so remembering it would be a lie `orig_of` cannot detect.
    let Some(orig) = patch(sl, hook) else {
        return false;
    };
    p.push((sl as usize, orig));
    true
}

/// The function that slot `idx` of `this`'s vtable held before we redirected it.
///
/// Every hook transmutes this and calls it, so a zero here is a null call inside explorer.exe.
/// It cannot be reached: a hook only runs because [`install`] recorded the slot first, and the
/// table is never emptied except by [`restore_patches`], which puts the originals back in the
/// same breath. Poisoning is the one way the lookup could have failed anyway, and [`held`]
/// takes that off the table.
pub(crate) unsafe fn orig_of(this: *mut c_void, idx: usize) -> usize {
    let sl = vslot(this, idx) as usize;
    held(&PATCHES)
        .iter()
        .find(|(a, _)| *a == sl)
        .map(|(_, o)| *o)
        .unwrap_or(0)
}

/// Take a lock, poisoned or not.
///
/// A poisoned mutex means some other thread panicked while holding it, which says nothing
/// about the contents here: the slot table is a plain `Vec` of integers and cannot be left
/// half-written. Refusing to read it would turn one thread's panic into a null call in every
/// hook afterwards, which is a far worse outcome than reading a table that is perfectly fine.
pub(crate) fn held<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// QueryInterface on a raw object; returns the interface pointer (AddRef'd) or null.
pub(crate) unsafe fn qi(obj: *mut c_void, iid: &GUID) -> *mut c_void {
    let f: QiFn = std::mem::transmute(*vslot(obj, 0));
    let mut out: *mut c_void = std::ptr::null_mut();
    if f(obj, iid, &mut out) == 0 {
        out
    } else {
        std::ptr::null_mut()
    }
}
pub(crate) unsafe fn release(obj: *mut c_void) {
    let f: ReleaseFn = std::mem::transmute(*vslot(obj, 2));
    f(obj);
}

/// True if `[p, p+len)` is committed, readable memory, checked before touching a
/// pointer we did not create.
pub(crate) unsafe fn mem_readable(p: usize, len: usize) -> bool {
    if p < 0x1_0000 {
        return false;
    }
    let mut mbi: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    let sz = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if VirtualQuery(p as *const c_void, &mut mbi, sz) == 0 || mbi.State != MEM_COMMIT {
        return false;
    }
    if mbi.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
        return false;
    }
    let end = mbi.BaseAddress as usize + mbi.RegionSize;
    p.checked_add(len).map(|e| e <= end).unwrap_or(false)
}

/// A conservative "is this a COM interface pointer?" test: it and its vtable are
/// readable and the first vtable entry lands in a loaded module. That makes a
/// following `QueryInterface` on it safe (a real object answers E_NOINTERFACE
/// rather than faulting).
pub(crate) unsafe fn looks_like_com(p: usize) -> bool {
    if !mem_readable(p, 8) {
        return false;
    }
    let vtbl = *(p as *const usize);
    if !mem_readable(vtbl, 8 * 3) {
        return false;
    }
    // QueryInterface/AddRef/Release must all be code in a loaded module; this
    // rules out plain data pointers that merely start with a module-ish word.
    for i in 0..3 {
        let fptr = *((vtbl as *const usize).add(i));
        let mut h: HMODULE = std::ptr::null_mut();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            fptr as *const u16,
            &mut h,
        ) == 0
        {
            return false;
        }
    }
    true
}
pub(crate) unsafe fn read_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    while *p.add(n) != 0 && n < 8192 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// Read a NUL-terminated wide string without leaving the committed region it starts in.
///
/// The scan below walks memory the caller owns looking for something shaped like a path, so
/// most of what it dereferences is not a string at all. An unterminated run near the end of a
/// region would otherwise walk into the next page, and an access violation here is an
/// explorer.exe crash.
pub(crate) unsafe fn read_wide_in_region(p: usize) -> Option<String> {
    let mut mbi: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    let sz = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if VirtualQuery(p as *const c_void, &mut mbi, sz) == 0
        || mbi.State != MEM_COMMIT
        || mbi.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0
    {
        return None;
    }
    let end = (mbi.BaseAddress as usize).checked_add(mbi.RegionSize)?;
    let max = (end.checked_sub(p)? / 2).min(1024);
    let mut n = 0usize;
    while n < max && *((p as *const u16).add(n)) != 0 {
        n += 1;
    }
    // No terminator inside the region: not a string we are allowed to read.
    if n == 0 || n == max {
        return None;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        p as *const u16,
        n,
    )))
}

/// Put every vtable we touched back the way we found it, so unloading the shim
/// leaves no trace of it in the process.
pub(crate) fn restore_patches() {
    {
        let mut p = held(&PATCHES);
        for (sl, orig) in p.drain(..) {
            unsafe { patch(sl as *mut usize, orig) };
        }
    }
}

/// Run `f`, and turn a panic inside it into `fallback` rather than a dead Explorer.
///
/// Every hook in this shim is `extern "system"`, an ABI Rust is not allowed to unwind across.
/// A panic in one does not come back as an error code the shell can cope with; it aborts the
/// process, and the process is explorer.exe. The whole desktop, for a bad index in a string
/// parser.
///
/// So the boundary is drawn here instead: the parts that read shell-supplied text run inside
/// this, and anything that goes wrong in them becomes the same "we have no answer" the code
/// already knows how to handle: the search falls through to Windows, and the reason is in the
/// log. That is a worse search, once. The alternative is the user losing their session.
pub(crate) fn guard<T>(what: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    // The panic message itself would otherwise go to a stderr no Explorer process has.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(e) => {
            let why = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panicked".to_string());
            crate::log(&format!("PANIC in {what}: {why} - falling back"));
            fallback
        }
    }
}
