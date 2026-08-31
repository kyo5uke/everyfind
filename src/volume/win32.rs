//! Real NTFS volume backend: USN/MFT access via `DeviceIoControl`.
//!
//! Read FSCTLs only: `FSCTL_QUERY_USN_JOURNAL`, `FSCTL_ENUM_USN_DATA` (build), and
//! `FSCTL_READ_USN_JOURNAL` (M2 tail). The sole write FSCTL is `FSCTL_CREATE_USN_JOURNAL`,
//! confined to [`Win32Volume::create_journal`] and reached only via explicit opt-in
//! (`ef-index --create-journal`) on a test volume. Behavior is pinned to what the probes
//! measured on real hardware: V2 records only, an 8-byte next-cursor
//! header on each ENUM/READ buffer, and FRNs masked to their low 48 bits (record number).

use std::collections::HashMap;
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandleEx, ReadFile, SetFilePointerEx,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use super::{JournalError, JournalInfo, RawRecord, UsnEvent, UsnVolume};

// Stable, documented Win32 constants, defined locally to keep the windows-sys feature
// surface minimal (matches the probe).
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// Open the reparse point itself rather than following it.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// `FILE_INFO_BY_HANDLE_CLASS::FileIdInfo`: VolumeSerialNumber@0 + 128-bit FileId@8.
const FILE_ID_INFO: i32 = 18;
/// `FILE_INFO_BY_HANDLE_CLASS::FileStandardInfo`: AllocationSize@0 + EndOfFile@8 + NumberOfLinks.
const FILE_STANDARD_INFO: i32 = 1;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00e7;
const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_INVALID_PARAMETER: u32 = 87;
/// The journal itself is gone, or was never activated. Both mean the cursor we hold refers to
/// a journal that no longer exists, which is a discontinuity exactly like a wrap, and neither
/// was mapped, so `fsutil usn deletejournal` surfaced as an opaque `Other`. `ef-index --watch`
/// then exited on the `?`, and the daemon logged and retried every poll while `phase` stayed
/// READY: the index quietly frozen, clients told it was current.
const ERROR_JOURNAL_DELETED: u32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;

/// An NTFS FRN is `sequence(16) << 48 | record#(48)`. Identity and parent references
/// are compared on the record number (measured; see probe P5).
const FRN_MASK: u64 = (1u64 << 48) - 1;

/// Byte offset (`FILE_BEGIN`) for `SetFilePointerEx`.
const FILE_BEGIN: u32 = 0;

/// NTFS geometry from `FSCTL_GET_NTFS_VOLUME_DATA`, used by the M5 size pass (`enum_sizes`) and
/// the capacity `size_hint`.
#[derive(Clone, Copy, Debug)]
struct VolData {
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    bytes_per_frs: u32,
    mft_valid_len: u64,
    mft_start_lcn: u64,
}

/// 1 MiB enumeration buffer: amortizes the DeviceIoControl round-trips.
const ENUM_BUF_BYTES: usize = 1 << 20;
/// 256 KiB journal-read buffer per `FSCTL_READ_USN_JOURNAL` round-trip.
const READ_BUF_BYTES: usize = 256 * 1024;

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Parse a volume argument (`"C"`, `"C:"`, or `"C:\"`) to its uppercase drive letter.
fn parse_drive(volume: &str) -> Result<char> {
    volume
        .trim_end_matches(['\\', ':'])
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .context("volume must start with a drive letter, e.g. C:")
}

/// Open `\\.\<drive>:` with `access`. Requires elevation (raw volume access needs admin).
fn open_handle(drive: char, access: u32) -> Result<*mut c_void> {
    let path = format!(r"\\.\{drive}:");
    let wide: Vec<u16> = OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a valid NUL-terminated wide string; other args are constants.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        if err == ERROR_ACCESS_DENIED {
            bail!("access denied opening {path}; run ef-index in an ELEVATED terminal");
        }
        bail!("CreateFileW({path}) failed (GetLastError = {err})");
    }
    Ok(handle)
}

/// A read-only handle to a single NTFS volume (`\\.\C:`).
pub struct Win32Volume {
    handle: *mut c_void,
    drive: char,
    /// Cached accepted `READ_USN_JOURNAL_DATA` input length (0 = not yet detected). The
    /// struct size is version-dependent (V0=40, V1=44 packed / 48 padded); detect once.
    read_len: AtomicU32,
    /// Optional cancellation predicate, polled between enumeration batches. When it returns
    /// true, [`enum_records`](Win32Volume::enum_records) aborts early. The service installs
    /// this so a STOP during the initial (~30 s) enumeration is honored promptly.
    cancel: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Optional progress callback, invoked between enumeration batches with the running record
    /// count. The daemon uses it to surface "index building (N so far)" in `ef status` while
    /// the initial enumeration runs.
    progress: Option<Box<dyn Fn(usize) + Send + Sync>>,
    /// The volume's geometry, read once. See [`volume_data`](Win32Volume::volume_data).
    vol_data: std::sync::OnceLock<Option<VolData>>,
}

impl Win32Volume {
    /// Open `\\.\<letter>:` for read-only FSCTLs. Accepts `"C"`, `"C:"`, or `"C:\"`.
    /// Requires an elevated process (opening the raw volume needs admin).
    pub fn open(volume: &str) -> Result<Self> {
        let drive = parse_drive(volume)?;
        let handle = open_handle(drive, GENERIC_READ)?;
        Ok(Self {
            handle,
            drive,
            read_len: AtomicU32::new(0),
            cancel: None,
            progress: None,
            vol_data: std::sync::OnceLock::new(),
        })
    }

