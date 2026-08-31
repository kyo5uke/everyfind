//! A replacement Windows Search rowset.
//!
//! The search folder's DefView pulls results out of the CollatorDSO rowset with
//! two interfaces (measured, not guessed): `IRowsetLocate::GetRowsAt` pages out
//! opaque row handles, then `IGetRow::GetRowFromHROW` turns each handle into a
//! property store the view reads name/url/etc. from. Row handles are just 1-based
//! indices here. The per-row store is the stock `PSCreateMemoryPropertyStore`
//! object with our values written in: no custom property store needed.
//!
//! This object stands in for the real rowset so the folder shows Everyfind's
//! results while the breadcrumb, view, and navigation stay completely native.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use windows::core::{
    implement, IUnknown, Interface, Result, GUID, HRESULT, PCWSTR, PROPVARIANT, PWSTR,
};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY};
use windows::Win32::System::Com::IServiceProvider;
use windows::Win32::System::Com::StructuredStorage::{
    InitPropVariantFromBuffer, InitPropVariantFromFileTime, PropVariantToStringAlloc,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree, IEnumUnknown};
use windows::Win32::System::Search::{
    ICondition, IGetRow, IGetRow_Impl, IRowsetInfo, IRowsetInfo_Impl, IRowsetLocate,
    IRowsetLocate_Impl, IRowset_Impl, DBPROP, DBPROPIDSET, DBPROPSET, HACCESSOR,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyStore, IPropertyStoreCache, PSCreateMemoryPropertyStore, PSGetNameFromPropertyKey,
    PSGetPropertyKeyFromName, PROPERTYKEY, PSC_NORMAL,
};
use windows::Win32::UI::Shell::{
    IFolderView2, ILCreateFromPathW, ILFree, ILGetSize, IShellBrowser, IShellItem,
    SIGDN_DESKTOPABSOLUTEEDITING, SIGDN_FILESYSPATH, SIGDN_PARENTRELATIVEFORADDRESSBAR,
};

/// `DB_S_ENDOFROWSET`, a *success* code (high bit clear) the provider returns
/// when it hands back fewer rows than asked because the set ran out.
const DB_S_ENDOFROWSET: HRESULT = HRESULT(0x0004_0EC6u32 as i32);
static SELF_DUMP: AtomicUsize = AtomicUsize::new(0);

/// One result row: the path, and whatever the filesystem already told us about it.
///
/// Every property the view asks for is derived from these two, matching what the genuine
/// provider returns. The metadata rides along because it has usually been read already,
/// ordering the page by date modified reads it for every row before the page is handed over,
/// and [`fill`] would otherwise read it a second time for each row the view fetches. At five
/// thousand rows that second read was measured at 0.09 ms each, so it was worth carrying.
///
/// `None` means nobody has looked yet (a row rebuilt from the cached answer, or the fixed
/// mechanism-test rows), and `fill` reads it itself.
#[derive(Clone)]
pub struct Row {
    pub path: String, // e.g. r"C:\Windows\notepad.exe"
    pub meta: Option<std::fs::Metadata>,
}

impl Row {
    pub fn new(path: impl Into<String>) -> Self {
        Row {
            path: path.into(),
            meta: None,
        }
    }

    /// A row whose metadata was read on the way here.
    pub fn seen(path: impl Into<String>, meta: Option<std::fs::Metadata>) -> Self {
        Row {
            path: path.into(),
            meta,
        }
    }
}

/// Resolve a canonical name ("System.ParsingPath") to its key. Doing this by name
/// keeps the property list readable and avoids hunting for scattered constants.
fn pkey(name: &str) -> Option<PROPERTYKEY> {
    if let Some(k) = resolved().get(name) {
        return Some(*k);
    }
    resolve(name)
}

/// Every canonical name this file writes, resolved once.
///
/// `PSGetPropertyKeyFromName` goes to the property-system schema, and a row writes sixteen of
/// them. That is 0.02 ms a row (`rowcost`), which was nothing at a page of a thousand and is
/// most of a tenth of a second at five thousand, for sixteen answers that never change while
/// the process lives. The map is keyed by the same `&'static str` the call sites pass, so a
/// name that is not on the list still resolves, just not for free.
fn resolved() -> &'static std::collections::HashMap<&'static str, PROPERTYKEY> {
    static KEYS: std::sync::OnceLock<std::collections::HashMap<&'static str, PROPERTYKEY>> =
        std::sync::OnceLock::new();
    KEYS.get_or_init(|| {
        WRITTEN_PROPERTIES
            .iter()
            .filter_map(|&name| resolve(name).map(|k| (name, k)))
            .collect()
    })
}

fn resolve(name: &str) -> Option<PROPERTYKEY> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key = PROPERTYKEY::default();
    unsafe { PSGetPropertyKeyFromName(PCWSTR(wide.as_ptr()), &mut key).ok()? };
    Some(key)
}

/// The properties [`fill`] writes on a row. Kept next to the resolver it warms, and checked
/// against the property system by `every_property_name_we_write_resolves`.
const WRITTEN_PROPERTIES: &[&str] = &[
    "System.ParsingPath",
    "System.ItemId",
    "System.DelegateIDList",
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

fn put(cache: &IPropertyStoreCache, name: &str, value: PROPVARIANT) {
    if let Some(k) = pkey(name) {
        unsafe {
            let _ = cache.SetValueAndState(&k, &value, PSC_NORMAL);
        }
    }
}

/// A `SystemTime` as the `FILETIME`-typed PROPVARIANT the date columns expect.
/// Anything unrepresentable just yields no value (a blank cell, never a panic).
fn put_time(cache: &IPropertyStoreCache, name: &str, t: std::io::Result<std::time::SystemTime>) {
    let Ok(t) = t else { return };
    let Ok(dur) = t.duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    // FILETIME is 100 ns ticks since 1601; Unix epoch is 11644473600 s later.
    let ticks = dur.as_nanos() / 100 + 116_444_736_000_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    if let (Some(k), Ok(v)) = (pkey(name), unsafe { InitPropVariantFromFileTime(&ft) }) {
        unsafe {
            let _ = cache.SetValueAndState(&k, &v, PSC_NORMAL);
        }
    }
}

/// SFGAO flags, as the genuine provider reports them.
///
/// The file value is measured off a native result row (`0x4040_0177`): filesystem + stream,
/// copy/move/link/rename/delete/propsheet/droptarget. Deriving it from the folder value
/// instead (clearing only the folder bit) left `SFGAO_FILESYSANCESTOR` and
/// `SFGAO_STORAGEANCESTOR` set, telling the shell a *file* can contain filesystem children.
const SFGAO_FOLDERISH: u32 = 1_887_437_183;
const SFGAO_FILEISH: u32 = 1_077_936_503;

/// Fallbacks for a row whose file could not be stat'ed (deleted between the search and the
/// fetch, or unreachable). Real attributes are used whenever the metadata is available.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// The real shell PIDL for `path`, as bytes.
///
/// A genuine search result is a *delegate* item: it stands for a shell item elsewhere and
/// carries that item's PIDL, so the shell can resolve the thing itself: its parent folder,
/// its verbs, and the directory a program launched from it starts in. Without it, running an
/// executable from the results starts it with the wrong current directory, and anything that
/// resolves a file beside itself at run time fails; opening the same program from its folder
/// works. Measured on a native result row, which carries `System.DelegateIDList`; ours did
/// not until this.
fn delegate_idlist(path: &str) -> Option<Vec<u8>> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
        if pidl.is_null() {
            return None;
        }
        let cb = ILGetSize(Some(pidl)) as usize;
        let bytes = std::slice::from_raw_parts(pidl as *const u8, cb).to_vec();
        ILFree(Some(pidl));
        Some(bytes)
    }
}

