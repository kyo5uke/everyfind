//! How long does the view take to fill, and with how many rows?
//!
//! The row cap exists because Explorer builds a shell item, an icon and a property set for
//! every row we hand it, but the figure that cap was chosen from is second-hand. This drives
//! a search and polls the view until its item count stops changing, so the cost of a page can
//! be read in wall-clock time instead of assumed.
//!
//!   cargo run --example settle -- <query> <folder>

use std::time::{Duration, Instant};
use windows::core::{Interface, BSTR, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellView, IShellWindows, IWebBrowser2, ShellWindows,
    SVGIO_ALLVIEW,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let query = args.next().unwrap_or_else(|| "a".into());
    let folder = args.next().unwrap_or_else(|| r"C:\".into());
    let url = format!(
        "search-ms:query={query}&crumb=location:{}",
        folder
            .replace(char::from(58), "%3A")
            .replace(char::from(92), "%5C")
    );

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).expect("ShellWindows");
        let mut found: Option<IWebBrowser2> = None;
        for i in 0..shell.Count().unwrap_or(0) {
            if let Ok(d) = shell.Item(&VARIANT::from(i)) {
                if let Ok(w) = d.cast::<IWebBrowser2>() {
                    found = Some(w);
                    break;
                }
            }
        }
        let Some(wb) = found else {
            println!("no Explorer window is open");
            return;
        };

        let t0 = Instant::now();
        wb.Navigate2(
            &VARIANT::from(BSTR::from(url.as_str())),
            None,
            None,
            None,
            None,
        )
        .expect("navigate");

        // Poll until the count holds still for a second.
        let (mut last, mut stable_since) = (-1i32, Instant::now());
        let mut first_row_at: Option<Duration> = None;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let n = (|| {
                let sp: IServiceProvider = wb.cast().ok()?;
                let b: IShellBrowser = sp.QueryService(&IShellBrowser::IID).ok()?;
                let v: IShellView = b.QueryActiveShellView().ok()?;
                let fv: IFolderView = v.cast().ok()?;
                fv.ItemCount(SVGIO_ALLVIEW).ok()
            })()
            .unwrap_or(-1);
            if n != last {
                if n > 0 && first_row_at.is_none() {
                    first_row_at = Some(t0.elapsed());
                }
                last = n;
                stable_since = Instant::now();
            }
            if last >= 0 && stable_since.elapsed() > Duration::from_millis(1200) {
                break;
            }
            if t0.elapsed() > Duration::from_secs(45) {
                println!("gave up after 45 s (count {last})");
                return;
            }
        }
        println!(
            "{query:>8} in {folder:<16} {last:>4} items | first row {:>6.0} ms | settled {:>6.0} ms",
            first_row_at.unwrap_or_default().as_millis(),
            (t0.elapsed() - Duration::from_millis(1200)).as_millis(),
        );
    }
}