    /// Install a predicate polled between enumeration batches; when it returns true,
    /// [`enum_records`](Win32Volume::enum_records) stops early with a "cancelled" error. Lets
    /// the service abort a long initial enumeration on a STOP request without waiting ~30 s.
    pub fn set_cancel(&mut self, predicate: impl Fn() -> bool + Send + Sync + 'static) {
        self.cancel = Some(Box::new(predicate));
    }

    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c())
    }

    /// Install a progress callback (running record count, between batches).
    pub fn set_progress(&mut self, f: impl Fn(usize) + Send + Sync + 'static) {
        self.progress = Some(Box::new(f));
    }

    /// Remove the progress callback (after the initial enumeration completes).
    pub fn clear_progress(&mut self) {
        self.progress = None;
    }

    /// Create (or resize) this volume's USN journal via `FSCTL_CREATE_USN_JOURNAL`.
    ///
    /// **This is the one sanctioned volume WRITE** (a transient `GENERIC_WRITE` handle is
    /// opened just for the FSCTL, then closed; the volume's own handle stays read-only). It
    /// must only be reached through explicit opt-in (`ef-index --create-journal`) on a test
    /// volume, never implicitly, never on a system volume.
    pub fn create_journal(&self, max_size: u64, allocation_delta: u64) -> Result<()> {
        let write = open_handle(self.drive, GENERIC_READ | GENERIC_WRITE)
            .context("opening volume for write (--create-journal)")?;
        // CREATE_USN_JOURNAL_DATA { MaximumSize: u64, AllocationDelta: u64 } (16 bytes).
        let mut input = [0u8; 16];
        input[0..8].copy_from_slice(&max_size.to_le_bytes());
        input[8..16].copy_from_slice(&allocation_delta.to_le_bytes());
        let mut returned = 0u32;
        // SAFETY: write FSCTL with a valid 16-byte input and no output buffer.
        let ok = unsafe {
            DeviceIoControl(
                write,
                FSCTL_CREATE_USN_JOURNAL,
                input.as_ptr().cast(),
                input.len() as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        let err = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        // SAFETY: `write` is a valid handle from open_handle and not used after this.
        unsafe { CloseHandle(write) };
        if ok == 0 {
            bail!("FSCTL_CREATE_USN_JOURNAL failed (GetLastError = {err})");
        }
        Ok(())
    }

    /// Drive letter this volume was opened as (e.g. `'C'`).
    pub fn drive(&self) -> char {
        self.drive
    }

    fn device_io(&self, code: u32, input: &[u8], output: &mut [u8], returned: &mut u32) -> i32 {
        // SAFETY: read-only FSCTL; buffers are valid for the sizes passed.
        unsafe {
            DeviceIoControl(
                self.handle,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                returned,
                ptr::null_mut(),
            )
        }
    }

    /// One `FSCTL_READ_USN_JOURNAL` (V2 pinned via Min=Max=2). Returns `(ok, bytes_returned)`;
    /// `out[0..8]` is the next-USN header. The accepted input length is version-dependent
    /// (V0=40, V1=44 packed / 48 padded), so detect it once and cache in `self.read_len`.
    fn read_once(&self, start_usn: u64, journal_id: u64, out: &mut [u8]) -> (i32, u32) {
        // READ_USN_JOURNAL_DATA_V1: StartUsn@0, ReasonMask@8, ReturnOnlyOnClose@12,
        // Timeout@16, BytesToWaitFor@24, UsnJournalID@32, MinMajor@40, MaxMajor@42.
        // Bytes 44..48 are padding (for the 48-byte attempt); a 40-byte length omits the
        // version words and drives the journal's default major version (V2 here).
        // ReturnOnlyOnClose(12)=0, Timeout(16)=0, BytesToWaitFor(24)=0 -> non-blocking.
        let mut input = [0u8; 48];
        input[0..8].copy_from_slice(&start_usn.to_le_bytes());
        input[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // all reasons
        input[32..40].copy_from_slice(&journal_id.to_le_bytes());
        input[40..42].copy_from_slice(&2u16.to_le_bytes());
        input[42..44].copy_from_slice(&2u16.to_le_bytes());

        let cached = self.read_len.load(Ordering::Relaxed);
        let mut last = (0i32, 0u32);
        for &len in &[44u32, 48, 40] {
            if cached != 0 && len != cached {
                continue;
            }
            let mut returned = 0u32;
            // SAFETY: read-only FSCTL; input/output buffers valid for the given sizes.
            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_READ_USN_JOURNAL,
                    input.as_ptr().cast(),
                    len,
                    out.as_mut_ptr().cast(),
                    out.len() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok != 0 {
                self.read_len.store(len, Ordering::Relaxed);
                return (ok, returned);
            }
            last = (ok, returned);
            if unsafe { GetLastError() } != ERROR_INVALID_PARAMETER {
                return last; // real error: caller inspects GetLastError
            }
        }
        last
    }

    /// Read `FSCTL_GET_NTFS_VOLUME_DATA` into the geometry the M5 size pass and `size_hint` need.
    /// The volume's geometry: sector, cluster and file-record sizes, and where `$MFT` starts.
    ///
    /// Read once per open handle, because none of it can change while the handle is open and
    /// asking is not free: this is a `DeviceIoControl`, and `alloc_clusters` calls it to learn
    /// one number, once per file. The live size refresh stats up to 4,096 files a poll, so an
    /// uncached read meant 4,096 extra round trips into the filesystem every second on a
    /// machine that was merely compiling something.
    fn volume_data(&self) -> Option<VolData> {
        *self.vol_data.get_or_init(|| self.read_volume_data())
    }

    fn read_volume_data(&self) -> Option<VolData> {
        // NTFS_VOLUME_DATA_BUFFER: BytesPerSector@40, BytesPerCluster@44, BytesPerFileRecordSegment@48,
        // MftValidDataLength@56, MftStartLcn@64.
        let mut out = [0u8; 128];
        let mut returned = 0u32;
        let ok = self.device_io(FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut out, &mut returned);
        if ok == 0 || (returned as usize) < 96 {
            return None;
        }
        Some(VolData {
            bytes_per_sector: rd_u32(&out, 40),
            bytes_per_cluster: rd_u32(&out, 44),
            bytes_per_frs: rd_u32(&out, 48),
            mft_valid_len: rd_u64(&out, 56),
            mft_start_lcn: rd_u64(&out, 64),
        })
    }

    /// The `$MFT`'s own extents, following an `$ATTRIBUTE_LIST` when it has one.
    ///
    /// A small `$MFT` keeps its whole run list in record 0 and this is one parse. A large or
    /// fragmented one outgrows the record, and NTFS moves part of the list into extension
    /// records named by an `$ATTRIBUTE_LIST`. This used to stop there and give up on sizes
    /// entirely, which sounds conservative until you notice when it happens: measured on a
    /// 929 GB volume in ordinary use, with a 6 GB `$MFT`, `ef du` reported **47 GiB against
    /// 827 GB actually used**. "Only on fragmented volumes" turns out to mean "on any machine
    /// that has been used", so the list is followed instead.
    ///
    /// Assembled in passes, because of a circularity: reading extension record N means knowing
    /// where record N lives, which means already having the extents that cover it. Record 0's
    /// own fragment covers the front of the `$MFT`, that reaches the next fragment, and so on.
    /// Each pass must place at least one more fragment or the loop stops: a run list that
    /// points outside what it has built is bad data, not a reason to spin.
    fn mft_extents(&self, rec0: &[u8], frs: u64, bpc: u64, bps: usize) -> Vec<(u64, u64)> {
        let mut extents: Vec<(u64, u64)> = Vec::new();
        let mut list_bytes: Vec<u8> = Vec::new();

        for attr in attributes(rec0) {
            match rd_u32(attr, 0) {
                0x20 => {
                    // Resident is the ordinary case. A non-resident `$ATTRIBUTE_LIST` addresses
                    // the volume directly, so its runs can be read without any of this.
                    if let Some(v) = resident_value(attr) {
                        list_bytes = v.to_vec();
                    } else if let Some(runs) = {
                        let a = attr;
                        (rd_u32(a, 0) == 0x20 && a.len() >= 64 && a[8] != 0)
                            .then(|| {
                                let off = rd_u16(a, 32) as usize;
                                (off < a.len()).then(|| &a[off..])
                            })
                            .flatten()
                    } {
                        let mut ext = Vec::new();
                        parse_mft_extents(runs, &mut ext);
                        for (lcn, clusters) in ext {
                            let mut buf = vec![0u8; (clusters * bpc) as usize];
                            if self.read_volume_at(lcn * bpc, &mut buf) {
                                list_bytes.extend_from_slice(&buf);
                            }
                        }
                    }
                }
                0x80 => {
                    if let Some(runs) = unnamed_data_runs(attr) {
                        parse_mft_extents(runs, &mut extents);
                    }
                }
                _ => {}
            }
        }

        tracing::info!(
            extents_in_record0 = extents.len(),
            attribute_list_bytes = list_bytes.len(),
            "$MFT record 0 parsed"
        );
        if list_bytes.is_empty() {
            return extents;
        }

        let fragments = attribute_list_data_fragments(&list_bytes);
        // Record 0 holds the first fragment; the rest live elsewhere and are read below.
        let mut pending: Vec<u64> = fragments
            .iter()
            .map(|f| f.record)
            .filter(|&r| r != 0)
            .collect();
        pending.dedup();

        let mut rec = vec![0u8; frs as usize];
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|&record| {
                let Some(at) = mft_offset_of(&extents, record, frs, bpc) else {
                    return true; // not reachable yet; a later pass may place it
                };
                if !self.read_volume_at(at, &mut rec) || &rec[0..4] != b"FILE" {
                    return false; // unreadable is not retryable
                }
                apply_fixup(&mut rec, bps);
                for attr in attributes(&rec) {
                    if let Some(runs) = unnamed_data_runs(attr) {
                        parse_mft_extents(runs, &mut extents);
                    }
                }
                false
            });
            if pending.len() == before {
                break; // no progress: the remaining fragments cannot be placed
            }
        }

        if !pending.is_empty() {
            tracing::warn!(
                unreachable = pending.len(),
                "some $MFT fragments could not be placed; du sizes cover only part of the volume"
            );
        }
        tracing::info!(
            fragments = fragments.len(),
            extents = extents.len(),
            clusters = extents.iter().map(|&(_, c)| c).sum::<u64>(),
            "$MFT run list assembled"
        );
        extents
    }

    /// Read `buf.len()` bytes from the raw volume at byte `offset` (sector-aligned) into `buf`.
    /// Used only by the M5 size pass to read `$MFT` clusters (read-only).
    fn read_volume_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let mut newpos = 0i64;
        // SAFETY: FILE_BEGIN seek to a sector-aligned offset on our read-only volume handle.
        if unsafe { SetFilePointerEx(self.handle, offset as i64, &mut newpos, FILE_BEGIN) } == 0 {
            return false;
        }
        let mut total = 0usize;
        while total < buf.len() {
            let mut got = 0u32;
            let want = (buf.len() - total) as u32;
            // SAFETY: valid handle; writing into buf[total..] which is valid for `want` bytes.
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf[total..].as_mut_ptr().cast(),
                    want,
                    &mut got,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || got == 0 {
                return total == buf.len();
            }
            total += got as usize;
        }
        true
    }
}