/// Fill a row's property store the way the genuine Windows Search provider does, measured
/// against a native search in the same folder class, not guessed at.
fn fill(cache: &IPropertyStoreCache, row: &Row, rowid: u64) {
    let path = &row.path;
    let leaf = path.rsplit(['\\', '/']).next().unwrap_or(path).to_string();
    // Already read while the page was being ordered, for all but the rows that never went
    // through that (see [`Row`]). `symlink_metadata` there and here, so a reparse point is
    // described as itself, which is also what Explorer's own listing reports, since that
    // comes from `FindFirstFile`.
    let meta = match &row.meta {
        Some(m) => Some(m.clone()),
        None => std::fs::symlink_metadata(path).ok(),
    };
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);

    // A plain Windows path, not the `file:` URL this used to carry; that is the form the
    // genuine provider puts on a row (`C:\Windows\System32\downlevel\api-ms-...dll`).
    put(
        cache,
        "System.ParsingPath",
        PROPVARIANT::from(path.as_str()),
    );
    // The identity, and the whole of it. Without this the folder answers "same item" for every
    // pair of rows under `SHCIDS_CANONICALONLY`, so clicking one result painted them all
    // selected while the real selection stayed at one. Measured: 15 of 15 pairs equal before,
    // 0 of 15 after.
    //
    // Found by diffing the properties baked into a native result's PIDL against ours, which is
    // also how the three guesses that came first (`System.ItemUrl`, `System.Search.EntryID`,
    // `System.Search.Store`) were ruled out: the native item carries none of them.
    put(cache, "System.ItemId", PROPVARIANT::from(path.as_str()));
    // What makes the row stand for the real file rather than merely describe it; see
    // `delegate_idlist`.
    if let Some(idl) = delegate_idlist(path) {
        if let (Some(k), Ok(v)) = (pkey("System.DelegateIDList"), unsafe {
            InitPropVariantFromBuffer(idl.as_ptr() as *const c_void, idl.len() as u32)
        }) {
            unsafe {
                let _ = cache.SetValueAndState(&k, &v, PSC_NORMAL);
            }
        }
    }
    put(
        cache,
        "System.ParsingName",
        PROPVARIANT::from(leaf.as_str()),
    );
    put(
        cache,
        "System.ItemNameDisplay",
        PROPVARIANT::from(leaf.as_str()),
    );
    // The two path columns a search view shows next to the name. A result list is only useful
    // if you can see *where* each hit lives, and both of these read empty without this.
    put(
        cache,
        "System.ItemPathDisplay",
        PROPVARIANT::from(path.as_str()),
    );
    let parent = path
        .rsplit_once(['\\', '/'])
        .map(|(dir, _)| dir)
        .unwrap_or("");
    put(
        cache,
        "System.ItemFolderPathDisplay",
        PROPVARIANT::from(parent),
    );
    put(cache, "System.Search.RowID", PROPVARIANT::from(rowid));
    // Everyfind hands rows back best-match-first (exact > prefix > substring, then
    // shorter path). Express that as a *descending* rank so the view's "most
    // relevant" sort reflects it, and, unlike a flat rank, restores that order
    // after the user has sorted by another column (a flat rank collapses to a tie,
    // and a stable sort would leave the rows in the other column's order forever).
    let rank = 1_000_000u32.saturating_sub(rowid as u32);
    put(cache, "System.Search.Rank", PROPVARIANT::from(rank));

    // The real attributes when the file is reachable. A constant `FILE_ATTRIBUTE_NORMAL`
    // describes every hidden and system file as an ordinary one, so the shell has nothing to
    // filter on and shows them to a user who has "show hidden files" off: the one place
    // these results would visibly disagree with the rest of Explorer.
    let attrs = meta
        .as_ref()
        .map(|m| m.file_attributes())
        .unwrap_or(if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        });
    put(cache, "System.FileAttributes", PROPVARIANT::from(attrs));

    if is_dir {
        put(cache, "System.ItemType", PROPVARIANT::from("Directory"));
        put(cache, "System.Kind", PROPVARIANT::from("folder"));
        put(
            cache,
            "System.SFGAOFlags",
            PROPVARIANT::from(SFGAO_FOLDERISH),
        );
    } else {
        let ext = leaf.rfind('.').map(|i| &leaf[i..]).unwrap_or("");
        put(cache, "System.ItemType", PROPVARIANT::from(ext));
        put(cache, "System.SFGAOFlags", PROPVARIANT::from(SFGAO_FILEISH));
        if let Some(m) = &meta {
            put(cache, "System.Size", PROPVARIANT::from(m.len()));
        }
    }
    if let Some(m) = &meta {
        put_time(cache, "System.DateModified", m.modified());
        put_time(cache, "System.DateCreated", m.created());
        put_time(cache, "System.DateAccessed", m.accessed());
    }
}

#[implement(IRowsetLocate, IGetRow, IRowsetInfo)]
pub struct EfRowset {
    rows: Vec<Row>,
    cursor: AtomicIsize,
}

impl EfRowset {
    /// Write `hrows` (1-based indices) for the window [start, start+crows) into the
    /// caller's or a freshly allocated array, and report how many were produced.
    unsafe fn fetch(
        &self,
        start: isize,
        crows: isize,
        pcrowsobtained: *mut usize,
        prghrows: *mut *mut usize,
    ) -> Result<()> {
        let len = self.rows.len() as isize;
        let (first, count) = if crows >= 0 {
            let f = start.max(0);
            (f, (len - f).clamp(0, crows))
        } else {
            // Backward fetch: crows is negative and the window *ends* at `start`. Pull that
            // end back inside the set first: a start past the last row would otherwise hand
            // out handles for rows that do not exist, and `GetRowFromHROW` can only reject
            // them.
            let want = -crows;
            let last = start.min(len - 1);
            let f = (last - want + 1).max(0);
            (f, (last - f + 1).clamp(0, want))
        };
        let n = count.max(0) as usize;

        if !prghrows.is_null() {
            let buf = if (*prghrows).is_null() {
                let p = CoTaskMemAlloc(n.max(1) * std::mem::size_of::<usize>()) as *mut usize;
                // Explorer is the host: a failed allocation must come back as an error code,
                // not as a write through null.
                if p.is_null() {
                    if !pcrowsobtained.is_null() {
                        *pcrowsobtained = 0;
                    }
                    return Err(E_OUTOFMEMORY.into());
                }
                *prghrows = p;
                p
            } else {
                *prghrows
            };
            for i in 0..n {
                *buf.add(i) = (first as usize) + i + 1; // HROW is 1-based
            }
        }
        if !pcrowsobtained.is_null() {
            *pcrowsobtained = n;
        }
        if (n as isize) < crows.abs() {
            Err(DB_S_ENDOFROWSET.into())
        } else {
            Ok(())
        }
    }
}

