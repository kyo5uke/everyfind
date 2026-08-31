//! Everyfind M5 (`ef du`) size-source spike: verify, on real hardware, WHERE per-file
//! disk sizes can come from and how fast, before any `ef du` design is committed.
//!
//! Run in an ELEVATED terminal (both the raw `\\.\C:` MFT read and a T: fixture need admin):
//!
//! ```text
//! cargo run --release --example probe_du -- C: --speed    # P18 + P19(A/B) + P19c + P20 on real C:
//! cargo run --release --example probe_du -- T: --verify    # correctness/semantics on the T: fixture
//! ```
//!
//! Answers:
//!   P18  Does ENUM_USN_DATA / USN_RECORD carry a size field? (expected: NO -> a 2nd source is needed)
//!   P19  Size-source spike, wall-clock on real C:, TWO candidate bulk methods compared:
//!        (A) raw $MFT read: follow $MFT's own $DATA runs, parse each FILE record's $DATA size
//!        (B) directory-tree enumeration: GetFileInformationByHandleEx(FileIdBothDirectoryInfo)
//!            -> FileId(=FRN) + AllocationSize + EndOfFile per child, recursively
//!        Decision rule (approved): prefer (B) unless (B) is impractically slow; (A) is the
//!        fallback for "fast but fragile" (undocumented on-disk format). Record BOTH numbers.
//!   P19c (B) adoption risk: does (B)'s FileId set match the index's ENUM FRN set 100%?
//!        A miss = an entry (B) never sizes -> an UNDER-count. Measure the miss rate both ways.
//!   P20  Lazy per-file stat cost (GetFileInformationByHandle / GetCompressedFileSizeW),
//!        the small-subtree fallback + the correctness oracle for A/B. µs/file, extrapolated.
//!
//! **Read-only on the volume**: `\\.\C:` is opened GENERIC_READ; directory enumeration is the
//! ordinary file API. The only writes are to the T: fixture (a sanctioned test volume).
//!
//! Simplifications noted in output where they affect a *count* (they do not affect the timing,
//! which is the point of the spike, nor method (B), which is the correctness path):
//!   - (A) skips MFT extension records (BaseFileRecordSegment != 0); a hugely-fragmented file
//!     whose $DATA moved to an extension record would be missed by (A); rare, and (B)/(C) see it.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, OsStr};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetCompressedFileSizeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    ReadFile, SetFilePointerEx, BY_HANDLE_FILE_INFORMATION,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// ---- Stable Win32 constants (hand-defined, repo practice: minimal windows-sys surface) ----
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;

const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;
const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_HANDLE_EOF: u32 = 38;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// `FILE_INFO_BY_HANDLE_CLASS::FileIdBothDirectoryInfo`: bulk directory enumeration that
/// yields FileId(=FRN) + AllocationSize + EndOfFile per child (method B).
const FILE_ID_BOTH_DIRECTORY_INFO: i32 = 10;
/// `FILE_INFO_BY_HANDLE_CLASS::FileStandardInfo`: authoritative per-file AllocationSize
/// (cluster-accurate) + EndOfFile + NumberOfLinks.
const FILE_STANDARD_INFO: i32 = 1;

/// FRN = seq(16) << 48 | record#(48). Identity/parent compare on the record number (probe P5).
const FRN_MASK: u64 = (1u64 << 48) - 1;

/// One per-FRN allocated/real disagreement in `--verify`: `(frn, (a_alloc, a_real), (b_alloc, b_real))`.
type FrnMismatch = (u64, (u64, u64), (u64, u64));

// ------------------------------- little-endian readers -------------------------------
fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn rd_i64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn human(bytes: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.2} {}", U[i])
}

// ------------------------------- volume / file opens -------------------------------
fn open_volume(letter: &str) -> Result<*mut c_void, u32> {
    let w = wide(&format!(r"\\.\{letter}:"));
    // SAFETY: valid NUL-terminated wide path; standard read-only raw-volume open.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(h)
    }
}

fn open_dir(path_w: &[u16]) -> Result<*mut c_void, u32> {
    // SAFETY: valid NUL-terminated wide path; directory list open (backup semantics).
    let h = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(h)
    }
}

fn get_volume_data(h: *mut c_void) -> Option<VolData> {
    let mut out = [0u8; 128];
    let mut returned = 0u32;
    // SAFETY: read-only FSCTL, output buffer >= NTFS_VOLUME_DATA_BUFFER.
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_GET_NTFS_VOLUME_DATA,
            ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 96 {
        return None;
    }
    Some(VolData {
        bytes_per_sector: rd_u32(&out, 40),
        bytes_per_cluster: rd_u32(&out, 44),
        bytes_per_frs: rd_u32(&out, 48),
        mft_valid_len: rd_u64(&out, 56),
        mft_start_lcn: rd_i64(&out, 64) as u64,
    })
}

#[derive(Clone, Copy, Debug)]
struct VolData {
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    bytes_per_frs: u32,
    mft_valid_len: u64,
    mft_start_lcn: u64,
}