impl Drop for Win32Volume {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful CreateFileW and is not closed elsewhere.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Apply the NTFS multi-sector fixup (update sequence array) to a FILE record in place.
fn apply_fixup(rec: &mut [u8], bytes_per_sector: usize) {
    let usa_off = rd_u16(rec, 4) as usize;
    let usa_cnt = rd_u16(rec, 6) as usize; // count of u16s incl. the update sequence number
    if usa_cnt == 0 || usa_off + usa_cnt * 2 > rec.len() {
        return;
    }
    for i in 1..usa_cnt {
        let sector_end = i * bytes_per_sector - 2;
        let src = usa_off + i * 2;
        if sector_end + 2 <= rec.len() && src + 2 <= rec.len() {
            rec[sector_end] = rec[src];
            rec[sector_end + 1] = rec[src + 1];
        }
    }
}

/// Sum the ALLOCATED clusters of an NTFS data-run list (non-sparse runs only). Exact for
/// sparse/compressed files, where the nominal `AllocatedSize` header over-reports (validated on
/// the T: fixture, probe P19). `off_bytes == 0` marks a sparse hole (no clusters).
fn data_run_clusters(runs: &[u8]) -> u64 {
    let mut i = 0usize;
    let mut clusters = 0u64;
    while i < runs.len() {
        let header = runs[i];
        if header == 0 {
            break;
        }
        let len_bytes = (header & 0x0f) as usize;
        let off_bytes = (header >> 4) as usize;
        i += 1;
        if i + len_bytes + off_bytes > runs.len() {
            break;
        }
        let mut length: u64 = 0;
        for k in 0..len_bytes {
            length |= (runs[i + k] as u64) << (8 * k);
        }
        i += len_bytes;
        if off_bytes != 0 {
            clusters += length;
        }
        i += off_bytes;
    }
    clusters
}

/// Parse an NTFS data-run list into `(lcn, cluster_count)` extents (skips sparse holes). Used to
/// locate `$MFT`'s own clusters so the size pass can read the whole MFT sequentially.
fn parse_mft_extents(runs: &[u8], out: &mut Vec<(u64, u64)>) {
    let mut i = 0usize;
    let mut cur_lcn: i64 = 0;
    while i < runs.len() {
        let header = runs[i];
        if header == 0 {
            break;
        }
        let len_bytes = (header & 0x0f) as usize;
        let off_bytes = (header >> 4) as usize;
        i += 1;
        if i + len_bytes + off_bytes > runs.len() {
            break;
        }
        let mut length: u64 = 0;
        for k in 0..len_bytes {
            length |= (runs[i + k] as u64) << (8 * k);
        }
        i += len_bytes;
        if off_bytes == 0 {
            continue; // sparse (not expected for $MFT)
        }
        let mut delta: i64 = 0;
        for k in 0..off_bytes {
            delta |= (runs[i + k] as i64) << (8 * k);
        }
        let sign_bit = 1i64 << (8 * off_bytes - 1);
        if delta & sign_bit != 0 {
            delta -= 1i64 << (8 * off_bytes);
        }
        i += off_bytes;
        cur_lcn += delta;
        if cur_lcn >= 0 {
            out.push((cur_lcn as u64, length));
        }
    }
}

/// What one FILE record settles about a file's allocated size.
///
/// A file whose data runs outgrow its base record - a big one, or a fragmented one - keeps them
/// in *extension* records named by an `$ATTRIBUTE_LIST`, and the base is then left holding no
/// runs at all. Reading only the base makes such a file look like zero bytes, and reporting that
/// as a size is worse than reporting nothing: measured on this volume, a 20.5 GiB directory of
/// model files came out as `0.0 B` and the whole of `C:` as 596.9 GiB against 838.3 GiB really
/// used, while `ef status` said 99.99% of entries had a size. Telling the three cases apart is
/// what lets [`Win32Volume::enum_sizes`] add the pieces back together.
#[derive(Debug, PartialEq, Eq)]
enum RecordSize {
    /// A base record holding all of its own runs: this is the whole answer for that file.
    Whole(u64),
    /// A base record with an `$ATTRIBUTE_LIST`: only the part that stayed behind. The rest
    /// arrives as [`RecordSize::Extension`] from records elsewhere in the MFT.
    Partial(u64),
    /// An extension record. Its runs belong to the base record it names, which may be scanned
    /// before or after this one.
    Extension { base: u64, clusters: u64 },
    /// Not a live FILE record; it says nothing about anything.
    Absent,
}

/// Read one FILE record's contribution to a file's allocated size.
fn record_size(rec: &[u8]) -> RecordSize {
    // 40, not 24: `rd_u64(rec, 32)` below reads the base-record reference. The old bound was
    // wrong and only safe because `enum_sizes` refuses a volume whose record size is under 42.
    if rec.len() < 40 || &rec[0..4] != b"FILE" {
        return RecordSize::Absent;
    }
    if rd_u16(rec, 22) & 0x0001 == 0 {
        return RecordSize::Absent; // not in use
    }

    // Both facts from one walk. This runs once per MFT record - six million times on a normal
    // volume - so asking the same attributes twice, once for the runs and once for the list,
    // is a second pass over the whole MFT for a bool. `attributes` only yields slices of 8
    // bytes or more, so the type field is always there to read. Resident data lives inside the
    // record and occupies no clusters, which is why it contributes nothing here.
    let mut clusters = 0u64;
    let mut listed = false;
    for attr in attributes(rec) {
        match rd_u32(attr, 0) {
            0x20 => listed = true,
            0x80 => {
                if let Some(runs) = unnamed_data_runs(attr) {
                    clusters += data_run_clusters(runs);
                }
            }
            _ => {}
        }
    }

    let base = rd_u64(rec, 32) & FRN_MASK;
    if base != 0 {
        return RecordSize::Extension { base, clusters };
    }
    if listed {
        RecordSize::Partial(clusters)
    } else {
        RecordSize::Whole(clusters)
    }
}

/// Clamp a cluster count to the `u32` the index stores, flagging the overflow (16 TiB at the
/// usual 4 KiB cluster) so `ef du` can say its total is a lower bound.
fn clamp_clusters(c: u64) -> (u32, bool) {
    if c > u32::MAX as u64 {
        (u32::MAX, true)
    } else {
        (c as u32, false)
    }
}

/// The attributes of a FILE record, each as its whole slice.
///
/// One walker, because there were two (here and in the `$MFT` record-0 scan) and they had
/// already drifted: the same two conditions tested in the opposite order, one accumulating and
/// one breaking. Duplication of a parser over raw on-disk bytes is how a bounds bug gets fixed
/// in one copy and left in the other.
fn attributes(rec: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = if rec.len() >= 22 {
        rd_u16(rec, 20) as usize
    } else {
        rec.len()
    };
    std::iter::from_fn(move || {
        if pos + 8 > rec.len() || rd_u32(rec, pos) == 0xFFFF_FFFF {
            return None;
        }
        let alen = rd_u32(rec, pos + 4) as usize;
        if alen < 8 || pos + alen > rec.len() {
            return None;
        }
        let attr = &rec[pos..pos + alen];
        pos += alen;
        Some(attr)
    })
}

/// The value bytes of a resident attribute.
fn resident_value(attr: &[u8]) -> Option<&[u8]> {
    if attr.len() < 24 || attr[8] != 0 {
        return None; // non-resident
    }
    let len = rd_u32(attr, 16) as usize;
    let off = rd_u16(attr, 20) as usize;
    (off <= attr.len() && len <= attr.len() - off).then(|| &attr[off..off + len])
}

/// One `$ATTRIBUTE_LIST` entry that matters here: an unnamed `$DATA` fragment, the MFT record
/// that holds it, and the VCN it starts at.
struct AttrFragment {
    start_vcn: u64,
    record: u64,
}

/// Every unnamed-`$DATA` fragment named by an `$ATTRIBUTE_LIST`, in VCN order.
///
/// Entry layout: type(4) length(2) name_len(1) name_off(1) start_vcn(8) base_ref(8) id(2).
/// The base reference is an MFT *file* reference, so the record number is its low 48 bits.
fn attribute_list_data_fragments(list: &[u8]) -> Vec<AttrFragment> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 26 <= list.len() {
        let entry_len = rd_u16(list, i + 4) as usize;
        if entry_len < 26 || i + entry_len > list.len() {
            break;
        }
        // Unnamed $DATA only: a named stream on $MFT is not the run list we are assembling.
        if rd_u32(list, i) == 0x80 && list[i + 6] == 0 {
            out.push(AttrFragment {
                start_vcn: rd_u64(list, i + 8),
                record: rd_u64(list, i + 16) & FRN_MASK,
            });
        }
        i += entry_len;
    }
    out.sort_by_key(|f| f.start_vcn);
    out
}