impl IRowset_Impl for EfRowset_Impl {
    fn AddRefRows(
        &self,
        crows: usize,
        _rghrows: *const usize,
        rgrefcounts: *mut u32,
        rgrowstatus: *mut u32,
    ) -> Result<()> {
        unsafe {
            for i in 0..crows {
                if !rgrefcounts.is_null() {
                    *rgrefcounts.add(i) = 1;
                }
                if !rgrowstatus.is_null() {
                    *rgrowstatus.add(i) = 0; // DBROWSTATUS_S_OK
                }
            }
        }
        Ok(())
    }

    fn GetData(&self, _hrow: usize, _haccessor: HACCESSOR, _pdata: *mut c_void) -> Result<()> {
        // The folder reads rows through IGetRow, never accessor bindings.
        Err(E_NOTIMPL.into())
    }

    fn GetNextRows(
        &self,
        _hreserved: usize,
        lrowsoffset: isize,
        crows: isize,
        pcrowsobtained: *mut usize,
        prghrows: *mut *mut usize,
    ) -> Result<()> {
        let cur = self.cursor.load(Ordering::SeqCst);
        let start = cur + lrowsoffset.max(0);
        let r = unsafe { self.fetch(start, crows, pcrowsobtained, prghrows) };
        let got = unsafe {
            if pcrowsobtained.is_null() {
                0
            } else {
                *pcrowsobtained
            }
        };
        self.cursor.store(start + got as isize, Ordering::SeqCst);
        r
    }

    fn ReleaseRows(
        &self,
        crows: usize,
        _rghrows: *const usize,
        _rgrowoptions: *const u32,
        rgrefcounts: *mut u32,
        rgrowstatus: *mut u32,
    ) -> Result<()> {
        unsafe {
            for i in 0..crows {
                if !rgrefcounts.is_null() {
                    *rgrefcounts.add(i) = 0;
                }
                if !rgrowstatus.is_null() {
                    *rgrowstatus.add(i) = 0;
                }
            }
        }
        Ok(())
    }

    fn RestartPosition(&self, _hreserved: usize) -> Result<()> {
        self.cursor.store(0, Ordering::SeqCst);
        Ok(())
    }
}

impl IRowsetLocate_Impl for EfRowset_Impl {
    fn Compare(
        &self,
        _hreserved: usize,
        _cb1: usize,
        _pb1: *const u8,
        _cb2: usize,
        _pb2: *const u8,
    ) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn GetRowsAt(
        &self,
        _hreserved1: usize,
        _hreserved2: usize,
        cbbookmark: usize,
        pbookmark: *const u8,
        lrowsoffset: isize,
        crows: isize,
        pcrowsobtained: *mut usize,
        prghrows: *mut *mut usize,
    ) -> Result<()> {
        // Bookmark selects the base row: DBBMK_FIRST (0x01) is the first, DBBMK_LAST (0x02)
        // the *last*, not one past it. Treating it as one past made offset 0 name a row that
        // does not exist: a forward fetch from there returned nothing, and a backward fetch
        // handed out `len + 1`, which `GetRowFromHROW` can only reject. Anything else is read
        // as "from the start"; the shell has only ever been seen to send DBBMK_FIRST.
        let base = unsafe {
            if cbbookmark >= 1 && !pbookmark.is_null() && *pbookmark == 2 {
                self.rows.len().saturating_sub(1) as isize
            } else {
                0
            }
        };
        if crate::logging_rows() {
            crate::log(&format!(
                "PROV: GetRowsAt base={base} off={lrowsoffset} crows={crows} (have {})",
                self.rows.len()
            ));
        }
        unsafe { self.fetch(base + lrowsoffset, crows, pcrowsobtained, prghrows) }
    }

