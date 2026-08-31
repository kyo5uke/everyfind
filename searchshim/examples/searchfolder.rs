//! Can a native search-results folder be made to show an *explicit* item list?
//!
//! `ISearchFolderItemFactory` builds the same folder Explorer navigates to for a
//! search: native "Search Results in X" chrome and breadcrumb. Normally its
//! contents come from running a condition over a scope (Windows Search). The
//! question for approach A: if the scope is set to a specific set of *files*
//! (Everyfind's results) with no condition, does the bound folder enumerate
//! exactly those files? If yes, we get the native search look with our items and
//! never touch the registry or inject into Explorer.
//!
//!   cargo run --example searchfolder

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::{ITEMIDLIST, STRRET};
use windows::Win32::UI::Shell::{
    IEnumIDList, ISearchFolderItemFactory, IShellFolder, SHCreateShellItemArrayFromIDLists,
    SHGetDesktopFolder, SHParseDisplayName, StrRetToStrW, SHCONTF_FOLDERS, SHCONTF_NONFOLDERS,
    SHGDN_NORMAL,
};

// CLSID_SearchFolderItemFactory
const CLSID_SEARCH_FOLDER_ITEM_FACTORY: GUID =
    GUID::from_u128(0x14010e02_bbbd_41f0_88e3_eda371216584);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn parse(path: &str) -> Option<*mut ITEMIDLIST> {
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    let w = wide(path);
    unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None) }.ok()?;
    if pidl.is_null() {
        None
    } else {
        Some(pidl)
    }
}

fn main() {
    // Stand-ins for "Everyfind's results": a few real, distinctive files.
    let results = [
        r"C:\Windows\notepad.exe",
        r"C:\Windows\explorer.exe",
        r"C:\Windows\win.ini",
    ];

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let pidls: Vec<*const ITEMIDLIST> = results
            .iter()
            .filter_map(|p| parse(p))
            .map(|p| p as *const ITEMIDLIST)
            .collect();
        println!("built {} result pidls", pidls.len());

        let array = match SHCreateShellItemArrayFromIDLists(&pidls) {
            Ok(a) => a,
            Err(e) => {
                println!("SHCreateShellItemArrayFromIDLists failed: {e}");
                return;
            }
        };

        let factory: ISearchFolderItemFactory =
            match CoCreateInstance(&CLSID_SEARCH_FOLDER_ITEM_FACTORY, None, CLSCTX_ALL) {
                Ok(f) => f,
                Err(e) => {
                    println!("CoCreateInstance(SearchFolderItemFactory) failed: {e}");
                    return;
                }
            };

        if let Err(e) = factory.SetDisplayName(PCWSTR(wide("everyfind-test").as_ptr())) {
            println!("SetDisplayName failed: {e}");
        }
        // The key move: scope = the explicit files, no condition.
        if let Err(e) = factory.SetScope(&array) {
            println!("SetScope(files) failed: {e}");
        }

        let pidl = match factory.GetIDList() {
            Ok(p) => p,
            Err(e) => {
                println!("GetIDList failed: {e}");
                return;
            }
        };
        if pidl.is_null() {
            println!("GetIDList returned null");
            return;
        }
        println!("search folder pidl built");

        // Bind it and enumerate: what does the native search folder actually show?
        let desktop: IShellFolder = match SHGetDesktopFolder() {
            Ok(d) => d,
            Err(e) => {
                println!("SHGetDesktopFolder failed: {e}");
                return;
            }
        };
        let folder: IShellFolder = match desktop.BindToObject(pidl, None) {
            Ok(f) => f,
            Err(e) => {
                println!("BindToObject(search pidl) failed: {e}");
                return;
            }
        };

        let mut en: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32;
        let hr = folder.EnumObjects(HWND::default(), flags, &mut en);
        if hr.is_err() {
            println!("EnumObjects hr=0x{:08X}", hr.0 as u32);
        }
        let Some(en) = en else {
            println!("EnumObjects returned no enumerator");
            return;
        };

        let mut shown = Vec::new();
        loop {
            let mut fetched = [std::ptr::null_mut::<ITEMIDLIST>(); 1];
            let mut got = 0u32;
            let hr = en.Next(&mut fetched, Some(&mut got));
            if hr.is_err() || got == 0 {
                break;
            }
            let child = fetched[0];
            let mut sr = STRRET::default();
            if folder
                .GetDisplayNameOf(child, SHGDN_NORMAL, &mut sr)
                .is_ok()
            {
                let mut pw = PWSTR::null();
                if StrRetToStrW(&mut sr, Some(child), &mut pw).is_ok() {
                    shown.push(pw.to_string().unwrap_or_default());
                }
            }
            if shown.len() > 50 {
                break;
            }
        }

        println!(
            "--- items the native search folder shows ({}) ---",
            shown.len()
        );
        for s in &shown {
            println!("  {s}");
        }
        println!(
            "verdict: {}",
            if shown.len() == pidls.len() && !pidls.is_empty() {
                "scope=files -> shows exactly our items (A possible WITHOUT injection)"
            } else if shown.is_empty() {
                "empty -> scope=files does not list files this way"
            } else {
                "different count -> not a clean explicit list"
            }
        );
    }
}
