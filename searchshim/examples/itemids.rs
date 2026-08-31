//! Are the items in the live search view distinguishable from each other?
//!
//! The reported symptom is that clicking one result paints *every* row selected while the
//! shell's real selection stays at one item. That is what a view does when its data items all
//! carry the same identity: it marks "the selected one" and every row matches.
//!
//! Identity in a shell view is the item's PIDL, compared through the folder's `CompareIDs`.
//! So this asks the live folder directly: no Explorer restart, no guessing at which property
//! carries identity. It prints each item's PIDL bytes and then compares every pair.
//!
//!   (drive a search in Explorer first, then)
//!   cargo run --example itemids -- <query>

use windows::core::{Interface, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellFolder, IShellView, IShellWindows, IWebBrowser2,
    ShellWindows, SHGDN_FORPARSING, SHGDN_INFOLDER, SVGIO_ALLVIEW,
};

/// The raw bytes of a PIDL: each item is `cb: u16` followed by `cb - 2` bytes, terminated by a
/// zero `cb`. Reading them is the only way to see whether two items really differ.
unsafe fn pidl_bytes(p: *const ITEMIDLIST) -> Vec<u8> {
    let mut out = Vec::new();
    let mut q = p as *const u8;
    loop {
        let cb = u16::from_le_bytes([*q, *q.add(1)]) as usize;
        if cb == 0 {
            break;
        }
        out.extend_from_slice(std::slice::from_raw_parts(q, cb));
        q = q.add(cb);
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The serialized property sets inside a PIDL, as `{fmtid}#pid` names.
///
/// A DBFolder item PIDL is a bag of `1SPS` sections: `cb | "1SPS" | FMTID`, then values of
/// `cbValue | pid | reserved | PROPVARIANT` until a zero `cbValue`. Listing what is *in* the
/// bag (rather than what can be read back out of it) is how a missing section shows up.
fn pidl_props(bytes: &[u8]) -> Vec<String> {
    const MAGIC: &[u8] = b"1SPS";
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 24 <= bytes.len() {
        if &bytes[i..i + 4] != MAGIC {
            i += 1;
            continue;
        }
        let g = &bytes[i + 4..i + 20];
        let mut data4 = [0u8; 8];
        data4.copy_from_slice(&g[8..16]);
        let guid = windows::core::GUID {
            data1: u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
            data2: u16::from_le_bytes([g[4], g[5]]),
            data3: u16::from_le_bytes([g[6], g[7]]),
            data4,
        };
        // Values follow the FMTID.
        let mut v = i + 20;
        while v + 8 <= bytes.len() {
            let cb =
                u32::from_le_bytes([bytes[v], bytes[v + 1], bytes[v + 2], bytes[v + 3]]) as usize;
            if cb == 0 || cb > 0x1_0000 || v + cb > bytes.len() {
                break;
            }
            let pid = u32::from_le_bytes([bytes[v + 4], bytes[v + 5], bytes[v + 6], bytes[v + 7]]);
            let key = windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY { fmtid: guid, pid };
            let name = unsafe {
                windows::Win32::UI::Shell::PropertiesSystem::PSGetNameFromPropertyKey(&key)
            }
            .map(|p| unsafe {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            })
            .unwrap_or_else(|_| format!("{guid:?}#{pid}"));
            out.push(name);
            v += cb;
        }
        i = v.max(i + 4);
    }
    out
}

unsafe fn name(
    folder: &IShellFolder,
    pidl: *const ITEMIDLIST,
    flags: windows::Win32::UI::Shell::SHGDNF,
) -> String {
    let mut s = windows::Win32::UI::Shell::Common::STRRET::default();
    if folder.GetDisplayNameOf(pidl, flags, &mut s).is_err() {
        return "<no name>".into();
    }
    let mut buf = [0u16; 1024];
    if windows::Win32::UI::Shell::StrRetToBufW(&mut s, Some(pidl), &mut buf).is_err() {
        return "<unprintable>".into();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "notepad".into());

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).expect("ShellWindows");

        let mut target: Option<IWebBrowser2> = None;
        for i in 0..shell.Count().unwrap_or(0) {
            let Ok(disp) = shell.Item(&VARIANT::from(i)) else {
                continue;
            };
            let Ok(wb) = disp.cast::<IWebBrowser2>() else {
                continue;
            };
            let loc = wb.LocationName().map(|b| b.to_string()).unwrap_or_default();
            println!("  window: {loc:?}");
            if loc.to_lowercase().contains(&query.to_lowercase()) {
                target = Some(wb);
                break;
            }
        }
        // Fall back to whatever window is open: a search driven programmatically does not
        // always rename its location, and the items are what matter here, not the caption.
        if target.is_none() {
            for i in 0..shell.Count().unwrap_or(0) {
                if let Ok(disp) = shell.Item(&VARIANT::from(i)) {
                    if let Ok(wb) = disp.cast::<IWebBrowser2>() {
                        println!("  (no name match; using the first window)");
                        target = Some(wb);
                        break;
                    }
                }
            }
        }
        let Some(wb) = target else {
            println!("\nno Explorer window is open at all");
            return;
        };

        let sp: IServiceProvider = wb.cast().expect("IServiceProvider");
        let browser: IShellBrowser = sp.QueryService(&IShellBrowser::IID).expect("IShellBrowser");
        let view: IShellView = browser.QueryActiveShellView().expect("active view");
        let fv: IFolderView = view.cast().expect("IFolderView");
        let folder: IShellFolder = fv.GetFolder().expect("folder");

        let n = fv.ItemCount(SVGIO_ALLVIEW).unwrap_or(0);
        println!("\nthe view shows {n} items\n");

        let take = n;
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for i in 0..take {
            match fv.Item(i) {
                Ok(p) => pidls.push(p),
                Err(e) => println!("Item({i}) failed: {e}"),
            }
        }

        for (i, &p) in pidls.iter().enumerate() {
            let bytes = pidl_bytes(p);
            println!("item {i}");
            println!("  in-folder name : {}", name(&folder, p, SHGDN_INFOLDER));
            println!("  parsing name   : {}", name(&folder, p, SHGDN_FORPARSING));
            let _ = &bytes;
            if i == 0 {
                let props = pidl_props(&bytes);
                println!("  pidl carries {} serialized properties:", props.len());
                for p in &props {
                    println!("      {p}");
                }
            }
            let _ = hex(&bytes);
        }

        // The question the view itself asks. `CompareIDs` returns a signed short in the low
        // word; zero means "these are the same item". The lParam picks *how* to compare, and
        // it matters: column 0 is a sort, while SHCIDS_CANONICALONLY asks the folder for
        // identity proper, which is what a selection is keyed on.
        const SHCIDS_ALLFIELDS: isize = 0x8000_0000u32 as i32 as isize;
        const SHCIDS_CANONICALONLY: isize = 0x1000_0000;
        for (label, lparam) in [
            ("column 0", 0isize),
            ("SHCIDS_ALLFIELDS", SHCIDS_ALLFIELDS),
            ("SHCIDS_CANONICALONLY", SHCIDS_CANONICALONLY),
        ] {
            println!("\n--- CompareIDs, {label} (0 = the folder says SAME item) ---");
            let mut same = 0;
            let mut pairs = 0;
            for i in 0..pidls.len() {
                for j in (i + 1)..pidls.len() {
                    let hr = folder.CompareIDs(
                        windows::Win32::Foundation::LPARAM(lparam),
                        pidls[i],
                        pidls[j],
                    );
                    if hr.is_err() {
                        println!("  {i} vs {j}: failed 0x{:08X}", hr.0 as u32);
                        continue;
                    }
                    pairs += 1;
                    let code = (hr.0 & 0xffff) as i16;
                    if code == 0 {
                        same += 1;
                    }
                }
            }
            println!("  {same} of {pairs} pairs compare EQUAL");
        }
        // Which properties does the folder actually find on these items? The canonical
        // comparison reads one of them; a property that is missing reads the same (absent)
        // for every row, which is exactly how 12 distinct files end up as one identity.
        if let Ok(f2) = folder.cast::<windows::Win32::UI::Shell::IShellFolder2>() {
            println!("\n--- what the folder can read off each item ---");
            // The delegate PIDL is a blob, so report its shape rather than a printable value:
            // a mismatch in type or length against a native item is what would explain the
            // shell failing to resolve our rows to the real file.
            {
                let w: Vec<u16> = "System.DelegateIDList"
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let mut key = windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY::default();
                if windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName(
                    windows::core::PCWSTR(w.as_ptr()),
                    &mut key,
                )
                .is_ok()
                {
                    for (i, &p) in pidls.iter().take(2).enumerate() {
                        match f2.GetDetailsEx(p, &key) {
                            Ok(v) => {
                                let raw = &v as *const _ as *const u8;
                                let vt = *(raw as *const u16);
                                println!("  item {i} DelegateIDList: vt={vt}");
                            }
                            Err(e) => println!("  item {i} DelegateIDList: {e:?}"),
                        }
                    }
                }
            }
            for cand in [
                "System.ItemUrl",
                "System.ParsingPath",
                "System.ItemNameDisplay",
                "System.ParsingName",
                "System.Search.EntryID",
                "System.Search.WorkId",
                "System.Search.RowID",
                "System.Search.Rank",
                "System.ItemFolderPathDisplay",
                "System.ItemPathDisplay",
                "System.Search.Store",
            ] {
                let w: Vec<u16> = cand.encode_utf16().chain(std::iter::once(0)).collect();
                let mut key = windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY::default();
                if windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName(
                    windows::core::PCWSTR(w.as_ptr()),
                    &mut key,
                )
                .is_err()
                {
                    println!("  {cand:30} <name does not resolve>");
                    continue;
                }
                let mut vals = Vec::new();
                for &p in pidls.iter() {
                    let v = match f2.GetDetailsEx(p, &key) {
                        Ok(v) => v,
                        Err(_) => {
                            vals.push("<err>".to_string());
                            continue;
                        }
                    };
                    let pv = windows::core::PROPVARIANT::try_from(&v).unwrap_or_default();
                    let s =
                        windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc(
                            &pv,
                        )
                        .map(|p| {
                            let s = p.to_string().unwrap_or_default();
                            CoTaskMemFree(Some(p.0 as *const _));
                            s
                        })
                        .unwrap_or_else(|_| "<empty>".into());
                    vals.push(s);
                }
                let distinct: std::collections::HashSet<&String> = vals.iter().collect();
                println!(
                    "  {cand:30} {} distinct value(s)   e.g. {:?}",
                    distinct.len(),
                    vals.first().map(|s| s.chars().take(46).collect::<String>())
                );
            }
        }

        // Everything the folder has on one item, by canonical name. Run this against a native
        // search and against a substituted one and diff the two lists: whatever the real
        // provider supplies and we do not is the entire search space for the identity.
        println!("\n--- every property on item 0 ---");
        if let Some(&p0) = pidls.first() {
            match folder
                .BindToObject::<_, windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore>(
                    p0, None,
                ) {
                Ok(store) => {
                    let n = store.GetCount().unwrap_or(0);
                    println!("  ({n} properties)");
                    for i in 0..n {
                        let mut key =
                            windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY::default();
                        if store.GetAt(i, &mut key).is_err() {
                            continue;
                        }
                        let name =
                            windows::Win32::UI::Shell::PropertiesSystem::PSGetNameFromPropertyKey(
                                &key,
                            )
                            .map(|p| {
                                let s = p.to_string().unwrap_or_default();
                                CoTaskMemFree(Some(p.0 as *const _));
                                s
                            })
                            .unwrap_or_else(|_| format!("{:?}#{}", key.fmtid, key.pid));
                        let val = match store.GetValue(&key) {
                            Ok(v) => windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc(&v)
                                .map(|p| {
                                    let s = p.to_string().unwrap_or_default();
                                    CoTaskMemFree(Some(p.0 as *const _));
                                    s.chars().take(60).collect::<String>()
                                })
                                .unwrap_or_else(|_| "<unprintable>".into()),
                            Err(_) => "<err>".into(),
                        };
                        println!("  {name} = {val}");
                    }
                }
                Err(e) => println!("  BindToObject(IPropertyStore) failed: {e}"),
            }
        }

        println!("\n--- raw byte equality ---");
        for i in 0..pidls.len() {
            for j in (i + 1)..pidls.len() {
                let same = pidl_bytes(pidls[i]) == pidl_bytes(pidls[j]);
                println!("  {i} vs {j}: identical bytes = {same}");
            }
        }

        for p in pidls {
            CoTaskMemFree(Some(p as *const _));
        }
    }
}