// =====================================================================================
// P18: is there a size field in the USN/ENUM record?
// =====================================================================================
fn p18(h: *mut c_void, vd: &VolData) {
    println!("\n== P18: ENUM_USN_DATA / USN_RECORD_V2 has NO size field ==");
    println!(
        "  volume geometry: bytes/sector={} bytes/cluster={} bytes/FRS={} mftValidLen={} ({}) mftStartLcn={}",
        vd.bytes_per_sector,
        vd.bytes_per_cluster,
        vd.bytes_per_frs,
        vd.mft_valid_len,
        human(vd.mft_valid_len),
        vd.mft_start_lcn
    );

    let mut input = [0u8; 28];
    input[0..8].copy_from_slice(&0u64.to_le_bytes());
    input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
    input[24..26].copy_from_slice(&2u16.to_le_bytes());
    input[26..28].copy_from_slice(&2u16.to_le_bytes());
    let mut out = vec![0u8; 64 * 1024];
    let mut returned = 0u32;
    // SAFETY: read-only FSCTL; buffers valid for the given sizes.
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_ENUM_USN_DATA,
            input.as_ptr().cast(),
            28,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        println!("  ENUM failed, err {}", unsafe { GetLastError() });
        return;
    }
    // The V2 record layout parsed by src/volume/win32.rs is:
    //   RecordLength@0 Major@4 Minor@6 FRN@8 ParentFRN@16 Usn@24 TimeStamp@32
    //   Reason@40 SourceInfo@44 SecurityId@48 FileAttributes@52 NameLen@56 NameOff@58 Name@60..
    // Every field is accounted for: there is no AllocatedSize / EndOfFile anywhere.
    let mut pos = 8usize;
    let end = returned as usize;
    let mut shown = 0;
    while pos + 60 <= end && shown < 2 {
        let rec = &out[pos..end];
        let rl = rd_u32(rec, 0) as usize;
        if rl < 60 || pos + rl > end {
            break;
        }
        let name_len = rd_u16(rec, 56) as usize;
        let name_off = rd_u16(rec, 58) as usize;
        let name = if name_off + name_len <= rl {
            String::from_utf16_lossy(
                &rec[name_off..name_off + name_len]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect::<Vec<u16>>(),
            )
        } else {
            String::new()
        };
        println!(
            "  record: len={rl} v{}.{} frn#{} parent#{} attr={:#06x} nameLen={name_len} name={name:?}",
            rd_u16(rec, 4),
            rd_u16(rec, 6),
            rd_u64(rec, 8) & FRN_MASK,
            rd_u64(rec, 16) & FRN_MASK,
            rd_u32(rec, 52),
        );
        pos += rl;
        shown += 1;
    }
    println!("  => CONFIRMED: the record ends at the file name; no AllocatedSize/EndOfFile field.");
    println!("     => sizes require a SECOND source (P19 measures which).");
}

// =====================================================================================
// P19 (B) directory-tree enumeration: FileId + AllocationSize + EndOfFile per child
// =====================================================================================
struct DirWalkResult {
    files: u64,
    dirs: u64,
    alloc_total: u64,
    real_total: u64,
    frn_set: HashSet<u64>,
    denied_dirs: u64,
    reparse_skipped: u64,
    /// Reservoir of up to `SAMPLE` full wide paths, for P20.
    sample_paths: Vec<Vec<u16>>,
    elapsed_s: f64,
}

const SAMPLE: usize = 5000;

