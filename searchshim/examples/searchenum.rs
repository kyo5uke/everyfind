//! Does a real search-results folder enumerate synchronously?
//!
//! Binds `search-ms:query=<q>&crumb=location:<loc>` and calls `EnumObjects`,
//! counting what comes back and how long it took. If the matches arrive from a
//! plain `EnumObjects` call, then a folder wrapper that overrides `EnumObjects`
//! could substitute Everyfind's items while keeping the native search identity.
//! If it returns nothing (results stream in asynchronously to the view instead),
//! the seam is elsewhere and the wrapper idea will not work.
//!
//!   cargo run --example searchenum -- "notepad" "C:\Windows"

use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::Common::{ITEMIDLIST, STRRET};
use windows::Win32::UI::Shell::{
    IEnumIDList, IShellFolder, SHGetDesktopFolder, SHParseDisplayName, StrRetToStrW,
    SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, SHGDN_NORMAL,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    let q = std::env::args().nth(1).unwrap_or_else(|| "notepad".into());
    let loc = std::env::args()
        .nth(2)
        .unwrap_or_else(|| r"C:\Windows".into());
    let url = format!("search-ms:query={q}&crumb=location:{loc}");
    println!("binding: {url}");

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        if let Err(e) = SHParseDisplayName(PCWSTR(wide(&url).as_ptr()), None, &mut pidl, 0, None) {
            println!("SHParseDisplayName failed: {e}");
            return;
        }

        let desktop: IShellFolder = SHGetDesktopFolder().expect("desktop");
        let folder: IShellFolder = match desktop.BindToObject(pidl, None) {
            Ok(f) => f,
            Err(e) => {
                println!("BindToObject failed: {e}");
                return;
            }
        };
        println!("bound the search folder; calling EnumObjects...");

        let began = Instant::now();
        let mut en: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32;
        let hr = folder.EnumObjects(HWND::default(), flags, &mut en);
        println!(
            "EnumObjects returned hr=0x{:08X} after {} ms",
            hr.0 as u32,
            began.elapsed().as_millis()
        );
        let Some(en) = en else {
            println!("no enumerator -> results are NOT delivered via EnumObjects");
            return;
        };

        let mut count = 0usize;
        let mut sample = Vec::new();
        loop {
            let mut fetched = [std::ptr::null_mut::<ITEMIDLIST>(); 1];
            let mut got = 0u32;
            let hr = en.Next(&mut fetched, Some(&mut got));
            if hr.is_err() || got == 0 {
                break;
            }
            count += 1;
            if sample.len() < 8 {
                let child = fetched[0];
                let mut sr = STRRET::default();
                if folder
                    .GetDisplayNameOf(child, SHGDN_NORMAL, &mut sr)
                    .is_ok()
                {
                    let mut pw = windows::core::PWSTR::null();
                    if StrRetToStrW(&mut sr, Some(child), &mut pw).is_ok() {
                        sample.push(pw.to_string().unwrap_or_default());
                    }
                }
            }
            if count > 100000 {
                break;
            }
        }
        println!(
            "enumerated {count} items in {} ms total",
            began.elapsed().as_millis()
        );
        for s in &sample {
            println!("  {s}");
        }
        println!(
            "verdict: {}",
            if count > 0 {
                "EnumObjects yields the results -> a folder wrapper CAN substitute items"
            } else {
                "EnumObjects yields nothing -> results are async; wrapper won't work"
            }
        );
    }
}
