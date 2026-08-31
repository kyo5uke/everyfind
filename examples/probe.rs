//! Everyfind API probe: verifies real NTFS/USN behavior before the Win32 backend
//! is written. Run in an ELEVATED terminal (opening `\\.\C:` needs admin):
//!
//! ```text
//! cargo run --example probe -- C:            # probe the C: volume (P1-P9)
//! cargo run --example probe -- T: --dump     # small fixture: also dump every record
//! ```
//!
//! Answers, on real hardware:
//!   P1 QUERY_USN_JOURNAL version/fields
//!   P2 ENUM_USN_DATA input/output buffer layout + continuation
//!   P3 MFT_ENUM_DATA_V1{MinMajor=Max=2} forces V2 records (64-bit FRN)?  [top priority]
//!   P4 do 8.3 short names appear in the stream?
//!   P5 does the root (record #5) appear; how are parent refs to it encoded
//!   P6 can `\\.\C:` open / is admin required
//!   P7 total record count and enum wall-clock
//!   P8 are attributes (REPARSE_POINT/DIRECTORY) present per record
//!   P9 hardlink enum behavior (one record per FILE, or per NAME?)
//!
//! Read-only: only QUERY_USN_JOURNAL and ENUM_USN_DATA (read FSCTLs) are issued.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// Access/share/creation flags and FSCTL codes are defined locally (stable, documented
// values) to keep the windows-sys feature surface minimal.
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;
const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_INVALID_PARAMETER: u32 = 87;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// NTFS file reference number = 16-bit sequence number (high) | 48-bit record number
/// (low). Parent references and identity must be compared on the record number.
const FRN_MASK: u64 = (1u64 << 48) - 1;
/// NTFS root directory is MFT record number 5.
const ROOT_RECORD: u64 = 5;