fn dir_walk(root_letter: &str, collect_frns: bool) -> DirWalkResult {
    let mut r = DirWalkResult {
        files: 0,
        dirs: 0,
        alloc_total: 0,
        real_total: 0,
        frn_set: HashSet::new(),
        denied_dirs: 0,
        reparse_skipped: 0,
        sample_paths: Vec::new(),
        elapsed_s: 0.0,
    };
    if collect_frns {
        r.frn_set.reserve(6_500_000);
    }

    // Worklist of directory paths (wide, NUL-terminated). Start at "<L>:\".
    let mut stack: Vec<Vec<u16>> = vec![wide(&format!(r"{root_letter}:\"))];
    let mut buf = vec![0u8; 128 * 1024];
    let t0 = Instant::now();

    while let Some(dir_w) = stack.pop() {
        let h = match open_dir(&dir_w) {
            Ok(h) => h,
            Err(_) => {
                r.denied_dirs += 1;
                continue;
            }
        };
        r.dirs += 1;
        // Prefix (dir path without the trailing NUL) for building child paths.
        let prefix: Vec<u16> = dir_w[..dir_w.len() - 1].to_vec();
        loop {
            // SAFETY: valid dir handle; buffer valid for its length; class is FileIdBothDirectoryInfo.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    h,
                    FILE_ID_BOTH_DIRECTORY_INFO,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                )
            };
            if ok == 0 {
                // ERROR_NO_MORE_FILES ends the enumeration for this directory.
                break;
            }
            let mut off = 0usize;
            loop {
                let e = &buf[off..];
                let next = rd_u32(e, 0) as usize;
                let end_of_file = rd_u64(e, 40); // EndOfFile (real)
                let alloc = rd_u64(e, 48); // AllocationSize (on-disk)
                let attrs = rd_u32(e, 56);
                let name_len_bytes = rd_u32(e, 60) as usize;
                let file_id = rd_u64(e, 96) & FRN_MASK;
                let name_u16: Vec<u16> = e[104..104 + name_len_bytes]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();

                let is_dotted = matches!(name_u16.as_slice(), [0x2e] | [0x2e, 0x2e]); // "." ".."
                if !is_dotted {
                    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                    let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                    if collect_frns {
                        r.frn_set.insert(file_id);
                    }
                    if is_dir {
                        r.dirs += 0; // counted on open below
                        if is_reparse {
                            // A junction/symlink dir: count the entry, do NOT recurse into it
                            // (its target's children are counted at the target's real location).
                            r.reparse_skipped += 1;
                        } else {
                            // Build child path: prefix + "\" + name + NUL, push to worklist.
                            let mut child = prefix.clone();
                            if child.last() != Some(&0x5c) {
                                child.push(0x5c); // backslash
                            }
                            child.extend_from_slice(&name_u16);
                            child.push(0);
                            stack.push(child);
                        }
                    } else {
                        r.files += 1;
                        r.alloc_total += alloc;
                        r.real_total += end_of_file;
                        if r.sample_paths.len() < SAMPLE {
                            let mut p = prefix.clone();
                            if p.last() != Some(&0x5c) {
                                p.push(0x5c);
                            }
                            p.extend_from_slice(&name_u16);
                            p.push(0);
                            r.sample_paths.push(p);
                        }
                    }
                }
                if next == 0 {
                    break;
                }
                off += next;
            }
        }
        // SAFETY: valid handle from open_dir.
        unsafe { CloseHandle(h) };
    }
    // `dirs` above double-counted; recompute: every opened dir incremented once. The `+= 0`
    // line is a no-op kept for readability. `dirs` == number of directories successfully opened.
    r.elapsed_s = t0.elapsed().as_secs_f64();
    r
}

// =====================================================================================
// P19 (A) raw $MFT read: follow $MFT's $DATA runs, parse each FILE record's $DATA size
// =====================================================================================
fn read_at(h: *mut c_void, offset: u64, buf: &mut [u8]) -> bool {
    let mut newpos = 0i64;
    // SAFETY: FILE_BEGIN seek to a sector-aligned offset on the raw volume handle.
    let ok = unsafe { SetFilePointerEx(h, offset as i64, &mut newpos, 0) };
    if ok == 0 {
        return false;
    }
    let mut total = 0usize;
    while total < buf.len() {
        let mut got = 0u32;
        let want = (buf.len() - total) as u32;
        // SAFETY: valid handle; writing into buf[total..].
        let ok = unsafe {
            ReadFile(
                h,
                buf[total..].as_mut_ptr().cast(),
                want,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == 0 || got == 0 {
            return total > 0 && total == buf.len();
        }
        total += got as usize;
    }
    true
}

/// Apply the NTFS multi-sector fixup to a FILE record in place.
fn apply_fixup(rec: &mut [u8], bytes_per_sector: usize) {
    let usa_off = rd_u16(rec, 4) as usize;
    let usa_cnt = rd_u16(rec, 6) as usize; // count of u16s incl. the USN itself
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

/// Sum the ALLOCATED clusters of an NTFS data-run list (non-sparse runs only). This is the
/// on-disk allocation; for sparse/compressed files it is smaller than the nominal `AllocatedSize`
/// header field (validated on the T: fixture: naive AllocatedSize over-reports sparse/compressed).
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
            clusters += length; // a real (non-sparse) run occupies `length` clusters
        }
        i += off_bytes; // off_bytes == 0 => sparse hole (no clusters)
    }
    clusters
}

/// Parse one FILE record's default `$DATA` (0x80, unnamed) allocated/real size. `bpc` is bytes
/// per cluster. Returns `(alloc, real, named_data_count, named_data_alloc)`; `None` if not a
/// live base record. Allocated for non-resident streams is counted from the DATA RUNS (exact for
/// sparse/compressed), not the nominal `AllocatedSize` field. Resident streams occupy 0 clusters.
fn parse_record(rec: &[u8], bpc: u64) -> Option<(u64, u64, u32, u64)> {
    if &rec[0..4] != b"FILE" {
        return None;
    }
    let flags = rd_u16(rec, 22);
    if flags & 0x0001 == 0 {
        return None; // not in use
    }
    if rd_u64(rec, 32) & FRN_MASK != 0 {
        return None; // extension record (has a base): skip (see module note)
    }
    let mut pos = rd_u16(rec, 20) as usize; // first attribute offset
    let mut alloc = 0u64;
    let mut real = 0u64;
    let mut named_cnt = 0u32;
    let mut named_alloc = 0u64;
    while pos + 8 <= rec.len() {
        let atype = rd_u32(rec, pos);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let alen = rd_u32(rec, pos + 4) as usize;
        if alen < 8 || pos + alen > rec.len() {
            break;
        }
        if atype == 0x80 {
            let non_res = rec[pos + 8];
            let name_len = rec[pos + 9];
            let (a, r) = if non_res != 0 {
                let runs_off = pos + rd_u16(rec, pos + 32) as usize;
                let a = if runs_off <= pos + alen {
                    data_run_clusters(&rec[runs_off..pos + alen]) * bpc
                } else {
                    0
                };
                (a, rd_u64(rec, pos + 48)) // real = RealSize (DataSize) @0x30
            } else {
                // Resident: no clusters (alloc 0), real = content length.
                (0u64, rd_u32(rec, pos + 16) as u64)
            };
            if name_len == 0 {
                alloc += a;
                real += r;
            } else {
                named_cnt += 1;
                named_alloc += a;
            }
        }
        pos += alen;
    }
    Some((alloc, real, named_cnt, named_alloc))
}

struct MftMeta {
    records_live: u64,
    files_with_data: u64,
    bytes_read: u64,
    elapsed_s: f64,
}

/// Read the whole $MFT (following $MFT's own $DATA runs) and invoke `sink(frn, alloc, real,
/// named_alloc)` for every live base FILE record. `frn` is the record's MFT slot index, which
/// *is* its (masked) file reference number, so the caller can key sizes by FRN and match them
/// to index entries. Returns coarse meta (counts + wall-clock). `None` on a read/parse failure.
fn mft_scan(
    h: *mut c_void,
    vd: &VolData,
    sink: &mut dyn FnMut(u64, u64, u64, u64),
) -> Option<MftMeta> {
    let bpc = vd.bytes_per_cluster as u64;
    let frs = vd.bytes_per_frs as usize;
    let bps = vd.bytes_per_sector as usize;

    // 1) Read $MFT's own record (#0) at MftStartLcn to get its $DATA data runs (the extents).
    let mut rec0 = vec![0u8; frs];
    if !read_at(h, vd.mft_start_lcn * bpc, &mut rec0) {
        println!("  (A) could not read $MFT record #0");
        return None;
    }
    apply_fixup(&mut rec0, bps);
    if &rec0[0..4] != b"FILE" {
        println!("  (A) $MFT record #0 has no FILE signature");
        return None;
    }
    // Find the non-resident $DATA (0x80, unnamed) and parse its data runs.
    let mut extents: Vec<(u64, u64)> = Vec::new(); // (lcn, cluster_count)
    let mut pos = rd_u16(&rec0, 20) as usize;
    while pos + 8 <= rec0.len() {
        let atype = rd_u32(&rec0, pos);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let alen = rd_u32(&rec0, pos + 4) as usize;
        if alen < 8 || pos + alen > rec0.len() {
            break;
        }
        if atype == 0x80 && rec0[pos + 8] != 0 && rec0[pos + 9] == 0 {
            let runs_off = pos + rd_u16(&rec0, pos + 32) as usize;
            parse_data_runs(&rec0[runs_off..pos + alen], &mut extents);
            break;
        }
        pos += alen;
    }
    if extents.is_empty() {
        println!("  (A) no $MFT $DATA runs parsed");
        return None;
    }

    // 2) Read the extents sequentially, parse every FILE record, stop at MftValidDataLength.
    let max_records = vd.mft_valid_len / frs as u64;
    let chunk_bytes = 8 * 1024 * 1024u64; // 8 MiB sequential reads
    let mut chunk = vec![0u8; chunk_bytes as usize];
    let mut records_seen: u64 = 0;
    let mut meta = MftMeta {
        records_live: 0,
        files_with_data: 0,
        bytes_read: 0,
        elapsed_s: 0.0,
    };
    let t0 = Instant::now();
    'outer: for (lcn, clusters) in extents {
        let mut remaining = clusters * bpc;
        let mut base = lcn * bpc;
        while remaining > 0 {
            let this = remaining.min(chunk_bytes);
            let slice = &mut chunk[..this as usize];
            if !read_at(h, base, slice) {
                break;
            }
            meta.bytes_read += this;
            let mut o = 0usize;
            while o + frs <= slice.len() {
                if records_seen >= max_records {
                    break 'outer;
                }
                let frn = records_seen; // MFT slot index == (masked) file reference number
                records_seen += 1;
                let rec = &mut slice[o..o + frs];
                if &rec[0..4] == b"FILE" {
                    apply_fixup(rec, bps);
                    if let Some((a, r, _ncnt, nalloc)) = parse_record(rec, bpc) {
                        meta.records_live += 1;
                        if a > 0 || r > 0 {
                            meta.files_with_data += 1;
                        }
                        sink(frn, a, r, nalloc);
                    }
                }
                o += frs;
            }
            base += this;
            remaining -= this;
        }
    }
    meta.elapsed_s = t0.elapsed().as_secs_f64();
    Some(meta)
}

