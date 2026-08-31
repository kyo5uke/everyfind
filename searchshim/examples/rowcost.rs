//! Where does the time go when building a page of rows?
//!
//! A whole-drive one-character search spends about 900 ms in the daemon and about the same
//! again here, for five hundred rows. This times the two things `fill` does per row that are
//! not cheap: resolving a canonical property name to its key, and stat'ing the file.

use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::PropertiesSystem::{PSGetPropertyKeyFromName, PROPERTYKEY};

const NAMES: &[&str] = &[
    "System.ParsingPath",
    "System.ItemId",
    "System.ParsingName",
    "System.ItemNameDisplay",
    "System.ItemPathDisplay",
    "System.ItemFolderPathDisplay",
    "System.Search.RowID",
    "System.Search.Rank",
    "System.FileAttributes",
    "System.ItemType",
    "System.Kind",
    "System.SFGAOFlags",
    "System.Size",
    "System.DateModified",
    "System.DateCreated",
    "System.DateAccessed",
];

fn main() {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // 1. Resolving property names, the way `pkey` does it, once per property per row.
    let t = Instant::now();
    for _ in 0..rows {
        for n in NAMES {
            let w: Vec<u16> = n.encode_utf16().chain(std::iter::once(0)).collect();
            let mut key = PROPERTYKEY::default();
            unsafe {
                let _ = PSGetPropertyKeyFromName(PCWSTR(w.as_ptr()), &mut key);
            }
        }
    }
    let per_row = t.elapsed().as_secs_f64() * 1000.0 / rows as f64;
    println!(
        "property-name lookups : {:>7.0} ms for {rows} rows ({:.2} ms/row, {} per row)",
        t.elapsed().as_millis(),
        per_row,
        NAMES.len()
    );

    // 2. The same, resolved once and reused.
    let t = Instant::now();
    let keys: Vec<PROPERTYKEY> = NAMES
        .iter()
        .map(|n| {
            let w: Vec<u16> = n.encode_utf16().chain(std::iter::once(0)).collect();
            let mut key = PROPERTYKEY::default();
            unsafe {
                let _ = PSGetPropertyKeyFromName(PCWSTR(w.as_ptr()), &mut key);
            }
            key
        })
        .collect();
    println!(
        "  same, resolved once : {:>7.2} ms total ({} keys)",
        t.elapsed().as_secs_f64() * 1000.0,
        keys.len()
    );

    // 3. Stat'ing real files.
    let dir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let paths: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    if paths.is_empty() {
        println!("no files to stat in {dir}");
        return;
    }
    let t = Instant::now();
    for i in 0..rows {
        let _ = std::fs::metadata(&paths[i % paths.len()]);
    }
    println!(
        "file metadata         : {:>7.0} ms for {rows} rows ({:.2} ms/row)",
        t.elapsed().as_millis(),
        t.elapsed().as_secs_f64() * 1000.0 / rows as f64
    );
}