fn rec_no(frn: u64) -> u64 {
    frn & FRN_MASK
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

struct Rec {
    major: u16,
    frn: u64,
    parent_frn: u64,
    attributes: u32,
    name: String,
}

/// Parse one USN_RECORD (V2 or V3) from the front of `rec`; return it and its length.
fn parse_record(rec: &[u8]) -> (Rec, usize) {
    let record_length = rd_u32(rec, 0) as usize;
    let major = rd_u16(rec, 4);
    // V2: FRN@8 (8), ParentFRN@16 (8), FileAttributes@52, FileNameLength@56.
    // V3: FRN@8 (16), ParentFRN@24 (16), FileAttributes@68, FileNameLength@72.
    let (frn, parent_frn, attr_off, name_len_off) = if major >= 3 {
        (rd_u64(rec, 8), rd_u64(rec, 24), 68usize, 72usize)
    } else {
        (rd_u64(rec, 8), rd_u64(rec, 16), 52usize, 56usize)
    };
    let attributes = rd_u32(rec, attr_off);
    let name_len = rd_u16(rec, name_len_off) as usize;
    let name_off = rd_u16(rec, name_len_off + 2) as usize;
    let name_u16: Vec<u16> = rec[name_off..name_off + name_len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let name = String::from_utf16_lossy(&name_u16);
    (
        Rec {
            major,
            frn,
            parent_frn,
            attributes,
            name,
        },
        record_length,
    )
}

fn open_path(path: &str) -> Result<*mut c_void, u32> {
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: standard CreateFileW call with a valid NUL-terminated wide path.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
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

/// P5 (independent): the root directory's own FRN, via GetFileInformationByHandle on
/// `<drive>:\`. Confirms the root's record number is 5 and shows its sequence bits.
fn probe_root_frn(letter: &str) {
    println!("\n== P5a: root directory FRN via handle ==");
    let root_path = format!(r"{letter}:\");
    let h = match open_path(&root_path) {
        Ok(h) => h,
        Err(e) => {
            println!("  open {root_path:?} FAILED, err {e}");
            return;
        }
    };
    // SAFETY: zeroed POD struct, filled by a valid handle call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
    if ok != 0 {
        let frn = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        println!(
            "  {root_path} full FRN = {frn}  (record# = {})",
            rec_no(frn)
        );
        println!("  links = {}", info.nNumberOfLinks);
    } else {
        println!("  GetFileInformationByHandle FAILED, err {}", unsafe {
            GetLastError()
        });
    }
    // SAFETY: valid handle from open_path.
    unsafe { CloseHandle(h) };
}

/// The record number of an existing path, via GetFileInformationByHandle.
fn file_rec_no(path: &str) -> Option<u64> {
    let h = open_path(path).ok()?;
    // SAFETY: zeroed POD filled by a valid handle call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
    // SAFETY: valid handle from open_path.
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let frn = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Some(rec_no(frn))
}

/// P9 (targeted): create a real hardlink pair on the volume, then count how many
/// ENUM records carry that shared FRN, one (one name) or two (both names).
fn probe_hardlink(h: *mut c_void, letter: &str) {
    use std::fs;
    println!("\n== P9: targeted hardlink test (mklink /H equivalent) ==");

    let dir = std::env::temp_dir().join("ef_probe_hltest");
    let file_a = dir.join("fileA_primary.txt");
    let file_b = dir.join("fileB_hardlink.txt");
    if let Err(e) = (|| -> std::io::Result<()> {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        fs::write(&file_a, b"Everyfind hardlink probe")?;
        fs::hard_link(&file_a, &file_b)?;
        Ok(())
    })() {
        println!("  setup FAILED: {e}  (temp dir must be on the {letter}: volume)");
        return;
    }

    let a_str = file_a.to_string_lossy().to_string();
    let Some(target) = file_rec_no(&a_str) else {
        println!("  could not read FRN of {a_str}");
        let _ = fs::remove_dir_all(&dir);
        return;
    };
    println!("  created 2 names sharing record# {target}:");
    println!("    {}", file_a.display());
    println!("    {}  (hardlink)", file_b.display());

    // Enumerate and collect every record carrying the target record number.
    let mut names: Vec<String> = Vec::new();
    let mut start_frn: u64 = 0;
    let mut out = vec![0u8; 128 * 1024];
    let mut returned = 0u32;
    loop {
        let mut input = [0u8; 32];
        input[0..8].copy_from_slice(&start_frn.to_le_bytes());
        input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
        input[24..26].copy_from_slice(&2u16.to_le_bytes());
        input[26..28].copy_from_slice(&2u16.to_le_bytes());
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
        if ok == 0 || returned <= 8 {
            break;
        }
        start_frn = rd_u64(&out, 0);
        let mut pos = 8usize;
        while pos < returned as usize {
            let (r, rec_len) = parse_record(&out[pos..]);
            if rec_len == 0 {
                break;
            }
            if rec_no(r.frn) == target {
                names.push(r.name);
            }
            pos += rec_len;
        }
    }

    println!(
        "  ENUM returned {} record(s) for record# {target}: {names:?}",
        names.len()
    );
    if names.len() >= 2 {
        println!("  -> one record PER NAME: hardlink alternate names ARE enumerated.");
    } else {
        println!("  -> one record PER FILE: alternate hardlink names are NOT in the MFT");
        println!("     enumeration (they'd arrive only via USN change records in M2).");
    }

    let _ = fs::remove_dir_all(&dir);
}

/// P13 (M2 memory): `FSCTL_GET_NTFS_VOLUME_DATA`, a cheap pre-enum size hint. Reserving
/// index capacity from it avoids the pow2 growth history the allocator retains.
fn probe_volume_data(h: *mut c_void, enumerated: Option<u64>) {
    println!("\n== P13: FSCTL_GET_NTFS_VOLUME_DATA (MFT size hint) ==");
    // NTFS_VOLUME_DATA_BUFFER: BytesPerFileRecordSegment@48 (u32), MftValidDataLength@56 (i64).
    let mut out = [0u8; 128];
    let mut returned = 0u32;
    // SAFETY: read-only FSCTL with a sufficiently large output buffer.
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
    if ok == 0 {
        println!("  FAILED, GetLastError = {}", unsafe { GetLastError() });
        return;
    }
    let bytes_per_frs = rd_u32(&out, 48);
    let mft_valid_len = rd_u64(&out, 56);
    println!("  bytes returned            = {returned} (NTFS_VOLUME_DATA_BUFFER = 96)");
    println!("  BytesPerFileRecordSegment = {bytes_per_frs}");
    println!("  MftValidDataLength        = {mft_valid_len}");
    if bytes_per_frs > 0 {
        let est = mft_valid_len / bytes_per_frs as u64;
        println!("  => MFT record segments (upper-bound entry count) = {est}");
        if let Some(n) = enumerated {
            println!(
                "  enumerated entries        = {n}  (ratio est/enum = {:.3}; est is an upper bound)",
                est as f64 / n as f64
            );
        }
    }
}

fn probe_journal(h: *mut c_void) {
    println!("\n== P1: FSCTL_QUERY_USN_JOURNAL ==");
    let mut out = [0u8; 128];
    let mut returned = 0u32;
    // SAFETY: read-only FSCTL with a sufficiently large output buffer.
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_QUERY_USN_JOURNAL,
            ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        println!("  FAILED, GetLastError = {}", unsafe { GetLastError() });
        return;
    }
    println!("  bytes returned = {returned} (V0=56, V1=60, V2>=80)");
    if returned >= 56 {
        println!("  UsnJournalID   = {:#x}", rd_u64(&out, 0));
        println!("  FirstUsn       = {}", rd_u64(&out, 8));
        println!("  NextUsn        = {}", rd_u64(&out, 16));
        println!("  MaxUsn         = {}", rd_u64(&out, 32));
        println!("  MaximumSize    = {}", rd_u64(&out, 40));
    }
    if returned >= 60 {
        println!("  MinSupportedMajorVersion = {}", rd_u16(&out, 56));
        println!("  MaxSupportedMajorVersion = {}", rd_u16(&out, 58));
    }
}

fn probe_enum(h: *mut c_void, dump: bool) {
    println!("\n== P2/P3: FSCTL_ENUM_USN_DATA (requesting V2 via MFT_ENUM_DATA_V1) ==");

    let mut use_v1 = true;
    let mut start_frn: u64 = 0;
    let mut out = vec![0u8; 128 * 1024];
    let mut returned = 0u32;

    let issue = |start: u64, v1: bool, out: &mut [u8], returned: &mut u32| -> i32 {
        let mut input = [0u8; 32];
        input[0..8].copy_from_slice(&start.to_le_bytes()); // StartFileReferenceNumber
        input[8..16].copy_from_slice(&0i64.to_le_bytes()); // LowUsn
        input[16..24].copy_from_slice(&i64::MAX.to_le_bytes()); // HighUsn
        let in_len = if v1 {
            input[24..26].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
            input[26..28].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion
            28u32
        } else {
            24u32
        };
        // SAFETY: read-only FSCTL; buffers valid for the given sizes.
        unsafe {
            DeviceIoControl(
                h,
                FSCTL_ENUM_USN_DATA,
                input.as_ptr().cast(),
                in_len,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                returned,
                ptr::null_mut(),
            )
        }
    };

    let mut ok = issue(start_frn, use_v1, &mut out, &mut returned);
    if ok == 0 {
        let err = unsafe { GetLastError() };
        if use_v1 && err == ERROR_INVALID_PARAMETER {
            println!("  MFT_ENUM_DATA_V1{{MinMajor=Max=2}} REJECTED (err 87). Falling back to V0.");
            use_v1 = false;
            ok = issue(start_frn, use_v1, &mut out, &mut returned);
        }
        if ok == 0 {
            println!("  ENUM FAILED, GetLastError = {}", unsafe {
                GetLastError()
            });
            return;
        }
    }
    println!(
        "  accepted input shape: {}",
        if use_v1 {
            "MFT_ENUM_DATA_V1 (28 bytes)"
        } else {
            "MFT_ENUM_DATA_V0 (24 bytes)"
        }
    );

    let mut total: u64 = 0;
    let mut versions: HashMap<u16, u64> = HashMap::new();
    let mut unique_frns: HashSet<u64> = HashSet::new(); // masked; P9 multiplicity
    let mut short_names: u64 = 0; // P4
    let mut reparse: u64 = 0; // P8
    let mut dirs: u64 = 0;
    let mut root_record: Option<(u64, u64)> = None; // (full_frn, parent_frn) for record #5
    let mut top_level_masked: u64 = 0; // parent record# == 5
    let mut top_level_raw: u64 = 0; // raw parent_frn == 5
    let mut top_level_names: Vec<String> = Vec::new();
    let mut dump_names: HashMap<u64, Vec<String>> = HashMap::new();
    let t0 = Instant::now();

    loop {
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_HANDLE_EOF {
                println!("  ENUM loop FAILED, GetLastError = {err}");
            }
            break;
        }
        if returned <= 8 {
            break;
        }
        start_frn = rd_u64(&out, 0);

        let mut pos = 8usize;
        while pos < returned as usize {
            let (r, rec_len) = parse_record(&out[pos..]);
            if rec_len == 0 {
                break;
            }
            total += 1;
            *versions.entry(r.major).or_insert(0) += 1;
            unique_frns.insert(rec_no(r.frn));
            if r.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                dirs += 1;
            }
            if r.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                reparse += 1;
            }
            if looks_like_short_name(&r.name) {
                short_names += 1;
            }
            if rec_no(r.frn) == ROOT_RECORD {
                root_record = Some((r.frn, r.parent_frn));
            }
            if rec_no(r.parent_frn) == ROOT_RECORD {
                top_level_masked += 1;
                if top_level_names.len() < 12 {
                    top_level_names.push(r.name.clone());
                }
            }
            if r.parent_frn == ROOT_RECORD {
                top_level_raw += 1;
            }
            if dump {
                println!(
                    "  frn={:<18} parent={:<18} v{} attr={:#06x} {}",
                    r.frn, r.parent_frn, r.major, r.attributes, r.name
                );
                dump_names.entry(rec_no(r.frn)).or_default().push(r.name);
            } else if total <= 5 {
                println!(
                    "  sample: frn={} (rec#{}) parent_rec#{} v{} attr={:#06x} name={:?}",
                    r.frn,
                    rec_no(r.frn),
                    rec_no(r.parent_frn),
                    r.major,
                    r.attributes,
                    r.name
                );
            }
            pos += rec_len;
        }
        ok = issue(start_frn, use_v1, &mut out, &mut returned);
    }
    let elapsed = t0.elapsed();

    println!("\n== P7: totals ==");
    println!("  records enumerated = {total}");
    println!("  unique FRNs (files)= {}", unique_frns.len());
    println!(
        "  directories        = {dirs}  (~{:.1}% of entries)",
        pct(dirs, total)
    );
    println!("  enum wall-clock    = {elapsed:.2?}  (debug build, parse+count only)");

    println!("\n== P3: record versions seen ==");
    let mut vs: Vec<_> = versions.iter().collect();
    vs.sort();
    for (v, c) in vs {
        println!("  V{v}: {c} records");
    }

    println!("\n== P8: attributes ==");
    println!("  reparse points = {reparse}");

    println!("\n== P4: 8.3 short-name heuristic ==");
    println!("  names matching NAME~N(.EXT), all-upper = {short_names}");

    println!("\n== P5: root (record #5) in the ENUM stream ==");
    match root_record {
        Some((frn, parent)) => println!(
            "  present: full FRN {frn}, parent record# {}",
            rec_no(parent)
        ),
        None => println!("  NOT present in the stream -> must be synthesized"),
    }
    println!("  records whose parent RECORD# == 5 (masked) = {top_level_masked}");
    println!("  records whose parent FRN    == 5 (raw)    = {top_level_raw}");
    println!(
        "  -> masking to 48 bits is {} to resolve top-level items",
        if top_level_masked > top_level_raw {
            "REQUIRED"
        } else {
            "not required"
        }
    );
    if !top_level_names.is_empty() {
        println!("  sample top-level names: {top_level_names:?}");
    }

    println!("\n== P9: hardlink multiplicity ==");
    let extra = total - unique_frns.len() as u64;
    println!(
        "  records(names) {total} - unique FRNs {} = {extra} extra names",
        unique_frns.len()
    );
    if extra > 0 {
        println!("  -> ENUM yields one record PER NAME; hardlinks appear as multiple records.");
    } else {
        println!("  -> one record per FRN on this volume (no hardlinks observed).");
    }
    if dump {
        println!("  FRNs with multiple names (from --dump):");
        for (frn, names) in dump_names.iter().filter(|(_, n)| n.len() > 1) {
            println!("    rec#{} -> {names:?}", rec_no(*frn));
        }
    }
}