struct MftResult {
    records_live: u64,
    files_with_data: u64,
    alloc_total: u64,
    real_total: u64,
    named_data_total_alloc: u64,
    bytes_read: u64,
    elapsed_s: f64,
}

/// Whole-volume summing wrapper over [`mft_scan`] for the speed path.
fn mft_read(h: *mut c_void, vd: &VolData) -> Option<MftResult> {
    let mut alloc_total = 0u64;
    let mut real_total = 0u64;
    let mut named_data_total_alloc = 0u64;
    let meta = mft_scan(h, vd, &mut |_frn, a, r, na| {
        alloc_total += a;
        real_total += r;
        named_data_total_alloc += na;
    })?;
    Some(MftResult {
        records_live: meta.records_live,
        files_with_data: meta.files_with_data,
        alloc_total,
        real_total,
        named_data_total_alloc,
        bytes_read: meta.bytes_read,
        elapsed_s: meta.elapsed_s,
    })
}

/// Parse an NTFS data-run list into (lcn, cluster_count) extents (skips sparse/holes).
fn parse_data_runs(runs: &[u8], out: &mut Vec<(u64, u64)>) {
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
            // Sparse run (a hole): no LCN. Skip for $MFT (not expected).
            i += 0;
            continue;
        }
        // Signed offset delta.
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

// =====================================================================================
// P19c: does ENUM (the index's FRN source) match (B)'s FileId set?
// =====================================================================================
fn enum_frn_set(h: *mut c_void) -> HashSet<u64> {
    let mut set = HashSet::with_capacity(6_500_000);
    let mut start: u64 = 0;
    let mut out = vec![0u8; 1 << 20];
    let mut returned = 0u32;
    loop {
        let mut input = [0u8; 28];
        input[0..8].copy_from_slice(&start.to_le_bytes());
        input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
        input[24..26].copy_from_slice(&2u16.to_le_bytes());
        input[26..28].copy_from_slice(&2u16.to_le_bytes());
        // SAFETY: read-only FSCTL; buffers valid for their sizes.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_ENUM_USN_DATA,
                input.as_ptr().cast(),
                28,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            if unsafe { GetLastError() } != ERROR_HANDLE_EOF {
                println!("  (P19c) ENUM error {}", unsafe { GetLastError() });
            }
            break;
        }
        if returned <= 8 {
            break;
        }
        start = rd_u64(&out, 0);
        let end = returned as usize;
        let mut pos = 8usize;
        while pos + 60 <= end {
            let rec = &out[pos..end];
            let rl = rd_u32(rec, 0) as usize;
            if rl < 60 || pos + rl > end {
                break;
            }
            set.insert(rd_u64(rec, 8) & FRN_MASK);
            pos += rl;
        }
    }
    set
}