    fn GetRowsByBookmark(
        &self,
        _hreserved: usize,
        _crows: usize,
        _rgcbbookmarks: *const usize,
        _rgpbookmarks: *const *const u8,
        _rghrows: *mut usize,
        _rgrowstatus: *mut u32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Hash(
        &self,
        _hreserved: usize,
        _cbookmarks: usize,
        _rgcbbookmarks: *const usize,
        _rgpbookmarks: *const *const u8,
        _rghashedvalues: *mut usize,
        _rgbookmarkstatus: *mut u32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

/// Print what the shell asked this rowset about itself.
///
/// Diagnostics only. It reads a caller-owned array, so callers gate it on both
/// [`crate::debugging`] and a non-null pointer.
unsafe fn log_requested_properties(csets: u32, rgsets: *const DBPROPIDSET) {
    let mut s = format!("RSINFO: GetProperties csets={csets}");
    for i in 0..csets.min(8) as usize {
        // DBPROPIDSET { DBPROPID* rgPropertyIDs; ULONG cPropertyIDs; GUID }
        let p = rgsets.add(i) as *const u8;
        let ids = *(p as *const usize);
        let cids = *(p.add(8) as *const u32);
        let g = &*(p.add(16) as *const GUID);
        s.push_str(&format!(" [set={g:?} n={cids}"));
        for j in 0..cids.min(12) as usize {
            if ids != 0 {
                s.push_str(&format!(" id{}", *((ids as *const u32).add(j))));
            }
        }
        s.push(']');
    }
    crate::log(&s);
}

/// `DB_S_ERRORSOCCURRED`: success, but some properties could not be supplied.
const DB_S_ERRORSOCCURRED: HRESULT = HRESULT(0x0004_0EC8u32 as i32);
const DBPROPSTATUS_NOTSUPPORTED: u32 = 1;
const DBPROPSTATUS_OK: u32 = 0;
const VT_I4: u16 = 3;
const VT_BOOL: u16 = 11;
const VARIANT_TRUE: i64 = -1;

/// What the view asks a search rowset about itself, and what the genuine provider answers,
/// read off the real thing once. The view discards a rowset that cannot describe itself, and
/// obtaining a real `IRowsetInfo` to forward to meant running a native query first (measured
/// at 5.4 s on a drive-wide search), so these answers replace it.
///
/// The second one is the row count, and it is *not* a constant: the value 12 was what the real
/// provider happened to answer for the query it was measured against. Reporting it verbatim
/// capped every search at twelve rows; the view fetched and built all thirty-one rows we
/// handed it and then kept exactly the first twelve, whatever we sent (measured at 31, 37 and
/// 50 rows).
const ROWSET_DESCRIPTION: &[(u128, u32, u16)] =
    &[(0xAA6EE6B0_E828_11D0_B23E_00AA0047FC01, 14, VT_BOOL)];
const ROWCOUNT_SET: u128 = 0x0B63E36E_9CCC_11D0_BCDB_00805FCCCE04;
const ROWCOUNT_ID: u32 = 1000;

fn described(set: &GUID, id: u32, rows: usize) -> Option<(u16, i64)> {
    if id == ROWCOUNT_ID && GUID::from_u128(ROWCOUNT_SET) == *set {
        return Some((VT_I4, rows as i64));
    }
    ROWSET_DESCRIPTION
        .iter()
        .find(|(g, i, _)| *i == id && GUID::from_u128(*g) == *set)
        .map(|(_, _, vt)| (*vt, VARIANT_TRUE))
}

/// Answer `IRowsetInfo::GetProperties` on our own: one property set per requested
/// set, each property answered from `ROWSET_DESCRIPTION` or marked not supported.
/// The returned memory is task-allocated, as OLE DB requires.
unsafe fn answer_properties(
    csets: u32,
    rgsets: *const DBPROPIDSET,
    pcsets: *mut u32,
    prgsets: *mut *mut DBPROPSET,
    rows: usize,
) -> Result<()> {
    if pcsets.is_null() || prgsets.is_null() {
        return Err(E_INVALIDARG.into());
    }
    *pcsets = 0;
    *prgsets = std::ptr::null_mut();
    if csets == 0 || rgsets.is_null() {
        return Ok(());
    }
    let sets = CoTaskMemAlloc(csets as usize * std::mem::size_of::<DBPROPSET>()) as *mut DBPROPSET;
    if sets.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    let mut any_unsupported = false;
    for i in 0..csets as usize {
        let req = &*rgsets.add(i);
        let n = req.cPropertyIDs as usize;
        let props = if n == 0 {
            std::ptr::null_mut()
        } else {
            CoTaskMemAlloc(n * std::mem::size_of::<DBPROP>()) as *mut DBPROP
        };
        // A failed per-set allocation would otherwise be written through below. Report the
        // set as carrying no properties rather than faulting inside Explorer.
        let n = if props.is_null() { 0 } else { n };
        for j in 0..n {
            let p = props.add(j);
            std::ptr::write_bytes(p as *mut u8, 0, std::mem::size_of::<DBPROP>());
            let id = if req.rgPropertyIDs.is_null() {
                0
            } else {
                *req.rgPropertyIDs.add(j)
            };
            (*p).dwPropertyID = id;
            match described(&req.guidPropertySet, id, rows) {
                Some((vt, value)) => {
                    (*p).dwStatus = DBPROPSTATUS_OK;
                    // VARIANT: the type tag leads, the payload sits eight bytes in.
                    let v = &mut (*p).vValue as *mut _ as *mut u8;
                    *(v as *mut u16) = vt;
                    *(v.add(8) as *mut i64) = value;
                }
                None => {
                    (*p).dwStatus = DBPROPSTATUS_NOTSUPPORTED;
                    any_unsupported = true;
                }
            }
        }
        let s = sets.add(i);
        std::ptr::write_bytes(s as *mut u8, 0, std::mem::size_of::<DBPROPSET>());
        (*s).rgProperties = props;
        (*s).cProperties = n as u32;
        (*s).guidPropertySet = req.guidPropertySet;
    }
    *pcsets = csets;
    *prgsets = sets;
    if any_unsupported {
        Err(DB_S_ERRORSOCCURRED.into())
    } else {
        Ok(())
    }
}

impl IRowsetInfo_Impl for EfRowset_Impl {
    // Forward through the real IRowsetInfo's raw vtable so the exact call is
    // relayed (the crate's friendly wrappers reshape the arguments).
    fn GetProperties(
        &self,
        cpropertyidsets: u32,
        rgpropertyidsets: *const DBPROPIDSET,
        pcpropertysets: *mut u32,
        prgpropertysets: *mut *mut DBPROPSET,
    ) -> Result<()> {
        // Diagnostics walk the caller's array, so they run only when asked for and only when
        // there is an array to walk. `answer_properties` below is what handles the null case,
        // and it has to be *reached*, not faulted past on the way to a log line nobody reads.
        if crate::debugging() && !rgpropertyidsets.is_null() {
            unsafe { log_requested_properties(cpropertyidsets, rgpropertyidsets) };
        }
        // Answered from what the genuine provider was measured to say. This used to forward
        // to a real `IRowsetInfo` instead, which meant running one native query per process
        // just to obtain it, and that query was the 5.4 s wait on a window's first search.
        unsafe {
            answer_properties(
                cpropertyidsets,
                rgpropertyidsets,
                pcpropertysets,
                prgpropertysets,
                self.rows.len(),
            )
        }
    }

    fn GetReferencedRowset(&self, _iordinal: usize, _riid: *const GUID) -> Result<IUnknown> {
        Err(E_NOTIMPL.into())
    }

    fn GetSpecification(&self, _riid: *const GUID) -> Result<IUnknown> {
        Err(E_NOTIMPL.into())
    }
}

impl IGetRow_Impl for EfRowset_Impl {
    fn GetRowFromHROW(
        &self,
        _punkouter: Option<&IUnknown>,
        hrow: usize,
        riid: *const GUID,
    ) -> Result<IUnknown> {
        if crate::logging_rows() {
            crate::log(&format!("PROV: GetRowFromHROW hrow={hrow}"));
        }
        let idx = hrow
            .checked_sub(1)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        let row = self
            .rows
            .get(idx)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        unsafe {
            let mut ppv: *mut c_void = std::ptr::null_mut();
            PSCreateMemoryPropertyStore(&IPropertyStoreCache::IID, &mut ppv)?;
            let cache = IPropertyStoreCache::from_raw(ppv);
            fill(&cache, row, hrow as u64);
            if SELF_DUMP.fetch_add(1, Ordering::SeqCst) < 2 {
                crate::log(&format!("PROV: built row {hrow} for {}", row.path));
                dump_store(cache.as_raw());
            }
            let mut out: *mut c_void = std::ptr::null_mut();
            cache.query(riid, &mut out).ok()?;
            Ok(IUnknown::from_raw(out))
        }
    }

    fn GetURLFromHROW(&self, hrow: usize) -> Result<PWSTR> {
        let idx = hrow
            .checked_sub(1)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        let row = self
            .rows
            .get(idx)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        // Hand back a task-allocated copy of the URL.
        let url = format!("file:{}", row.path.replace('\\', "/"));
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let p = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
            if p.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            Ok(PWSTR(p))
        }
    }
}

/// Print every property the *real* provider puts on a row, with canonical names.
/// This is how we learn exactly which keys and value shapes the view consumes,
/// rather than guessing at them.
pub unsafe fn dump_store(punk: *mut c_void) {
    let Some(unk) = IUnknown::from_raw_borrowed(&punk) else {
        return;
    };
    let store: IPropertyStore = match unk.cast() {
        Ok(s) => s,
        Err(e) => {
            crate::log(&format!("DUMP: not an IPropertyStore: {e:?}"));
            return;
        }
    };
    let n = store.GetCount().unwrap_or(0);
    crate::log(&format!("DUMP: row has {n} properties"));
    for i in 0..n {
        let mut key = PROPERTYKEY::default();
        if store.GetAt(i, &mut key).is_err() {
            continue;
        }
        let name = PSGetNameFromPropertyKey(&key)
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const c_void));
                s
            })
            .unwrap_or_else(|_| format!("{:?}#{}", key.fmtid, key.pid));
        let val = match store.GetValue(&key) {
            Ok(v) => PropVariantToStringAlloc(&v)
                .map(|p| {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const c_void));
                    s
                })
                .unwrap_or_else(|_| "<unprintable>".into()),
            Err(_) => "<err>".into(),
        };
        crate::log(&format!("DUMP:   {name} = {val}"));
    }
}

