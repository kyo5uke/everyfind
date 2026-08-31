//! Does the search view fetch more rows when you scroll?
//!
//! The shim hands the view fifty rows and it asks for exactly one page of thirty-two, then
//! stops. Whether that is a *cap* or *laziness* decides the whole embedding design: if the
//! view pages on demand, the per-row cost Explorer charges is only for what is on screen, and
//! trimming the answer buys nothing.
//!
//!   (drive a search first, then)  cargo run --example scroll -- <query>

use windows::core::{Interface, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellView, IShellWindows, IWebBrowser2, ShellWindows,
    SVGIO_ALLVIEW, SVSI_ENSUREVISIBLE, SVSI_FOCUSED,
};

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "a".into());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).expect("ShellWindows");
        let mut target = None;
        for i in 0..shell.Count().unwrap_or(0) {
            if let Ok(disp) = shell.Item(&VARIANT::from(i)) {
                if let Ok(wb) = disp.cast::<IWebBrowser2>() {
                    let loc = wb.LocationName().map(|b| b.to_string()).unwrap_or_default();
                    if loc.to_lowercase().contains(&query.to_lowercase()) {
                        target = Some(wb);
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

        let before = fv.ItemCount(SVGIO_ALLVIEW).unwrap_or(0);
        println!("before: the view holds {before} items");

        // Ask the view to bring a far-down item into sight. If it pages lazily, this is what
        // makes it come back for more rows.
        for idx in [31, 40, 49] {
            match fv.SelectItem(idx, SVSI_ENSUREVISIBLE.0 as u32 | SVSI_FOCUSED.0 as u32) {
                Ok(()) => println!("  scrolled to item {idx}"),
                Err(e) => println!("  item {idx}: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(1200));
        }
        let after = fv.ItemCount(SVGIO_ALLVIEW).unwrap_or(0);
        println!("after:  the view holds {after} items");
    }
}