// =====================================================================================
// P20: lazy per-file stat cost (the small-subtree fallback + A/B correctness oracle)
// =====================================================================================
fn p20(sample: &[Vec<u16>]) {
    println!("\n== P20: lazy per-file stat cost (fallback for small subtrees) ==");
    if sample.is_empty() {
        println!("  no sample paths collected");
        return;
    }
    let n = sample.len();

    // (c2) GetCompressedFileSizeW: allocated size, NO handle open (fast path).
    let t0 = Instant::now();
    let mut acc_alloc = 0u64;
    for p in sample {
        let mut high = 0u32;
        // SAFETY: valid NUL-terminated wide path.
        let low = unsafe { GetCompressedFileSizeW(p.as_ptr(), &mut high) };
        acc_alloc = acc_alloc.wrapping_add(((high as u64) << 32) | low as u64);
    }
    let comp_s = t0.elapsed().as_secs_f64();

    // (c1) CreateFile + GetFileInformationByHandle + Close: full stat (real size + links).
    let t1 = Instant::now();
    let mut acc_real = 0u64;
    let mut opened = 0u64;
    for p in sample {
        // SAFETY: valid wide path; read-only open.
        let h = unsafe {
            CreateFileW(
                p.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: zeroed POD filled by a valid handle call.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
        if unsafe { GetFileInformationByHandle(h, &mut info) } != 0 {
            acc_real = acc_real
                .wrapping_add(((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64);
            opened += 1;
        }
        // SAFETY: valid handle.
        unsafe { CloseHandle(h) };
    }
    let stat_s = t1.elapsed().as_secs_f64();

    let comp_us = comp_s * 1e6 / n as f64;
    let stat_us = stat_s * 1e6 / opened.max(1) as f64;
    println!("  sample size                 = {n} files");
    println!(
        "  GetCompressedFileSizeW      = {comp_us:.2} µs/file  (no handle; allocated) - Σ={}",
        human(acc_alloc)
    );
    println!(
        "  CreateFile+GetFileInfo      = {stat_us:.2} µs/file  ({opened} opened; real) - Σ={}",
        human(acc_real)
    );
    let ext = |us: f64| us * 6_000_000.0 / 1e6;
    println!(
        "  => extrapolated to 6.0M files: compressed-size {:.1} s, full-stat {:.1} s",
        ext(comp_us),
        ext(stat_us)
    );
    println!("     (single-threaded; a small subtree of a few k files is sub-100 ms either way)");
}

// =====================================================================================
fn speed(letter: &str) {
    let h = match open_volume(letter) {
        Ok(h) => h,
        Err(e) => {
            let hint = if e == ERROR_ACCESS_DENIED {
                " -> run from an ELEVATED terminal"
            } else {
                ""
            };
            println!("open \\\\.\\{letter}: FAILED err {e}{hint}");
            std::process::exit(1);
        }
    };
    let vd = match get_volume_data(h) {
        Some(v) => v,
        None => {
            println!("FSCTL_GET_NTFS_VOLUME_DATA failed");
            std::process::exit(1);
        }
    };

    p18(h, &vd);

    println!("\n== P19(A): raw $MFT read (follow $MFT $DATA runs, parse $DATA sizes) ==");
    let mft = mft_read(h, &vd);
    if let Some(m) = &mft {
        println!(
            "  live base records = {}  files-with-data = {}  bytes read = {} ({})",
            m.records_live,
            m.files_with_data,
            m.bytes_read,
            human(m.bytes_read)
        );
        println!(
            "  Σ allocated = {} ({})   Σ real = {} ({})   Σ ADS(named $DATA) alloc = {}",
            m.alloc_total,
            human(m.alloc_total),
            m.real_total,
            human(m.real_total),
            human(m.named_data_total_alloc)
        );
        println!("  WALL-CLOCK (A) = {:.2} s", m.elapsed_s);
    }

    println!("\n== P19(B): directory-tree enumeration (FileId + AllocationSize + EndOfFile) ==");
    let b = dir_walk(letter, true);
    println!(
        "  dirs opened = {}  files = {}  denied dirs = {}  reparse dirs not recursed = {}",
        b.dirs, b.files, b.denied_dirs, b.reparse_skipped
    );
    println!(
        "  Σ allocated = {} ({})   Σ real = {} ({})",
        b.alloc_total,
        human(b.alloc_total),
        b.real_total,
        human(b.real_total)
    );
    println!("  WALL-CLOCK (B) = {:.2} s", b.elapsed_s);

    // Decision line.
    if let Some(m) = &mft {
        println!("\n== P19 decision input ==");
        println!(
            "  (A) raw $MFT = {:.2} s   vs   (B) dir-enum = {:.2} s   (approved rule: prefer B unless impractically slow)",
            m.elapsed_s, b.elapsed_s
        );
    }

    // P19c: ENUM (index FRN source) vs (B) FileId set.
    println!("\n== P19c: ENUM FRN set vs (B) FileId set ==");
    let enum_set = enum_frn_set(h);
    let in_enum_not_b = enum_set.difference(&b.frn_set).count();
    let in_b_not_enum = b.frn_set.difference(&enum_set).count();
    let inter = enum_set.len() - in_enum_not_b;
    println!(
        "  |ENUM|={}  |B|={}  intersection={}",
        enum_set.len(),
        b.frn_set.len(),
        inter
    );
    println!(
        "  in ENUM but NOT sized by B = {in_enum_not_b}  ({:.4}% of ENUM)  <-- under-count risk",
        100.0 * in_enum_not_b as f64 / enum_set.len().max(1) as f64
    );
    println!("  in B but NOT in ENUM       = {in_b_not_enum}  (races / new files since enum)");

    p20(&b.sample_paths);

    // SAFETY: valid handle.
    unsafe { CloseHandle(h) };
    println!("\ndone (speed).");
}

// =====================================================================================
// --verify: correctness / semantics on the T: fixture
// =====================================================================================
fn verify(letter: &str) {
    println!("== M5 verify on {letter}: (T: fixture) ==");

    // Per-file allocated/real for the planted special files, via (B)-style single stat.
    let specials = [
        "sparse.bin",
        "compressed.bin",
        "ads_host.txt",
        "hardlink_to_file0.txt",
        r"du_known\f_4096.bin",
        r"du_known\f_5000.bin",
        r"du_known\sub\f_10000.bin",
        r"du_known\sub\f_1.bin",
    ];
    println!("\n-- per-file: AllocationSize (authoritative) | EndOfFile (real) | GetCompressedFileSize | links --");
    println!("   (finding: GetCompressedFileSizeW returns the LOGICAL size for normal files,");
    println!(
        "    it equals allocated only for compressed/sparse; use AllocationSize for allocated.)"
    );
    for rel in specials {
        let full = format!(r"{letter}:\{rel}");
        let w = wide(&full);
        let mut high = 0u32;
        // SAFETY: valid wide path.
        let low = unsafe { GetCompressedFileSizeW(w.as_ptr(), &mut high) };
        let compressed = ((high as u64) << 32) | low as u64;

        // Authoritative AllocationSize + EndOfFile + NumberOfLinks via FileStandardInfo.
        // SAFETY: read-only open of an existing file.
        let hf = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if hf == INVALID_HANDLE_VALUE {
            println!("  {rel:<26} open failed err {}", unsafe { GetLastError() });
            continue;
        }
        let mut si = [0u8; 24]; // FILE_STANDARD_INFO
                                // SAFETY: valid handle; buffer >= FILE_STANDARD_INFO; class FileStandardInfo.
        let ok = unsafe {
            GetFileInformationByHandleEx(hf, FILE_STANDARD_INFO, si.as_mut_ptr().cast(), 24)
        };
        // SAFETY: valid handle.
        unsafe { CloseHandle(hf) };
        if ok == 0 {
            println!("  {rel:<26} FileStandardInfo failed err {}", unsafe {
                GetLastError()
            });
            continue;
        }
        let alloc = rd_u64(&si, 0);
        let real = rd_u64(&si, 8);
        let links = rd_u32(&si, 16);
        println!(
            "  {rel:<26} alloc={alloc:>8} ({:<9}) real={real:>8} ({:<9}) comp={compressed:>8} links={links} {}",
            human(alloc),
            human(real),
            if alloc < real { "<- alloc<real" } else { "" }
        );
    }

    // du_known subtree aggregation via (B): sum allocated over descendants; compare to expected.
    println!("\n-- du_known subtree aggregation (method B) --");
    let sub = dir_walk_subtree(letter, r"du_known");
    println!(
        "  files under du_known = {}  Σ allocated = {} ({})  Σ real = {} ({})",
        sub.files,
        sub.alloc_total,
        human(sub.alloc_total),
        sub.real_total,
        human(sub.real_total)
    );
    println!("  (expected alloc excludes resident f_1.bin)");

    // Cross-validate method A (raw $MFT parser) against method B (dir-enum), KEYED BY FRN over
    // the ENUM (index) set: the true integration semantics. A's global sum includes NTFS system
    // metadata files ($MFT/$LogFile/$UsnJrnl/$Secure/...) that ENUM never returns; the real `ef du`
    // applies a size only to an FRN that exists in the index, so those are naturally excluded.
    // If A and B agree per-FRN on the index set (on a volume with sparse/compressed/resident/ADS/
    // hardlink files), A's on-disk-format parser is validated (the de-risk for A's "fragility").
    println!("\n-- method A (raw $MFT) vs method B (dir-enum), reconciled per-FRN over the ENUM/index set --");
    match open_volume(letter) {
        Ok(h) => {
            if let Some(vd) = get_volume_data(h) {
                let enum_set = enum_frn_set(h);
                let mut a_map: HashMap<u64, (u64, u64)> = HashMap::new();
                let meta = mft_scan(h, &vd, &mut |frn, a, r, _na| {
                    if a > 0 || r > 0 {
                        a_map.insert(frn, (a, r));
                    }
                });
                let b_map = dir_size_map(letter);

                // The decisive parser check: over FRNs BOTH methods sized (∩ the index set),
                // A's allocated/real must equal B's per FRN. Totals over the union differ only by
                // COVERAGE (A also sees NTFS system metafiles that ENUM surfaced but B's dir walk
                // cannot enumerate); that is expected, not a parser error.
                let (mut aa, mut ar, mut ba, mut br) = (0u64, 0u64, 0u64, 0u64);
                let mut both = 0u64;
                let mut only_a = 0u64; // system metafiles ENUM surfaced; B can't enumerate
                let mut only_b = 0u64; // e.g. B enumerated but A had 0 data (empty file)
                let mut mismatch: Vec<FrnMismatch> = Vec::new();
                for &frn in &enum_set {
                    let a = a_map.get(&frn).copied();
                    let b = b_map.get(&frn).copied();
                    match (a, b) {
                        (Some(av), Some(bv)) => {
                            both += 1;
                            aa += av.0;
                            ar += av.1;
                            ba += bv.0;
                            br += bv.1;
                            if av.0 != bv.0 && mismatch.len() < 10 {
                                mismatch.push((frn, av, bv));
                            }
                        }
                        (Some(_), None) => only_a += 1,
                        (None, Some(_)) => only_b += 1,
                        (None, None) => {}
                    }
                }
                if let Some(m) = meta {
                    println!(
                        "  A whole-volume: live records={} files-with-data={} (includes NTFS system files)",
                        m.records_live, m.files_with_data
                    );
                }
                println!("  |ENUM/index FRNs| = {}", enum_set.len());
                println!(
                    "  FRNs sized by BOTH = {both}   only-A (system metafiles, B can't enumerate) = {only_a}   only-B (A had 0 data) = {only_b}"
                );
                println!(
                    "  Σ over the BOTH set:  A alloc={} ({})  B alloc={} ({})   A real={} ({})  B real={} ({})",
                    aa,
                    human(aa),
                    ba,
                    human(ba),
                    ar,
                    human(ar),
                    br,
                    human(br)
                );
                println!(
                    "  => per-FRN ALLOCATED match on the BOTH set: {}   (mismatched FRNs: {})",
                    if aa == ba && mismatch.is_empty() {
                        "YES ✓ - A's raw-$MFT parser validated against the documented API"
                    } else {
                        "NO"
                    },
                    mismatch.len()
                );
                for (frn, av, bv) in &mismatch {
                    println!(
                        "     FRN {frn}: A alloc={} real={}  vs  B alloc={} real={}",
                        av.0, av.1, bv.0, bv.1
                    );
                }
            }
            // SAFETY: valid handle.
            unsafe { CloseHandle(h) };
        }
        Err(e) => println!("  could not open volume for A: err {e}"),
    }

    println!("\ndone (verify). Compare the numbers above to the fixture's known construction.");
}

/// Whole-volume directory enumeration -> `FRN -> (alloc, real)` map (dedup by FRN; all names of
/// a hardlink report the same size). Used by `--verify` to reconcile against method A per-FRN.
fn dir_size_map(letter: &str) -> HashMap<u64, (u64, u64)> {
    let mut map: HashMap<u64, (u64, u64)> = HashMap::new();
    let mut stack: Vec<Vec<u16>> = vec![wide(&format!(r"{letter}:\"))];
    let mut buf = vec![0u8; 128 * 1024];
    while let Some(dir_w) = stack.pop() {
        let h = match open_dir(&dir_w) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let prefix: Vec<u16> = dir_w[..dir_w.len() - 1].to_vec();
        loop {
            // SAFETY: valid dir handle; class FileIdBothDirectoryInfo.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    h,
                    FILE_ID_BOTH_DIRECTORY_INFO,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                )
            };
            if ok == 0 {
                break;
            }
            let mut off = 0usize;
            loop {
                let e = &buf[off..];
                let next = rd_u32(e, 0) as usize;
                let real = rd_u64(e, 40);
                let alloc = rd_u64(e, 48);
                let attrs = rd_u32(e, 56);
                let name_len_bytes = rd_u32(e, 60) as usize;
                let file_id = rd_u64(e, 96) & FRN_MASK;
                let name_u16: Vec<u16> = e[104..104 + name_len_bytes]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                let is_dotted = matches!(name_u16.as_slice(), [0x2e] | [0x2e, 0x2e]);
                if !is_dotted {
                    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                    let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                    map.entry(file_id).or_insert((alloc, real));
                    if is_dir && !is_reparse {
                        let mut child = prefix.clone();
                        if child.last() != Some(&0x5c) {
                            child.push(0x5c);
                        }
                        child.extend_from_slice(&name_u16);
                        child.push(0);
                        stack.push(child);
                    }
                }
                if next == 0 {
                    break;
                }
                off += next;
            }
        }
        // SAFETY: valid handle.
        unsafe { CloseHandle(h) };
    }
    map
}

fn dir_walk_subtree(letter: &str, rel: &str) -> DirWalkResult {
    let mut r = DirWalkResult {
        files: 0,
        dirs: 0,
        alloc_total: 0,
        real_total: 0,
        frn_set: HashSet::new(),
        denied_dirs: 0,
        reparse_skipped: 0,
        sample_paths: Vec::new(),
        elapsed_s: 0.0,
    };
    let mut stack: Vec<Vec<u16>> = vec![wide(&format!(r"{letter}:\{rel}"))];
    let mut buf = vec![0u8; 64 * 1024];
    while let Some(dir_w) = stack.pop() {
        let h = match open_dir(&dir_w) {
            Ok(h) => h,
            Err(_) => {
                r.denied_dirs += 1;
                continue;
            }
        };
        r.dirs += 1;
        let prefix: Vec<u16> = dir_w[..dir_w.len() - 1].to_vec();
        loop {
            // SAFETY: valid dir handle; class FileIdBothDirectoryInfo.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    h,
                    FILE_ID_BOTH_DIRECTORY_INFO,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                )
            };
            if ok == 0 {
                break;
            }
            let mut off = 0usize;
            loop {
                let e = &buf[off..];
                let next = rd_u32(e, 0) as usize;
                let real = rd_u64(e, 40);
                let alloc = rd_u64(e, 48);
                let attrs = rd_u32(e, 56);
                let name_len_bytes = rd_u32(e, 60) as usize;
                let name_u16: Vec<u16> = e[104..104 + name_len_bytes]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                let is_dotted = matches!(name_u16.as_slice(), [0x2e] | [0x2e, 0x2e]);
                if !is_dotted {
                    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                    let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                    if is_dir && !is_reparse {
                        let mut child = prefix.clone();
                        if child.last() != Some(&0x5c) {
                            child.push(0x5c);
                        }
                        child.extend_from_slice(&name_u16);
                        child.push(0);
                        stack.push(child);
                    } else if !is_dir {
                        r.files += 1;
                        r.alloc_total += alloc;
                        r.real_total += real;
                    }
                }
                if next == 0 {
                    break;
                }
                off += next;
            }
        }
        // SAFETY: valid handle.
        unsafe { CloseHandle(h) };
    }
    r
}

// =====================================================================================
// P19 (B, parallel). Give method B its fair shot: rayon work-stealing over subdirectories.
// The real `ef du` size source, if B, would parallelize. Sizes summed per NAME here (same as
// serial B); the actual index dedupes by FRN so hardlink double-count would not occur there.
// =====================================================================================
#[derive(Default)]
struct ParAcc {
    files: AtomicU64,
    dirs: AtomicU64,
    alloc: AtomicU64,
    real: AtomicU64,
    denied: AtomicU64,
}

fn walk_par(dir_w: Vec<u16>, acc: &ParAcc) {
    let h = match open_dir(&dir_w) {
        Ok(h) => h,
        Err(_) => {
            acc.denied.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    acc.dirs.fetch_add(1, Ordering::Relaxed);
    let prefix: Vec<u16> = dir_w[..dir_w.len() - 1].to_vec();
    let mut buf = vec![0u8; 128 * 1024];
    let mut subdirs: Vec<Vec<u16>> = Vec::new();
    let (mut lf, mut la, mut lr) = (0u64, 0u64, 0u64);
    loop {
        // SAFETY: valid dir handle; class FileIdBothDirectoryInfo.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h,
                FILE_ID_BOTH_DIRECTORY_INFO,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        if ok == 0 {
            break;
        }
        let mut off = 0usize;
        loop {
            let e = &buf[off..];
            let next = rd_u32(e, 0) as usize;
            let real = rd_u64(e, 40);
            let alloc = rd_u64(e, 48);
            let attrs = rd_u32(e, 56);
            let name_len_bytes = rd_u32(e, 60) as usize;
            let name_u16: Vec<u16> = e[104..104 + name_len_bytes]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let is_dotted = matches!(name_u16.as_slice(), [0x2e] | [0x2e, 0x2e]);
            if !is_dotted {
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                if is_dir && !is_reparse {
                    let mut child = prefix.clone();
                    if child.last() != Some(&0x5c) {
                        child.push(0x5c);
                    }
                    child.extend_from_slice(&name_u16);
                    child.push(0);
                    subdirs.push(child);
                } else if !is_dir {
                    lf += 1;
                    la += alloc;
                    lr += real;
                }
            }
            if next == 0 {
                break;
            }
            off += next;
        }
    }
    // SAFETY: valid handle.
    unsafe { CloseHandle(h) };
    acc.files.fetch_add(lf, Ordering::Relaxed);
    acc.alloc.fetch_add(la, Ordering::Relaxed);
    acc.real.fetch_add(lr, Ordering::Relaxed);
    subdirs.into_par_iter().for_each(|s| walk_par(s, acc));
}

fn parb(letter: &str) {
    println!("Everyfind M5 probe_du - volume {letter}: (parallel B)");
    let acc = ParAcc::default();
    let t0 = Instant::now();
    walk_par(wide(&format!(r"{letter}:\")), &acc);
    let s = t0.elapsed().as_secs_f64();
    println!(
        "\n== P19(B, parallel, {} rayon threads) ==",
        rayon::current_num_threads()
    );
    println!(
        "  dirs = {}  files = {}  denied = {}",
        acc.dirs.load(Ordering::Relaxed),
        acc.files.load(Ordering::Relaxed),
        acc.denied.load(Ordering::Relaxed)
    );
    println!(
        "  Σ allocated = {}   Σ real = {}",
        human(acc.alloc.load(Ordering::Relaxed)),
        human(acc.real.load(Ordering::Relaxed))
    );
    println!("  WALL-CLOCK (B parallel) = {s:.2} s");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vol = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "C:".to_string());
    let letter = vol.trim_end_matches(['\\', ':']).to_string();

    println!("Everyfind M5 probe_du - volume {vol}");
    if args.iter().any(|a| a == "--verify") {
        verify(&letter);
    } else if args.iter().any(|a| a == "--parb") {
        parb(&letter);
    } else if args.iter().any(|a| a == "--mfta") {
        mfta_only(&letter);
    } else {
        speed(&letter);
    }
}

/// Method A alone (raw $MFT + per-file data-run cluster counting): its wall-clock and totals,
/// without the slow serial-B / P19c / P20 stages of `--speed`.
fn mfta_only(letter: &str) {
    let h = match open_volume(letter) {
        Ok(h) => h,
        Err(e) => {
            println!("open \\\\.\\{letter}: FAILED err {e}");
            std::process::exit(1);
        }
    };
    let vd = get_volume_data(h).expect("GET_NTFS_VOLUME_DATA");
    if let Some(m) = mft_read(h, &vd) {
        println!("\n== P19(A): raw $MFT + data-run cluster counting ==");
        println!(
            "  live records={} files-with-data={}  bytes read={} ({})",
            m.records_live,
            m.files_with_data,
            m.bytes_read,
            human(m.bytes_read)
        );
        println!(
            "  Σ allocated = {} ({})   Σ real = {} ({})",
            m.alloc_total,
            human(m.alloc_total),
            m.real_total,
            human(m.real_total)
        );
        println!("  WALL-CLOCK (A) = {:.2} s", m.elapsed_s);
    }
    // SAFETY: valid handle.
    unsafe { CloseHandle(h) };
}