/// The folder a search was started from, asked of the site the shell attached to
/// the command. This is per-search (unlike anything cached per process), and
/// costs nothing: no native query needs to run to learn it.
///
/// Reaching an `IShellBrowser` at all is what says the caller is a shell view; nothing else
/// this DLL is activated for has one. Naming the folder then takes two readings, because the
/// view is not always sitting on a filesystem folder when the search runs: a search typed into
/// a results window, or a `search-ms:` navigation, leaves the view on a *search* folder, which
/// has no filesystem path. That folder does name its origin, in the `crumb=location:` term of
/// its own parsing name, so ask for the path, and fall back to reading the crumb.
/// What the site the shell attaches to the command actually is.
///
/// Diagnostic. `QueryService(SID_SShellBrowser)` on it returns `E_NOINTERFACE` (measured), so
/// the documented route to the searched folder is closed; this reports what routes are open.
pub unsafe fn dump_site(site: *mut c_void) {
    if !crate::debugging() {
        return;
    }
    let Some(unk) = IUnknown::from_raw_borrowed(&site) else {
        crate::log("SITE?: not an IUnknown");
        return;
    };
    if let Ok(p) = unk.cast::<windows::Win32::System::Com::IPersist>() {
        match p.GetClassID() {
            Ok(c) => crate::log(&format!("SITE?: class = {c:?}")),
            Err(e) => crate::log(&format!("SITE?: GetClassID failed: {e:?}")),
        }
    }
    macro_rules! probe {
        ($name:literal, $t:ty) => {
            crate::log(&format!(
                "SITE?: QI {:22} = {}",
                $name,
                unk.cast::<$t>().is_ok()
            ));
        };
    }
    probe!("IServiceProvider", IServiceProvider);
    probe!("IShellBrowser", IShellBrowser);
    probe!("IFolderView2", IFolderView2);
    probe!("IShellItem", IShellItem);
    probe!("IShellFolder", windows::Win32::UI::Shell::IShellFolder);
    probe!("IShellView", windows::Win32::UI::Shell::IShellView);
    probe!("IOleWindow", windows::Win32::System::Ole::IOleWindow);

    // Every service the shell view chain is normally reachable through.
    if let Ok(sp) = unk.cast::<IServiceProvider>() {
        for (name, sid) in [
            (
                "SID_SShellBrowser",
                GUID::from_u128(0x000214E2_0000_0000_C000_000000000046),
            ),
            (
                "SID_STopLevelBrowser",
                GUID::from_u128(0x4C96BE40_915C_11CF_99D3_00AA004AE837),
            ),
            (
                "SID_SFolderView",
                GUID::from_u128(0xCDE725B0_CCC9_4519_917E_325D72FAB4CE),
            ),
            (
                "IID_IFolderView",
                GUID::from_u128(0xCDE725B0_CCC9_4519_917E_325D72FAB4CE),
            ),
        ] {
            let ok = sp.QueryService::<IUnknown>(&sid).is_ok();
            crate::log(&format!("SITE?: QueryService {name:22} = {ok}"));
        }
    }
}

/// What the shell's site says this search is: the folder it started from, and (when the
/// same string carried them) the terms typed into the box.
///
/// Both come out of one `search-ms:` URL, so they are returned together. They used to be
/// returned one at a time, with the terms left in a process-wide slot for the caller to pick
/// up afterwards; one explorer.exe serves several windows, so a second search reaching this
/// between the two halves of the first left one window searching for the other's term.
pub struct SiteSearch {
    pub scope: String,
    pub term: Option<String>,
}

pub unsafe fn scope_from_site(site: *mut c_void) -> Option<SiteSearch> {
    // SID_SShellBrowser: the service id for the browser hosting this view.
    const SID_SSHELLBROWSER: GUID = GUID::from_u128(0x000214E2_0000_0000_C000_000000000046);
    // SID_SFolderView: the service id for the view itself.
    const SID_SFOLDERVIEW: GUID = GUID::from_u128(0xCDE725B0_CCC9_4519_917E_325D72FAB4CE);

    // `from_raw_borrowed::<IServiceProvider>` would *not* do this; it reinterprets the
    // pointer, and the site is an `IUnknown` whose vtable has three entries. Calling
    // `QueryService` through it invoked whatever the object happens to keep in slot 3, which
    // answered `E_NOINTERFACE` every time and made the scope look permanently unavailable.
    // Asking for the interface is the difference between "the shell will not tell us" and
    // "we never asked".
    let Some(unk) = IUnknown::from_raw_borrowed(&site) else {
        crate::log("SITE: the site is not a COM object");
        return None;
    };
    let sp: IServiceProvider = match unk.cast() {
        Ok(sp) => sp,
        Err(e) => {
            crate::log(&format!("SITE: the site is not an IServiceProvider: {e:?}"));
            return None;
        }
    };

    // Two routes to the view, tried in order, because the obvious one does not work here:
    // `QueryService(SID_SShellBrowser)` asked for `IShellBrowser` returns E_NOINTERFACE, while
    // the same service answers a request for plain `IUnknown` (measured). The service is
    // there; it just will not hand over that interface. Every failure is reported: when this
    // chain breaks, the only outward sign is that searches quietly stop being scoped.
    let folder_view: IFolderView2 = 'view: {
        match sp.QueryService::<IUnknown>(&SID_SFOLDERVIEW) {
            Ok(u) => match u.cast::<IFolderView2>() {
                Ok(fv) => break 'view fv,
                Err(e) => crate::log(&format!("SITE: SID_SFolderView is not IFolderView2: {e:?}")),
            },
            Err(e) => crate::log(&format!(
                "SITE: QueryService(SID_SFolderView) failed: {e:?}"
            )),
        }
        match sp.QueryService::<IUnknown>(&SID_SSHELLBROWSER) {
            Ok(u) => match u
                .cast::<IShellBrowser>()
                .and_then(|b| b.QueryActiveShellView())
                .and_then(|v| v.cast::<IFolderView2>())
            {
                Ok(fv) => break 'view fv,
                Err(e) => crate::log(&format!("SITE: browser route failed: {e:?}")),
            },
            Err(e) => crate::log(&format!(
                "SITE: QueryService(SID_SShellBrowser) failed: {e:?}"
            )),
        }
        return None;
    };
    let folder: IShellItem = match folder_view.GetFolder() {
        Ok(f) => f,
        Err(e) => {
            crate::log(&format!("SITE: IFolderView2::GetFolder failed: {e:?}"));
            return None;
        }
    };

    let read = |kind| -> Option<String> {
        let name = folder.GetDisplayName(kind).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        s.filter(|s| !s.is_empty())
    };
    // A real filesystem path: the view is a folder, not a search, so it names no terms.
    if let Some(path) = read(SIGDN_FILESYSPATH) {
        return Some(SiteSearch {
            scope: path,
            term: None,
        });
    }
    if crate::debugging() {
        use windows::Win32::UI::Shell::{
            SIGDN_DESKTOPABSOLUTEEDITING, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVE,
            SIGDN_PARENTRELATIVEEDITING, SIGDN_PARENTRELATIVEFORADDRESSBAR,
            SIGDN_PARENTRELATIVEPARSING, SIGDN_URL,
        };
        for (name, kind) in [
            ("NORMALDISPLAY", SIGDN_NORMALDISPLAY),
            ("PARENTRELATIVEPARSING", SIGDN_PARENTRELATIVEPARSING),
            ("PARENTRELATIVE", SIGDN_PARENTRELATIVE),
            ("PARENTRELATIVEEDITING", SIGDN_PARENTRELATIVEEDITING),
            (
                "PARENTRELATIVEFORADDRESSBAR",
                SIGDN_PARENTRELATIVEFORADDRESSBAR,
            ),
            ("DESKTOPABSOLUTEEDITING", SIGDN_DESKTOPABSOLUTEEDITING),
            ("URL", SIGDN_URL),
        ] {
            crate::log(&format!(
                "SITE?: {name:28} = {:?}",
                read(kind).unwrap_or_default()
            ));
        }
    }
    // By the time the site is attached the view is already showing the *search* folder, even
    // when the search was typed into a folder window, so there is no filesystem path to read
    // and the origin has to come out of the search itself. Of the ways to name that folder,
    // only these two spell it as the `search-ms:` URL that carries `crumb=location:`
    // (measured; DESKTOPABSOLUTEPARSING gives a display string, URL gives nothing).
    for kind in [
        SIGDN_DESKTOPABSOLUTEEDITING,
        SIGDN_PARENTRELATIVEFORADDRESSBAR,
    ] {
        let Some(name) = read(kind) else { continue };
        if let Some(scope) = scope_from_search_url(&name) {
            crate::log(&format!("SITE: scope from the search crumb = '{scope}'"));
            let term = term_from_search_url(&name);
            if let Some(t) = &term {
                crate::log(&format!("SITE: terms from the search crumb = '{t}'"));
            }
            return Some(SiteSearch { scope, term });
        }
    }
    crate::log("SITE: the view names no folder we can read");
    None
}

