//! Launch the first item of the live search view the way a double-click does, and report the
//! working directory the program was given.
//!
//! Explorer opens an item through the folder's `IContextMenu` with the folder's own path in
//! `lpDirectory`. A search-results folder has no path, so what a program actually starts in is
//! a question about the item, not about the program, and only measuring answers it.
//!
//!   cargo run --example invoke -- <query>

use windows::core::{Interface, PCSTR, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IFolderView, IShellBrowser, IShellFolder, IShellView, IShellWindows,
    IWebBrowser2, ShellWindows, CMINVOKECOMMANDINFO, SVGIO_ALLVIEW,
};

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "cwdprobe".into());
    let out = std::env::temp_dir().join("cwdprobe.txt");
    let _ = std::fs::remove_file(&out);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).expect("ShellWindows");
        let mut target: Option<IWebBrowser2> = None;
        for i in 0..shell.Count().unwrap_or(0) {
            if let Ok(d) = shell.Item(&VARIANT::from(i)) {
                if let Ok(w) = d.cast::<IWebBrowser2>() {
                    let loc = w.LocationName().map(|b| b.to_string()).unwrap_or_default();
                    if loc.to_lowercase().contains(&query.to_lowercase()) {
                        target = Some(w);
                        break;
                    }
                }
            }
        }
        let Some(wb) = target else {
            println!("no window matching {query:?}");
            return;
        };
        let sp: IServiceProvider = wb.cast().expect("sp");
        let browser: IShellBrowser = sp.QueryService(&IShellBrowser::IID).expect("browser");
        let view: IShellView = browser.QueryActiveShellView().expect("view");
        let fv: IFolderView = view.cast().expect("IFolderView");
        let folder: IShellFolder = fv.GetFolder().expect("folder");
        if fv.ItemCount(SVGIO_ALLVIEW).unwrap_or(0) == 0 {
            println!("the view is empty");
            return;
        }
        let pidl: *mut ITEMIDLIST = fv.Item(0).expect("item 0");
        let items = [pidl as *const ITEMIDLIST];
        let menu: IContextMenu = folder
            .GetUIObjectOf(None, &items, None)
            .expect("IContextMenu for the item");

        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            lpVerb: PCSTR(c"open".as_ptr() as *const u8),
            nShow: 1,
            ..Default::default()
        };
        match menu.InvokeCommand(&info) {
            Ok(()) => println!("invoked the default verb"),
            Err(e) => println!("InvokeCommand failed: {e}"),
        }
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if let Ok(s) = std::fs::read_to_string(&out) {
                println!("{s}");
                return;
            }
        }
        println!("the probe wrote nothing (it may not have run)");
    }
}