/// Byte offset on the volume of MFT record `record`, given the extents known so far.
///
/// The MFT is addressed by record number, but the extents are (lcn, clusters) pairs, so this
/// walks them accumulating length until the record's byte offset falls inside one. `None` when
/// the extents do not reach that far, which is exactly the case while the run list is still
/// being assembled, and the reason assembling it takes more than one pass.
fn mft_offset_of(extents: &[(u64, u64)], record: u64, frs: u64, bpc: u64) -> Option<u64> {
    let want = record.checked_mul(frs)?;
    let mut seen = 0u64;
    for &(lcn, clusters) in extents {
        let bytes = clusters.checked_mul(bpc)?;
        if want < seen + bytes {
            return Some(lcn.checked_mul(bpc)? + (want - seen));
        }
        seen += bytes;
    }
    None
}

/// The data-run bytes of `attr`, if it is the unnamed non-resident `$DATA`.
///
/// Every offset is checked against the attribute's own length. The previous code established
/// only `alen >= 8`, then read the name-length byte at +9 and the run-list offset at +32, so
/// an attribute claiming type `0x80` with a length between 8 and 34, at the tail of a record,
/// was an index-out-of-bounds panic. Real NTFS does not emit that; a record whose fixup was
/// mis-applied can, and this parser is fed raw disk bytes from a path documented as fail-soft.
fn unnamed_data_runs(attr: &[u8]) -> Option<&[u8]> {
    // 64 is the NTFS minimum for a non-resident header, and covers every field read below.
    if attr.len() < 64 || rd_u32(attr, 0) != 0x80 {
        return None;
    }
    let non_resident = attr[8] != 0;
    let named = attr[9] != 0;
    if !non_resident || named {
        return None;
    }
    let runs_off = rd_u16(attr, 32) as usize;
    if runs_off >= attr.len() {
        return None;
    }
    Some(&attr[runs_off..])
}