/// The terms typed into the search box, out of a `search-ms:` URL.
///
/// Two shapes, both measured on this machine:
///
/// ```text
/// search-ms:query=virtual insanity&crumb=location:C%3A%5CUsers%5Cme
/// search-ms:displayname=...&crumb=すべてのテキスト：(virtual%20insanity)&crumb=location:C%3A...
/// ```
///
/// The first is the documented parameter. The second is what Explorer writes when the terms
/// are typed into the box, and its label is *localised*: Japanese here, and with a fullwidth
/// colon behind it. So the label is never read: the terms are whatever sits between the first
/// `(` and the last `)` of a crumb that is not the location. A crumb without parentheses
/// (`kind:pics` and the like) is a filter rather than typed text and is passed over.
pub fn term_from_search_url(url: &str) -> Option<String> {
    if !url.to_ascii_lowercase().starts_with("search-ms:") {
        return None;
    }
    let mut best: Option<String> = None;
    for term in url.split('&') {
        // Only the first term carries the scheme; the rest are bare `name=value`.
        let bare = term.strip_prefix("search-ms:").unwrap_or(term);
        if let Some(q) = bare.strip_prefix("query=") {
            let decoded = percent_decode(q);
            if !decoded.trim().is_empty() {
                return Some(decoded.trim().to_string());
            }
        }
        let Some(crumb) = bare.strip_prefix("crumb=") else {
            continue;
        };
        if crumb.starts_with("location:") {
            continue;
        }
        // Two shapes carry the typed text, both without a readable label:
        //   label(term)  - older Windows builds and saved searches wrap it in parentheses.
        //   label：term   - Windows 11 (26200+) drops the parentheses and leaves the term
        //                  straight after the label's FULLWIDTH colon (U+FF1A). Measured on
        //                  this machine: `crumb=すべてのテキスト：taikonaut`.
        // Property filters (`kind:pics`, `size:large`) use an ASCII colon and no parentheses,
        // so neither shape captures them and they keep deferring to Windows.
        let captured = if let (Some(open), Some(close)) = (crumb.find('('), crumb.rfind(')')) {
            (close > open + 1).then(|| percent_decode(&crumb[open + 1..close]))
        } else {
            // No parentheses: the term is whatever follows the last fullwidth colon. A crumb
            // with only an ASCII colon (a filter) has no '：' and is passed over. Residual
            // tradeoff: a localised filter that also uses '：' (e.g. 種類：ピクチャ) would be
            // read as a term - a worse answer, never a stopped one, which is this shim's rule.
            crumb
                .rfind('：')
                .map(|i| percent_decode(&crumb[i + '：'.len_utf8()..]))
        };
        if let Some(decoded) = captured {
            if !decoded.trim().is_empty() && best.is_none() {
                best = Some(decoded.trim().to_string());
            }
        }
    }
    best
}