fn looks_like_short_name(name: &str) -> bool {
    let has_tilde_digit = name
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'~' && w[1].is_ascii_digit());
    has_tilde_digit && name == name.to_uppercase()
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vol = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "C:".to_string());
    let dump = args.iter().any(|a| a == "--dump");
    let hardlink_test = args.iter().any(|a| a == "--hardlink-test");
    let voldata = args.iter().any(|a| a == "--voldata");
    let letter = vol.trim_end_matches(['\\', ':']).to_string();

    println!("Everyfind probe - volume {vol}  (dump={dump})");
    println!("\n== P6: open \\\\.\\{letter}: ==");
    let h = match open_path(&format!(r"\\.\{letter}:")) {
        Ok(h) => {
            println!("  opened OK (GENERIC_READ | BACKUP_SEMANTICS)");
            h
        }
        Err(e) => {
            let hint = if e == ERROR_ACCESS_DENIED {
                "  -> ACCESS DENIED: run from an ELEVATED (admin) terminal."
            } else {
                ""
            };
            println!("  open FAILED, GetLastError = {e}\n{hint}");
            std::process::exit(1);
        }
    };

    if voldata {
        probe_volume_data(h, None);
    } else {
        probe_root_frn(&letter);
        probe_journal(h);
        if hardlink_test {
            probe_hardlink(h, &letter);
        } else {
            probe_enum(h, dump);
        }
    }

    // SAFETY: h is a valid handle from open_path.
    unsafe { CloseHandle(h) };
    println!("\ndone.");
}
