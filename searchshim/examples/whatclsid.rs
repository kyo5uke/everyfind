//! Ask the search-results folder what its own CLSID is.
//!
//! `search-ms:` is what Explorer's search box navigates to on Enter. Parsing it
//! to a PIDL and binding that PIDL yields the exact `IShellFolder` object the
//! view drives, and `IPersist::GetClassID` on it is that folder's real class.
//! That is the CLSID an override would have to stand in for, found by asking the
//! shell rather than by guessing which of Windows.Storage.Search.dll's dozens of
//! "search" CLSIDs is the one.
//!
//!   cargo run --example whatclsid -- "C:\Users\me\dev"

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::System::Com::{CoInitializeEx, IPersist, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{ILFree, IShellFolder, SHGetDesktopFolder, SHParseDisplayName};

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
    let loc = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"C:\Users\me\dev".into());
    let url = format!("search-ms:query=zzqmarker&crumb=location:{loc}");
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    println!("binding: {url}");

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let mut abs: *mut ITEMIDLIST = std::ptr::null_mut();
        if let Err(e) = SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut abs, 0, None) {
            println!("SHParseDisplayName failed: {e}");
            return;
        }
        if abs.is_null() {
            println!("SHParseDisplayName returned null pidl");
            return;
        }

        let desktop: IShellFolder = match SHGetDesktopFolder() {
            Ok(d) => d,
            Err(e) => {
                println!("SHGetDesktopFolder failed: {e}");
                ILFree(Some(abs));
                return;
            }
        };

        let folder: IShellFolder = match desktop.BindToObject(abs, None) {
            Ok(f) => f,
            Err(e) => {
                println!("BindToObject failed: {e}");
                ILFree(Some(abs));
                return;
            }
        };

        match folder.cast::<IPersist>() {
            Ok(persist) => match persist.GetClassID() {
                Ok(clsid) => println!("search-ms folder CLSID = {}", fmt_guid(&clsid)),
                Err(e) => println!("GetClassID failed: {e}"),
            },
            Err(e) => println!("folder has no IPersist: {e}"),
        }

        ILFree(Some(abs));
    }
}
