//! Inspect the *live* search-results folder Explorer is showing.
//!
//! Given a search window (found by its query as LocationName), walk
//! IWebBrowser2 -> IServiceProvider -> IShellBrowser -> active IShellView ->
//! IFolderView -> the folder, and report which interfaces that folder supports.
//! We are looking for a seam to feed items into: `IResultsFolder` (has AddIDList)
//! would be one; a working in-context `EnumObjects` another. What it supports
//! decides whether approach A has any hook point at all.
//!
//!   (drive a search for <query> first, then)
//!   cargo run --example inspect_live -- <query>

use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IEnumIDList, IFolderView, IResultsFolder, IShellBrowser, IShellFolder, IShellFolder2,
    IShellView, IShellWindows, IWebBrowser2, ShellWindows, SHCONTF_FOLDERS, SHCONTF_NONFOLDERS,
    SVGIO_ALLVIEW,
};

fn has<T: Interface>(unk: &windows::core::IUnknown) -> bool {
    unk.cast::<T>().is_ok()
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

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "notepad".into());

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).expect("ShellWindows");

        let count = shell.Count().unwrap_or(0);
        let mut target: Option<IWebBrowser2> = None;
        for i in 0..count {
            let idx = VARIANT::from(i);
            let Ok(disp) = shell.Item(&idx) else { continue };
            let Ok(wb) = disp.cast::<IWebBrowser2>() else {
                continue;
            };
            let name = wb.LocationName().map(|b| b.to_string()).unwrap_or_default();
            if name.eq_ignore_ascii_case(&query) {
                target = Some(wb);
                break;
            }
        }
        let Some(wb) = target else {
            println!("no search window whose location name is {query:?} - drive a search first");
            return;
        };
        println!("found the search window (location = {query:?})");

        // IWebBrowser2 -> IServiceProvider -> IShellBrowser
        let sp: IServiceProvider = match wb.cast() {
            Ok(s) => s,
            Err(e) => {
                println!("no IServiceProvider: {e}");
                return;
            }
        };
        let browser: IShellBrowser = match sp.QueryService(&IShellBrowser::IID) {
            Ok(b) => b,
            Err(e) => {
                println!("QueryService(IShellBrowser) failed: {e}");
                return;
            }
        };
        let view: IShellView = match browser.QueryActiveShellView() {
            Ok(v) => v,
            Err(e) => {
                println!("QueryActiveShellView failed: {e}");
                return;
            }
        };
        // IShellView -> IFolderView -> the folder
        let fv: IFolderView = match view.cast() {
            Ok(f) => f,
            Err(e) => {
                println!("view has no IFolderView: {e}");
                return;
            }
        };
        let folder: IShellFolder = match fv.GetFolder() {
            Ok(f) => f,
            Err(e) => {
                println!("GetFolder failed: {e}");
                return;
            }
        };
        println!("got the live search folder");

        let unk: windows::core::IUnknown = folder.cast().unwrap();
        // The class of the folder the view actually hosts; this is the one whose
        // EnumObjects would matter, if any.
        match folder.cast::<windows::Win32::System::Com::IPersist>() {
            Ok(p) => match p.GetClassID() {
                Ok(c) => println!("HOSTED folder CLSID = {}", fmt_guid(&c)),
                Err(e) => println!("GetClassID failed: {e}"),
            },
            Err(e) => println!("folder has no IPersist: {e}"),
        }
        println!("--- interfaces the live search folder supports ---");
        println!("  IShellFolder2  : {}", has::<IShellFolder2>(&unk));
        println!(
            "  IResultsFolder : {}  (AddIDList/RemoveItem - a feed seam)",
            has::<IResultsFolder>(&unk)
        );
        println!(
            "  IPersistFolder2: {}",
            has::<windows::Win32::UI::Shell::IPersistFolder2>(&unk)
        );
        if let Ok(n) = fv.ItemCount(SVGIO_ALLVIEW) {
            println!("view currently shows {n} items");
        }
        // (Deliberately not calling EnumObjects/Next here: cross-process it blocks
        // on the search folder's STA. The in-explorer agent test already showed
        // the view never calls DBFolder::EnumObjects at all.)
        let _ = (SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, HWND::default());
        let _: Option<IEnumIDList> = None;
        let _: Option<GUID> = None;
    }
}