/// A path in the extended form, so length is not a reason to fail.
///
/// `CreateFileW` refuses a path at or over `MAX_PATH` unless it is prefixed, and an index of a
/// whole volume is full of paths that are: `node_modules`, deep build output, decompiler dumps.
/// Those files were systematically the ones that never received a live size update, which is
/// invisible because the size pass is fail-soft. Already-prefixed and UNC paths are left alone.
fn extended_path(path: &str) -> Vec<u16> {
    let prefixed = if path.starts_with(r"\\") || path.len() < 260 {
        path.to_string()
    } else {
        format!(r"\\?\{path}")
    };
    OsStr::new(&prefixed)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The masked file reference number behind an open handle, for verifying that the path still
/// names the file we asked about.
fn handle_frn(h: *mut c_void) -> Option<u64> {
    let mut info = [0u8; 24]; // FILE_ID_INFO: VolumeSerialNumber@0, FileId@8 (16 bytes)
                              // SAFETY: valid handle; buffer is the documented size for FileIdInfo.
    let ok = unsafe {
        GetFileInformationByHandleEx(h, FILE_ID_INFO, info.as_mut_ptr().cast(), info.len() as u32)
    };
    // The low 64 bits of the 128-bit id are the NTFS FRN; mask to the record number, which is
    // what the index keys on.
    (ok != 0).then(|| rd_u64(&info, 8) & FRN_MASK)
}

impl UsnVolume for Win32Volume {
    fn journal_info(&self) -> Result<JournalInfo> {
        let mut out = [0u8; 80]; // USN_JOURNAL_DATA_V2 is 80 bytes (probe P1)
        let mut returned = 0u32;
        let ok = self.device_io(FSCTL_QUERY_USN_JOURNAL, &[], &mut out, &mut returned);
        if ok == 0 {
            let err = unsafe { GetLastError() };
            bail!("FSCTL_QUERY_USN_JOURNAL failed (GetLastError = {err})");
        }
        if (returned as usize) < 24 {
            bail!("QUERY_USN_JOURNAL returned only {returned} bytes");
        }
        // The first three DWORDLONGs are identical across V0/V1/V2.
        Ok(JournalInfo {
            journal_id: rd_u64(&out, 0),
            first_usn: rd_u64(&out, 8),
            next_usn: rd_u64(&out, 16),
        })
    }

    fn size_hint(&self) -> Option<u64> {
        // The MFT record-segment count is a tight upper bound on entries (measured P13: 1.045x on
        // real C:): free/unused segments are enumerated as nothing.
        let vd = self.volume_data()?;
        (vd.bytes_per_frs > 0).then(|| vd.mft_valid_len / vd.bytes_per_frs as u64)
    }

    fn cluster_bytes(&self) -> u32 {
        self.volume_data().map_or(4096, |v| v.bytes_per_cluster)
    }

    fn alloc_clusters(&self, frn: u64, path: &str) -> Option<(u32, bool)> {
        // Stat the file BY PATH (the masked FRN is not a valid OpenFileById reference; probe P5).
        // AllocationSize from the documented API is correct for sparse/compressed (validated on the
        // T: fixture, unlike the raw MFT field). A fresh handle by path; the volume's own handle is
        // unrelated. All-`None` on any failure => fail-soft (the size stays until the next enum).
        let cb = self.volume_data()?.bytes_per_cluster as u64;
        if cb == 0 {
            return None;
        }
        let wide = extended_path(path);
        // SAFETY: valid NUL-terminated wide path; read-only open, backup semantics.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                // Describe the link, never what it points at. Following one contradicts
                // `enum_sizes`, which reads the reparse point's own record and so reports its
                // (tiny) allocation, and a junction aimed at a dead network share made this
                // call wait out the SMB timeout, with the index write lock held.
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut si = [0u8; 24]; // FILE_STANDARD_INFO: AllocationSize@0, EndOfFile@8, ...
                                // SAFETY: valid handle; buffer >= FILE_STANDARD_INFO; class FileStandardInfo.
        let ok = unsafe {
            GetFileInformationByHandleEx(h, FILE_STANDARD_INFO, si.as_mut_ptr().cast(), 24)
        };
        // The path came from the index and the file may have been replaced since, by a
        // different file with the same name, which is a routine thing for a build tree to do
        // between one journal poll and the next. Sizing `frn` from whatever now answers to that
        // name would attach a real number to the wrong entry, so the identity is checked.
        let same = ok != 0 && handle_frn(h) == Some(frn);
        // SAFETY: valid handle from CreateFileW.
        unsafe { CloseHandle(h) };
        if !same {
            return None;
        }
        let clusters = rd_u64(&si, 0) / cb; // AllocationSize is cluster-aligned
        Some(if clusters > u32::MAX as u64 {
            (u32::MAX, true)
        } else {
            (clusters as u32, false)
        })
    }

    fn enum_sizes(&self, sink: &mut dyn FnMut(u64, u32, bool)) -> Result<()> {
        // M5 size pass = the validated probe-A method (data-run cluster counting + fixup). Read
        // $MFT via its own $DATA runs, then per FILE record yield (frn = MFT slot = masked FRN,
        // allocated_clusters, truncated). Every early return is Ok(()) so a size-read failure is
        // FAIL-SOFT: the index is already built; du just shows 0 for unsized entries.
        let vd = match self.volume_data() {
            Some(v)
                if v.bytes_per_cluster > 0 && v.bytes_per_frs >= 42 && v.bytes_per_sector > 0 =>
            {
                v
            }
            other => {
                // Fail-soft, but never silent. Every exit from this pass costs `ef du` its
                // numbers, and a `du` that reports 5% of a volume with no explanation is worse
                // than one that refuses: the figure looks authoritative and is not. Each return
                // below says which of them fired, so `sizes (du): 0 / N` has an answer.
                tracing::warn!(
                    geometry = ?other,
                    "the volume geometry is unreadable; du sizes unavailable"
                );
                return Ok(());
            }
        };
        let bpc = vd.bytes_per_cluster as u64;
        let frs = vd.bytes_per_frs as usize;
        let bps = vd.bytes_per_sector as usize;

        // 1) Read $MFT record #0 and parse its own non-resident $DATA runs = the MFT's extents.
        let mut rec0 = vec![0u8; frs];
        if !self.read_volume_at(vd.mft_start_lcn * bpc, &mut rec0) || &rec0[0..4] != b"FILE" {
            tracing::warn!(
                at = vd.mft_start_lcn * bpc,
                "could not read $MFT record 0; du sizes unavailable"
            );
            return Ok(());
        }
        apply_fixup(&mut rec0, bps);
        let extents = self.mft_extents(&rec0, frs as u64, bpc, bps);
        if extents.is_empty() {
            tracing::warn!("$MFT record 0 carries no unnamed $DATA runs; du sizes unavailable");
            return Ok(());
        }

        // 2) Read the extents sequentially, parse every FILE record, stop at MftValidDataLength.
        let max_records = vd.mft_valid_len / frs as u64;
        let mut sized = 0u64;
        let mut records_seen = 0u64;
        // Files whose `$DATA` outgrew their base record, held back until the pass ends: the runs
        // found in the base so far, keyed by file reference. They cannot be emitted on sight
        // because the extension records carrying the rest sit anywhere in the MFT, before or
        // after. Only fragmented files are in here, a few hundred on a normal volume.
        let mut partial: HashMap<u64, u64> = HashMap::new();
        // Clusters found in extension records, keyed by the base record each one names.
        let mut extra: HashMap<u64, u64> = HashMap::new();
        // Set by the two fail-soft exits below, and *only* by them: reaching `max_records` is
        // how this pass ends normally.
        let mut stopped_early = false;
        let chunk_bytes = 8 * 1024 * 1024u64; // 8 MiB sequential reads
        let mut chunk = vec![0u8; chunk_bytes as usize];
        let mut slot: u64 = 0; // MFT slot index == (masked) file reference number
        'outer: for (lcn, clusters) in extents {
            let mut remaining = clusters * bpc;
            let mut base = lcn * bpc;
            while remaining > 0 {
                let this = remaining.min(chunk_bytes);
                let s = &mut chunk[..this as usize];
                if !self.read_volume_at(base, s) {
                    // Stop the whole pass, not just this extent. `slot` *is* the file reference
                    // number handed to the sink, and skipping bytes without advancing it
                    // attaches every later size to a record `remaining / frs` slots too low,
                    // `ef du` would then report real numbers against the wrong files, and say
                    // nothing. Fail-soft here means fewer sizes, never wrong ones.
                    tracing::warn!(
                        at = base,
                        "MFT read failed during the size pass; sizes stop here"
                    );
                    stopped_early = true;
                    break 'outer;
                }
                // A chunk that is not a whole number of records would leave a tail unread while
                // `base` moves past it, resuming mid-record: the same class of desync as the
                // failed read above. It cannot happen with any real geometry (the chunk size
                // and every extent length are cluster multiples, and a cluster is a multiple of
                // the record size), but nothing here enforced it, so say so out loud.
                if !(this as usize).is_multiple_of(frs) {
                    // Not a debug assertion: in release it would leave a tail unread while
                    // `base` moved past it, resuming mid-record and desyncing `slot` from the
                    // MFT offset: real sizes attached to the wrong files, which is exactly
                    // what the failed-read return above exists to prevent. One branch per
                    // 8 MiB is not a cost worth trading for that.
                    tracing::warn!(
                        chunk = this,
                        record = frs,
                        "MFT chunk is not a whole number of records; sizes stop here"
                    );
                    stopped_early = true;
                    break 'outer;
                }
                let mut o = 0usize;
                while o + frs <= s.len() {
                    if slot >= max_records {
                        break 'outer;
                    }
                    let frn = slot;
                    slot += 1;
                    records_seen += 1;
                    let rec = &mut s[o..o + frs];
                    if &rec[0..4] == b"FILE" {
                        apply_fixup(rec, bps);
                        match record_size(rec) {
                            RecordSize::Whole(c) => {
                                let (clusters_u32, truncated) = clamp_clusters(c);
                                sink(frn, clusters_u32, truncated);
                                sized += 1;
                            }
                            RecordSize::Partial(c) => {
                                partial.insert(frn, c);
                            }
                            RecordSize::Extension { base, clusters } => {
                                if clusters > 0 {
                                    *extra.entry(base).or_default() += clusters;
                                }
                            }
                            RecordSize::Absent => {}
                        }
                    }
                    o += frs;
                }
                base += this;
                remaining -= this;
            }
        }
        // The held-back files, now that every extension record has been seen. A base that named
        // an `$ATTRIBUTE_LIST` but gathered no runs anywhere is a real 0 (its data is resident,
        // or the file is empty), so it is emitted rather than left out.
        let held = partial.len();
        if stopped_early {
            // Emitting these now would mean adding up extension records the pass never reached
            // and presenting the shortfall as a size. Leaving them unsized keeps them in the
            // `sizes (du)` shortfall, where `ef du` already knows to warn.
            tracing::warn!(
                held,
                "the pass stopped early; files whose $DATA lives in extension records are left \
                 unsized rather than reported short"
            );
        } else {
            for (frn, own) in partial {
                let (clusters_u32, truncated) =
                    clamp_clusters(own + extra.remove(&frn).unwrap_or(0));
                sink(frn, clusters_u32, truncated);
                sized += 1;
            }
        }
        // The pass is fail-soft at six points and each one leaves a different shortfall. One
        // line at the end says which shape actually happened: no records walked at all, records
        // walked but none sized, or a normal run that simply stopped early.
        tracing::info!(
            records_seen,
            sized,
            max_records,
            held,
            "$MFT size pass finished"
        );
        Ok(())
    }

    fn enum_records(&mut self, sink: &mut dyn FnMut(RawRecord<'_>)) -> Result<()> {
        let mut buf = vec![0u8; ENUM_BUF_BYTES];
        let mut name_u16: Vec<u16> = Vec::with_capacity(256);
        let mut start_frn: u64 = 0;
        let mut count: usize = 0;

        loop {
            // Honor a cancellation request between batches (a STOP during the initial enum).
            if self.cancelled() {
                bail!("enumeration cancelled");
            }
            // Report progress once per batch (cheap; the enum is many records per FSCTL).
            if let Some(p) = &self.progress {
                p(count);
            }

            // MFT_ENUM_DATA_V1 { StartFileReferenceNumber, LowUsn, HighUsn,
            //                    MinMajorVersion=2, MaxMajorVersion=2 } (probe P2/P3).
            let mut input = [0u8; 28];
            input[0..8].copy_from_slice(&start_frn.to_le_bytes());
            input[8..16].copy_from_slice(&0i64.to_le_bytes());
            input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
            input[24..26].copy_from_slice(&2u16.to_le_bytes());
            input[26..28].copy_from_slice(&2u16.to_le_bytes());

            let mut returned = 0u32;
            let ok = self.device_io(FSCTL_ENUM_USN_DATA, &input, &mut buf, &mut returned);
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF {
                    break;
                }
                bail!("FSCTL_ENUM_USN_DATA failed (GetLastError = {err})");
            }
            let end = returned as usize;
            if end <= 8 {
                break; // only the 8-byte next-FRN header, no records
            }
            start_frn = rd_u64(&buf, 0);

            let mut pos = 8usize;
            while pos + 60 <= end {
                let rec = &buf[pos..end];
                let record_length = rd_u32(rec, 0) as usize;
                if record_length < 60 || pos + record_length > end {
                    break;
                }
                // V2 fixed layout (V2 is pinned via MaxMajorVersion=2, probe P3):
                //   FRN@8, ParentFRN@16, FileAttributes@52, NameLen@56, NameOff@58.
                let frn = rd_u64(rec, 8) & FRN_MASK;
                let parent_frn = rd_u64(rec, 16) & FRN_MASK;
                let attributes = rd_u32(rec, 52);
                let name_len = rd_u16(rec, 56) as usize;
                let name_off = rd_u16(rec, 58) as usize;

                if name_off + name_len <= record_length {
                    name_u16.clear();
                    for c in rec[name_off..name_off + name_len].as_chunks::<2>().0.iter() {
                        name_u16.push(u16::from_le_bytes(*c));
                    }
                    sink(RawRecord {
                        frn,
                        parent_frn,
                        name_utf16: &name_u16,
                        attributes,
                    });
                    count += 1;
                }
                pos += record_length;
            }
        }
        Ok(())
    }

    fn read_journal(
        &self,
        start_usn: u64,
        journal_id: u64,
        sink: &mut dyn FnMut(UsnEvent<'_>),
    ) -> Result<u64, JournalError> {
        let mut buf = vec![0u8; READ_BUF_BYTES];
        let mut name_u16: Vec<u16> = Vec::with_capacity(256);
        let mut cursor = start_usn;

        loop {
            let (ok, returned) = self.read_once(cursor, journal_id, &mut buf);
            if ok == 0 {
                let err = unsafe { GetLastError() };
                return match err {
                    ERROR_JOURNAL_ENTRY_DELETED => Err(JournalError::EntryDeleted),
                    // A journal that is gone or inactive is the same kind of discontinuity as
                    // one that wrapped: the cursor means nothing any more, and the answer is to
                    // re-enumerate rather than to keep asking.
                    ERROR_JOURNAL_DELETED | ERROR_JOURNAL_NOT_ACTIVE => {
                        Err(JournalError::EntryDeleted)
                    }
                    _ => Err(JournalError::Other(anyhow!(
                        "FSCTL_READ_USN_JOURNAL failed (GetLastError = {err})"
                    ))),
                };
            }
            let end = returned as usize;
            if end < 8 {
                break; // no next-USN header: nothing usable
            }
            let next = rd_u64(&buf, 0);

            // V2 fixed layout: FRN@8, ParentFRN@16, Usn@24, Reason@40, FileAttributes@52,
            // NameLen@56, NameOff@58 (pinned via Min=Max=2, probe P3/P10).
            let mut pos = 8usize;
            while pos + 60 <= end {
                let rec = &buf[pos..end];
                let record_length = rd_u32(rec, 0) as usize;
                if record_length < 60 || pos + record_length > end {
                    break;
                }
                let frn = rd_u64(rec, 8) & FRN_MASK;
                let parent_frn = rd_u64(rec, 16) & FRN_MASK;
                let usn = rd_u64(rec, 24);
                let reason = rd_u32(rec, 40);
                let attributes = rd_u32(rec, 52);
                let name_len = rd_u16(rec, 56) as usize;
                let name_off = rd_u16(rec, 58) as usize;

                if name_off + name_len <= record_length {
                    name_u16.clear();
                    for c in rec[name_off..name_off + name_len].as_chunks::<2>().0.iter() {
                        name_u16.push(u16::from_le_bytes(*c));
                    }
                    sink(UsnEvent {
                        frn,
                        parent_frn,
                        usn,
                        reason,
                        name_utf16: &name_u16,
                        attributes,
                    });
                }
                pos += record_length;
            }

            if next <= cursor {
                cursor = next; // caught up (empty read returns the current tail) / no progress
                break;
            }
            cursor = next;
        }
        Ok(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FILE record header with `first attribute offset` at 20 and the in-use bit set, then
    /// whatever attribute bytes the caller wants. Enough to drive the walkers, and nothing more.
    fn record(attrs: &[u8]) -> Vec<u8> {
        let mut rec = vec![0u8; 56];
        rec[0..4].copy_from_slice(b"FILE");
        rec[20..22].copy_from_slice(&56u16.to_le_bytes()); // attributes start here
        rec[22..24].copy_from_slice(&1u16.to_le_bytes()); // in use
        rec.extend_from_slice(attrs);
        rec
    }

    /// The shape that used to panic: an attribute claiming `$DATA` with a length short enough
    /// that the name-length byte at +9 and the run-list offset at +32 fall outside the record.
    /// The size pass is documented as fail-soft; an index-out-of-bounds is not fail-soft.
    #[test]
    fn a_truncated_data_attribute_is_skipped_rather_than_read_past_the_end() {
        let mut attr = vec![0u8; 12];
        attr[0..4].copy_from_slice(&0x80u32.to_le_bytes()); // $DATA
        attr[4..8].copy_from_slice(&12u32.to_le_bytes()); // length 12; header is 64
        let rec = record(&attr);

        assert_eq!(record_size(&rec), RecordSize::Whole(0));
        assert_eq!(super::attributes(&rec).count(), 1);
        assert!(super::unnamed_data_runs(&attr).is_none());
    }

    /// A non-resident `$DATA` attribute of `clusters` clusters, in one run.
    fn data_attr(clusters: u8) -> Vec<u8> {
        let mut attr = vec![0u8; 72];
        attr[0..4].copy_from_slice(&0x80u32.to_le_bytes()); // $DATA
        attr[4..8].copy_from_slice(&72u32.to_le_bytes());
        attr[8] = 1; // non-resident
        attr[32..34].copy_from_slice(&64u16.to_le_bytes()); // run list at +64
        attr[64] = 0x11; // one length byte, one offset byte
        attr[65] = clusters;
        attr[66] = 0x05; // some LCN; anything non-zero is a real (non-sparse) run
        attr
    }

    /// An `$ATTRIBUTE_LIST` attribute, which is all a base record keeps once its `$DATA` has
    /// been moved out to extension records.
    fn attribute_list_attr() -> Vec<u8> {
        let mut a = vec![0u8; 8];
        a[0..4].copy_from_slice(&0x20u32.to_le_bytes());
        a[4..8].copy_from_slice(&8u32.to_le_bytes());
        a
    }

    /// The bug this type exists to prevent. A big or fragmented file keeps its data runs in
    /// extension records and leaves an `$ATTRIBUTE_LIST` behind, so the base record holds no
    /// runs at all. Reading the base alone finds nothing, and calling that nothing a *size* is
    /// how `ef du` reported `0.0 B` for 20.5 GiB of files, and 596.9 GiB for a volume with
    /// 838.3 GiB on it, while claiming 99.99% of entries had been sized.
    #[test]
    fn a_base_record_whose_data_moved_out_is_partial_not_a_confident_zero() {
        let base = record(&attribute_list_attr());
        assert_eq!(record_size(&base), RecordSize::Partial(0));

        // Some files keep part of their runs in the base and move only the overflow.
        let mut both = attribute_list_attr();
        both.extend_from_slice(&data_attr(16));
        assert_eq!(record_size(&record(&both)), RecordSize::Partial(16));
    }

    /// An extension record names the base it belongs to. Skipping it, on the grounds that "its
    /// base carries the size", threw away exactly the clusters the base was missing.
    #[test]
    fn an_extension_record_reports_its_clusters_against_its_base() {
        let mut rec = record(&data_attr(32));
        // A file reference: sequence number in the top 16 bits, record number in the low 48.
        rec[32..40].copy_from_slice(&0x0001_0000_0000_002Au64.to_le_bytes());

        assert_eq!(
            record_size(&rec),
            RecordSize::Extension {
                base: 42,
                clusters: 32
            }
        );
    }

    /// The ordinary file: one base record, its own runs, no list. Unchanged by all of the above.
    #[test]
    fn a_plain_base_record_still_answers_with_its_own_runs() {
        assert_eq!(record_size(&record(&data_attr(8))), RecordSize::Whole(8));
    }

    /// A named `$DATA` (an alternate stream) is not the file's own data, and a resident one has
    /// no clusters at all. Both were already handled; they are pinned here because the check
    /// moved into a shared function that two callers now depend on.
    #[test]
    fn only_the_unnamed_non_resident_data_attribute_yields_runs() {
        let mut attr = vec![0u8; 64];
        attr[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        attr[4..8].copy_from_slice(&64u32.to_le_bytes());
        attr[32..34].copy_from_slice(&64u16.to_le_bytes()); // run list at the very end

        let mut resident = attr.clone();
        resident[8] = 0;
        assert!(super::unnamed_data_runs(&resident).is_none(), "resident");

        let mut named = attr.clone();
        named[8] = 1;
        named[9] = 4; // a four-character stream name
        assert!(super::unnamed_data_runs(&named).is_none(), "named stream");

        let mut good = attr.clone();
        good[8] = 1;
        good[32..34].copy_from_slice(&40u16.to_le_bytes());
        assert_eq!(super::unnamed_data_runs(&good).map(|r| r.len()), Some(24));
    }

    /// An attribute list terminator ends the walk, and a length that would run past the record
    /// ends it too; neither is allowed to loop or to read out of bounds.
    #[test]
    fn the_attribute_walk_stops_at_the_terminator_and_at_a_bad_length() {
        let mut attrs = vec![0u8; 8];
        attrs[0..4].copy_from_slice(&0x10u32.to_le_bytes()); // $STANDARD_INFORMATION
        attrs[4..8].copy_from_slice(&8u32.to_le_bytes());
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // terminator
        assert_eq!(super::attributes(&record(&attrs)).count(), 1);

        let mut runaway = vec![0u8; 8];
        runaway[0..4].copy_from_slice(&0x10u32.to_le_bytes());
        runaway[4..8].copy_from_slice(&9999u32.to_le_bytes());
        assert_eq!(super::attributes(&record(&runaway)).count(), 0);
    }

    /// A path long enough to need the extended form gets it; anything already prefixed, or UNC,
    /// is left alone. `CreateFileW` refuses an unprefixed path at `MAX_PATH`, and a whole-volume
    /// index is full of them: `node_modules`, build output, decompiler dumps. Those files were
    /// systematically the ones that never received a live size update, invisibly, because the
    /// size path is fail-soft.
    #[test]
    fn only_a_long_plain_path_is_given_the_extended_prefix() {
        fn text(v: &[u16]) -> String {
            String::from_utf16_lossy(&v[..v.len() - 1])
        }
        let short = r"C:\Users\me\notes.txt";
        assert_eq!(text(&extended_path(short)), short, "short paths untouched");

        let long = format!(r"C:\{}\deep.txt", "x".repeat(300));
        assert_eq!(
            text(&extended_path(&long)),
            format!(r"\\?\{long}"),
            "a path past MAX_PATH is prefixed"
        );

        let unc = format!(r"\\nas\share\{}", "y".repeat(300));
        assert_eq!(text(&extended_path(&unc)), unc, "UNC is left alone");

        let already = format!(r"\\?\C:\{}", "z".repeat(300));
        assert_eq!(
            text(&extended_path(&already)),
            already,
            "not prefixed twice"
        );
    }
}

#[cfg(test)]
mod attribute_list_tests {
    use super::*;

    /// One `$ATTRIBUTE_LIST` entry: type, name length, starting VCN, and the record holding it.
    fn entry(attr_type: u32, name_len: u8, start_vcn: u64, record: u64) -> Vec<u8> {
        let mut e = vec![0u8; 26];
        e[0..4].copy_from_slice(&attr_type.to_le_bytes());
        e[4..6].copy_from_slice(&26u16.to_le_bytes()); // entry length
        e[6] = name_len;
        e[8..16].copy_from_slice(&start_vcn.to_le_bytes());
        e[16..24].copy_from_slice(&record.to_le_bytes());
        e
    }

    /// Only unnamed `$DATA` fragments count, and they come back in VCN order.
    ///
    /// A `$MFT` big enough to need an `$ATTRIBUTE_LIST` also lists `$STANDARD_INFORMATION`,
    /// `$FILE_NAME`, `$BITMAP` and often a named stream. Taking any of those as a run-list
    /// fragment would send the assembler to a record that holds no runs at all.
    #[test]
    fn only_unnamed_data_fragments_are_taken_and_they_are_sorted() {
        let mut list = Vec::new();
        list.extend(entry(0x10, 0, 0, 0)); // $STANDARD_INFORMATION
        list.extend(entry(0x80, 0, 5_000, 42)); // the second half of $DATA
        list.extend(entry(0xB0, 0, 0, 7)); // $BITMAP
        list.extend(entry(0x80, 4, 0, 99)); // a *named* $DATA stream, not ours
        list.extend(entry(0x80, 0, 0, 0)); // the first half, in record 0

        let frags = attribute_list_data_fragments(&list);
        let got: Vec<(u64, u64)> = frags.iter().map(|f| (f.start_vcn, f.record)).collect();
        assert_eq!(got, vec![(0, 0), (5_000, 42)]);
    }

    /// A truncated list stops rather than reading past its end, and a zero-length entry cannot
    /// spin the walker. Both shapes come from a mis-applied fixup, which this parser is fed.
    #[test]
    fn a_malformed_list_terminates() {
        let mut short = entry(0x80, 0, 0, 3);
        short.truncate(20);
        assert!(attribute_list_data_fragments(&short).is_empty());

        let mut zero_len = entry(0x80, 0, 0, 3);
        zero_len[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert!(attribute_list_data_fragments(&zero_len).is_empty());
    }

    /// A record number becomes a byte offset by walking the extents, because the `$MFT` is not
    /// contiguous; that is the whole reason this code exists.
    #[test]
    fn a_record_is_located_across_extents() {
        // Two extents: 2 clusters at LCN 100, then 3 clusters at LCN 500.
        // 4096-byte clusters, 1024-byte records => 4 records per cluster.
        let extents = vec![(100u64, 2u64), (500u64, 3u64)];
        let (frs, bpc) = (1024u64, 4096u64);

        // Record 0 sits at the start of the first extent.
        assert_eq!(mft_offset_of(&extents, 0, frs, bpc), Some(100 * 4096));
        // Record 7 is the last of the first extent (8 records = 2 clusters).
        assert_eq!(
            mft_offset_of(&extents, 7, frs, bpc),
            Some(100 * 4096 + 7 * 1024)
        );
        // Record 8 is the first of the *second* extent: the jump this function exists for.
        assert_eq!(mft_offset_of(&extents, 8, frs, bpc), Some(500 * 4096));
        assert_eq!(
            mft_offset_of(&extents, 9, frs, bpc),
            Some(500 * 4096 + 1024)
        );
        // Past the end: not placeable yet, which is a retry rather than an error.
        assert_eq!(mft_offset_of(&extents, 20, frs, bpc), None);
    }

    /// A resident attribute's value is its own bytes; a non-resident one has none here.
    #[test]
    fn resident_values_are_bounded_by_the_attribute() {
        let mut attr = vec![0u8; 40];
        attr[0..4].copy_from_slice(&0x20u32.to_le_bytes());
        attr[4..8].copy_from_slice(&40u32.to_le_bytes());
        attr[8] = 0; // resident
        attr[16..20].copy_from_slice(&8u32.to_le_bytes()); // value length
        attr[20..22].copy_from_slice(&24u16.to_le_bytes()); // value offset
        attr[24..32].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(resident_value(&attr), Some(&[1u8, 2, 3, 4, 5, 6, 7, 8][..]));

        // A length that runs past the attribute is refused, not read.
        attr[16..20].copy_from_slice(&999u32.to_le_bytes());
        assert_eq!(resident_value(&attr), None);

        // Non-resident carries no inline value.
        attr[8] = 1;
        assert_eq!(resident_value(&attr), None);
    }
}