/// The folder named by a `search-ms:` URL's `crumb=location:...` term.
///
/// Explorer builds these when it navigates to a search, and the location is percent-encoded
/// inside a `&`-separated term list. Anything that is not such a URL yields nothing, so a
/// caller can treat "no crumb" as "no scope" rather than as a guess.
pub fn scope_from_search_url(url: &str) -> Option<String> {
    if !url.to_ascii_lowercase().starts_with("search-ms:") {
        return None;
    }
    let raw = url
        .split('&')
        .find_map(|term| term.split_once("crumb=")?.1.strip_prefix("location:"))?;
    let decoded = percent_decode(raw);
    let trimmed = decoded.trim_end_matches('\\');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Percent-decoding, enough for a path in a `search-ms:` crumb (`%3A` for the colon, `%5C`
/// for separators, `%20` for spaces). Invalid escapes are left as written rather than
/// dropped: a mangled path should fail to match a folder, not silently become another one.
///
/// Bytes throughout, never `&s[i..j]`. The two characters after a `%` are not guaranteed to be
/// ASCII: `%` is a legal character in a Windows file name, and the crumb this parses carries
/// un-escaped localized text, so `100%達成` arrives verbatim and a byte-index slice lands
/// mid-character. That panics, and this runs inside `extern "system"` hooks Explorer calls,
/// an unwind there does not return an error, it takes explorer.exe down with it.
fn percent_decode(s: &str) -> String {
    /// One hex digit's value, or `None` for anything else (including any non-ASCII byte).
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pull the search term and scope out of the shell's query condition: the tree
/// it builds from the search box *before* Execute, so reading it here means the
/// native query never has to run. Walks the ICondition and collects leaf
/// (property, value) pairs, then picks the term and the `file:` scope.
pub unsafe fn extract_query(raw: *mut c_void) -> Option<(String, String)> {
    let cond = ICondition::from_raw_borrowed(&raw)?;
    let mut leaves: Vec<(String, String)> = Vec::new();
    walk(cond, &mut leaves, 0);

    let scope = leaves
        .iter()
        .find(|(_, v)| v.starts_with("file:"))
        .map(|(_, v)| v.trim_start_matches("file:").replace('/', "\\"))
        .unwrap_or_default();
    // Every word, not the first one. The shell splits what was typed into one leaf per word
    // ("virtual insanity" arrives as two) and taking a single leaf searched for "virtual"
    // alone, so `VirtualboxVMs` outranked the file actually being looked for. Joined by
    // spaces, which Everyfind reads as "all of these", the same as Everything and the same as
    // the `ef` command line.
    //
    // Leaves that are not the query come through here too: the shell attaches its own filters
    // (`System.SFGAOFlags`, and the folder as a `file:` URL), so only the ones that carry a
    // typed word are taken.
    let words: Vec<String> = leaves
        .iter()
        .filter(|(p, v)| {
            !v.is_empty()
                && !v.starts_with("file:")
                && (p.eq_ignore_ascii_case("System.Generic.String")
                    || p.eq_ignore_ascii_case("System.ItemNameDisplay"))
        })
        .map(|(_, v)| v.trim_matches('"').trim_end_matches('*').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    // A word with a space in it has to stay one term, or it becomes two.
    let term = words
        .iter()
        .map(|w| {
            if w.contains(' ') {
                format!("\"{w}\"")
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if term.is_empty() {
        None
    } else {
        Some((term, scope))
    }
}

unsafe fn walk(cond: &ICondition, out: &mut Vec<(String, String)>, depth: usize) {
    const CT_LEAF: i32 = 3; // CT_LEAF_CONDITION
    if depth > 40 || out.len() > 300 {
        return;
    }
    let ct = match cond.GetConditionType() {
        Ok(c) => c.0,
        Err(_) => return,
    };
    if ct == CT_LEAF {
        let mut pname = PWSTR::null();
        let mut pv = PROPVARIANT::default();
        // The shell states which comparison it wants, not just the value. We have never read
        // it: every term has been matched with Everyfind's own substring rule regardless of
        // what Explorer asked for. Log it so the two can be compared.
        let mut cop = windows::Win32::System::Search::Common::CONDITION_OPERATION::default();
        if cond
            .GetComparisonInfo(Some(&mut pname), Some(&mut cop), Some(&mut pv))
            .is_ok()
        {
            let opname = match cop.0 {
                0 => "IMPLICIT",
                1 => "EQUAL",
                2 => "NOTEQUAL",
                7 => "VALUE_STARTSWITH",
                8 => "VALUE_ENDSWITH",
                9 => "VALUE_CONTAINS",
                10 => "VALUE_NOTCONTAINS",
                11 => "DOSWILDCARDS",
                12 => "WORD_EQUAL",
                13 => "WORD_STARTSWITH",
                14 => "APPLICATION_SPECIFIC",
                _ => "other",
            };
            crate::log(&format!("COND op: {opname} ({})", cop.0));
            let prop = if pname.is_null() {
                String::new()
            } else {
                let s = pname.to_string().unwrap_or_default();
                CoTaskMemFree(Some(pname.0 as *const c_void));
                s
            };
            let val = PropVariantToStringAlloc(&pv)
                .ok()
                .map(|p| {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const c_void));
                    s
                })
                .unwrap_or_default();
            crate::log(&format!("COND leaf: '{prop}' = '{val}'"));
            out.push((prop, val));
        }
    } else if let Ok(en) = cond.GetSubConditions::<IEnumUnknown>() {
        loop {
            let mut buf: [Option<IUnknown>; 1] = [None];
            let mut fetched = 0u32;
            if en.Next(&mut buf, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            if let Some(u) = buf[0].take() {
                if let Ok(sub) = u.cast::<ICondition>() {
                    walk(&sub, out, depth + 1);
                }
            }
        }
    }
}

/// A few files that certainly exist, used to prove the substitution mechanism
/// end to end, decoupled from query parsing and everyfind.
pub fn fixed_rows() -> Vec<Row> {
    vec![
        Row::new(r"C:\Windows\notepad.exe"),
        Row::new(r"C:\Windows\explorer.exe"),
        Row::new(r"C:\Windows\System32\cmd.exe"),
    ]
}

/// Build the rowset and hand back the interface the folder's `Execute` asked for. Returns an
/// HRESULT for the shim's raw call site.
///
/// `Execute` asks for `IRowset`; the generated `QueryInterface` answers it from the
/// `IRowsetLocate` we implement, since `IRowsetLocate` derives from it.
pub unsafe fn make_rowset(rows: Vec<Row>, riid: *const c_void, out: *mut *mut c_void) -> i32 {
    let obj: IRowsetLocate = EfRowset {
        rows,
        cursor: AtomicIsize::new(0),
    }
    .into();
    obj.query(riid as *const GUID, out).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rowset(n: usize) -> EfRowset {
        EfRowset {
            rows: (0..n).map(|i| Row::new(format!(r"C:\f{i}"))).collect(),
            cursor: AtomicIsize::new(0),
        }
    }

    /// Drive `fetch` with a caller-supplied buffer (the shell supplies one too) and read back
    /// the handles it wrote, plus the code it returned.
    fn fetch(rs: &EfRowset, start: isize, crows: isize) -> (Vec<usize>, HRESULT) {
        let mut buf = vec![0usize; 64];
        let mut got = 0usize;
        let mut p = buf.as_mut_ptr();
        let hr = match unsafe { rs.fetch(start, crows, &mut got, &mut p) } {
            Ok(()) => HRESULT(0),
            Err(e) => e.code(),
        };
        (buf[..got].to_vec(), hr)
    }

    /// Every canonical name a row is filled with has to resolve to a property key.
    ///
    /// `put` is silent when it does not: `pkey` returns `None`, nothing is written, and the
    /// row simply lacks that property. A name that is wrong (or that stops being registered
    /// on some future Windows) would therefore cost a column, or an identity, with no error
    /// anywhere. This is the only place that failure becomes visible.
    #[test]
    fn every_property_name_we_write_resolves() {
        // `PSGetPropertyKeyFromName` needs an initialized apartment. In production the shim
        // only ever runs on a thread that is already inside a COM call, so this is the test's
        // own setup, but it is also the reason `pkey` can fail: on a thread without COM,
        // every property silently goes missing and the row comes out blank.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        // Iterating the const rather than a second copy of it: a property added to `fill`
        // and to that list is checked here without anybody remembering to add it twice. The
        // one worth naming is `System.ItemId`: it is the identity the folder compares items
        // by, and a name that stopped resolving would put the all-rows-selected bug straight
        // back, silently.
        assert!(WRITTEN_PROPERTIES.contains(&"System.ItemId"));
        for &name in WRITTEN_PROPERTIES {
            assert!(pkey(name).is_some(), "{name} did not resolve to a key");
        }
    }

    /// The same URL names the terms as well as the folder, which is the reading that does not
    /// go through the shell's private interface. Both shapes are as Explorer wrote them here.
    #[test]
    fn the_terms_can_be_read_out_of_a_search_url() {
        assert_eq!(
            term_from_search_url("search-ms:query=virtual insanity&crumb=location:C%3A%5CUsers"),
            Some("virtual insanity".to_string()),
            "the documented parameter"
        );
        assert_eq!(
            term_from_search_url(
                "search-ms:displayname=\u{691c}\u{7d22}%3A%20me\
                 &crumb=\u{3059}\u{3079}\u{3066}\u{306e}\u{30c6}\u{30ad}\u{30b9}\u{30c8}\u{ff1a}(virtual%20insanity)\
                 &crumb=location:C%3A%5CUsers%5Cme"
            ),
            Some("virtual insanity".to_string()),
            "a localised crumb label, read without ever looking at the label"
        );
        // Windows 11 (build 26200) drops the parentheses: the term sits straight after the
        // label's fullwidth colon. Exactly as captured from this machine's searchshim.log.
        assert_eq!(
            term_from_search_url(
                "search-ms:displayname=\u{691c}\u{7d22}\u{5834}\u{6240}%3A%20\u{30ed}\u{30fc}\u{30ab}\u{30eb}%20\u{30c7}\u{30a3}\u{30b9}\u{30af}%20(C%3A)\
                 &crumb=\u{3059}\u{3079}\u{3066}\u{306e}\u{30c6}\u{30ad}\u{30b9}\u{30c8}\u{ff1a}taikonaut\
                 &crumb=location:C%3A%5C"
            ),
            Some("taikonaut".to_string()),
            "no parentheses: term after the fullwidth colon"
        );
    }

    /// A crumb that is not typed text must not be mistaken for it, and a URL with no terms at
    /// all has to say so; the caller treats `None` as "let Windows answer".
    #[test]
    fn a_crumb_that_is_not_typed_text_is_passed_over() {
        assert_eq!(
            term_from_search_url("search-ms:crumb=kind:pics&crumb=location:C%3A%5CUsers"),
            None,
            "a filter, not typed text"
        );
        assert_eq!(
            term_from_search_url("search-ms:crumb=location:C%3A%5CUsers"),
            None,
            "the location is never the terms"
        );
        assert_eq!(
            term_from_search_url(r"C:\Users\me"),
            None,
            "not a URL at all"
        );
        assert_eq!(
            term_from_search_url("search-ms:query=%20&crumb=location:C%3A%5CUsers"),
            None,
            "a blank query is no query"
        );
    }

    /// A view sitting on a search folder has no filesystem path, so the scope has to be read
    /// out of the folder's own `search-ms:` parsing name. Getting this wrong is not a wrong
    /// answer but *no* answer: `query_for` returns nothing and every search silently falls
    /// back to Windows, which is exactly the regression this fallback exists to close.
    #[test]
    fn the_scope_can_be_read_out_of_a_search_url() {
        assert_eq!(
            scope_from_search_url("search-ms:query=notes&crumb=location:C%3A%5CUsers%5Cme&"),
            Some(r"C:\Users\me".to_string())
        );
        assert_eq!(
            scope_from_search_url(r"search-ms:query=x&crumb=location:C:\Program%20Files"),
            Some(r"C:\Program Files".to_string()),
            "an unescaped path with an escaped space"
        );
        assert_eq!(
            scope_from_search_url("search-ms:query=x&crumb=location:C%3A%5C"),
            Some("C:".to_string()),
            "a drive root loses its trailing separator, as seed_scope expects"
        );
    }

    /// A literal `%` next to a multi-byte character used to slice a `&str` at a byte index
    /// that is not a character boundary, which panics, and this parser runs inside the
    /// `extern "system"` hooks Explorer calls, where an unwind aborts explorer.exe rather
    /// than returning an error. `%` is a legal character in a Windows file name and the
    /// crumb carries un-escaped localized text, so both halves of that arrive together in
    /// any folder named like this one.
    #[test]
    fn a_percent_before_a_multibyte_character_decodes_instead_of_panicking() {
        assert_eq!(
            scope_from_search_url(r"search-ms:query=x&crumb=location:C:\Users\me\100%達成"),
            Some(r"C:\Users\me\100%達成".to_string()),
            "an escape that is not one must be left exactly as written"
        );
        assert_eq!(
            term_from_search_url("search-ms:query=50%割引&"),
            Some("50%割引".to_string())
        );
        // The same shape at the very end of the string, and one that *is* a valid escape
        // immediately before a multi-byte character.
        assert_eq!(percent_decode("done 100%"), "done 100%");
        assert_eq!(percent_decode("%あ"), "%あ");
        assert_eq!(percent_decode("%20あ"), " あ");
    }

    #[test]
    fn a_url_with_no_location_yields_no_scope() {
        assert_eq!(scope_from_search_url("search-ms:query=notes"), None);
        assert_eq!(scope_from_search_url("search-ms:crumb=kind:pics"), None);
        assert_eq!(
            scope_from_search_url(r"C:\Users\me"),
            None,
            "not a search URL"
        );
        assert_eq!(scope_from_search_url(""), None);
    }

    #[test]
    fn forward_fetch_hands_out_one_based_handles() {
        let rs = rowset(5);
        assert_eq!(fetch(&rs, 0, 3), (vec![1, 2, 3], HRESULT(0)));
        assert_eq!(fetch(&rs, 2, 2), (vec![3, 4], HRESULT(0)));
    }

    #[test]
    fn running_out_of_rows_is_a_short_read_not_an_error() {
        let rs = rowset(3);
        // Fewer rows than asked for: a success code that says the set ended.
        assert_eq!(fetch(&rs, 1, 10), (vec![2, 3], DB_S_ENDOFROWSET));
        assert_eq!(fetch(&rs, 99, 4), (vec![], DB_S_ENDOFROWSET));
    }

    #[test]
    fn backward_fetch_ends_at_the_named_row() {
        let rs = rowset(5);
        // The window ends at `start`, so the last handle is `start + 1`.
        assert_eq!(fetch(&rs, 4, -3), (vec![3, 4, 5], HRESULT(0)));
        assert_eq!(fetch(&rs, 1, -5), (vec![1, 2], DB_S_ENDOFROWSET));
    }

    /// Whatever it is asked for, `fetch` may only name rows that exist: every handle it
    /// writes is dereferenced by `GetRowFromHROW` as `hrow - 1` into `rows`.
    #[test]
    fn no_fetch_names_a_row_outside_the_set() {
        let rs = rowset(4);
        for start in [-3, 0, 3, 4, 99] {
            for crows in [-9, -1, 0, 1, 9] {
                let (handles, _) = fetch(&rs, start, crows);
                for h in handles {
                    assert!(
                        (1..=4).contains(&h),
                        "start={start} crows={crows} produced out-of-range handle {h}"
                    );
                }
            }
        }
    }
}
