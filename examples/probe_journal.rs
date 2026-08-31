//! Everyfind M2 probe: verifies real USN *journal-read* behavior before the tailing
//! backend is written. Run in an ELEVATED terminal (opening `\\.\X:` needs admin):
//!
//! ```text
//! cargo run --example probe_journal -- C:              # P10 (record shape) + P12 (poll)
//! cargo run --example probe_journal -- T: --overflow   # + P11 (journal overflow): TINY fixture only
//! cargo run --example probe_journal -- C: --p10-only    # just the record dump
//! ```
//!
//! Answers:
//!   P10 READ_USN_JOURNAL return unit; REASON OR reality; RENAME OLD/NEW ordering + payloads;
//!       dir-rename and cross-dir move shapes; HARD_LINK_CHANGE shape.
//!   P11 with a TINY journal (-TinyJournal fixture): the exact error code when reading from a
//!       trimmed USN (expect ERROR_JOURNAL_ENTRY_DELETED = 1181); journal_id stable across wrap.
//!   P12 idle poll cost (non-blocking reads) vs. blocking wait (BytesToWaitFor); no loss across
//!       a sleep gap.
//!
//! Writes performed: ordinary file create/write/rename/delete under a scratch dir on the target
//! volume (cleaned up). NO volume-write FSCTLs are issued (journal creation is left to the
//! fixture script / an explicit `ef-index --create-journal`).

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::System::IO::DeviceIoControl;

const GENERIC_READ: u32 = 0x8000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;

/// Cached accepted input length for READ_USN_JOURNAL (0 = not yet detected). The struct
/// size is version-dependent (V0=40, V1=44 packed / 48 padded); we detect once at runtime.
static READ_LEN: AtomicU32 = AtomicU32::new(0);

/// FRN = sequence(16) << 48 | record#(48); identity/parent compared on the record number.
const FRN_MASK: u64 = (1u64 << 48) - 1;

/// USN_REASON_* bits, in ascending value order (for a readable decode).
const REASONS: &[(u32, &str)] = &[
    (0x0000_0001, "DATA_OVERWRITE"),
    (0x0000_0002, "DATA_EXTEND"),
    (0x0000_0004, "DATA_TRUNCATION"),
    (0x0000_0010, "NAMED_DATA_OVERWRITE"),
    (0x0000_0020, "NAMED_DATA_EXTEND"),
    (0x0000_0040, "NAMED_DATA_TRUNCATION"),
    (0x0000_0100, "FILE_CREATE"),
    (0x0000_0200, "FILE_DELETE"),
    (0x0000_0400, "EA_CHANGE"),
    (0x0000_0800, "SECURITY_CHANGE"),
    (0x0000_1000, "RENAME_OLD_NAME"),
    (0x0000_2000, "RENAME_NEW_NAME"),
    (0x0000_4000, "INDEXABLE_CHANGE"),
    (0x0000_8000, "BASIC_INFO_CHANGE"),
    (0x0001_0000, "HARD_LINK_CHANGE"),
    (0x0002_0000, "COMPRESSION_CHANGE"),
    (0x0004_0000, "ENCRYPTION_CHANGE"),
    (0x0008_0000, "OBJECT_ID_CHANGE"),
    (0x0010_0000, "REPARSE_POINT_CHANGE"),
    (0x0020_0000, "STREAM_CHANGE"),
    (0x0040_0000, "TRANSACTED_CHANGE"),
    (0x0080_0000, "INTEGRITY_CHANGE"),
    (0x8000_0000, "CLOSE"),
];

fn decode_reason(reason: u32) -> String {
    let names: Vec<&str> = REASONS
        .iter()
        .filter(|(bit, _)| reason & bit != 0)
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        format!("{reason:#010x}")
    } else {
        format!("{reason:#010x} [{}]", names.join("|"))
    }
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

struct JournalData {
    journal_id: u64,
    first_usn: u64,
    next_usn: u64,
}

struct EventRec {
    usn: u64,
    frn: u64,
    parent_frn: u64,
    reason: u32,
    attributes: u32,
    name: String,
}

/// Parse one USN_RECORD_V2 from the front of `rec`; return (record, length).
/// V2: RecordLength@0, Major@4, FRN@8, ParentFRN@16, Usn@24, TimeStamp@32, Reason@40,
///     SourceInfo@44, SecurityId@48, FileAttributes@52, NameLen@56, NameOff@58.
fn parse_v2(rec: &[u8]) -> Option<(EventRec, usize)> {
    if rec.len() < 60 {
        return None;
    }
    let record_length = rd_u32(rec, 0) as usize;
    if record_length < 60 || record_length > rec.len() {
        return None;
    }
    let name_len = rd_u16(rec, 56) as usize;
    let name_off = rd_u16(rec, 58) as usize;
    if name_off + name_len > record_length {
        return None;
    }
    let name_u16: Vec<u16> = rec[name_off..name_off + name_len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some((
        EventRec {
            usn: rd_u64(rec, 24),
            frn: rd_u64(rec, 8) & FRN_MASK,
            parent_frn: rd_u64(rec, 16) & FRN_MASK,
            reason: rd_u32(rec, 40),
            attributes: rd_u32(rec, 52),
            name: String::from_utf16_lossy(&name_u16),
        },
        record_length,
    ))
}

fn open_volume(letter: &str) -> Result<*mut c_void, u32> {
    let path = format!(r"\\.\{letter}:");
    let wide: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: valid NUL-terminated wide path; other args are constants.
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

fn query_journal(h: *mut c_void) -> Result<JournalData, u32> {
    let mut out = [0u8; 80];
    let mut returned = 0u32;
    // SAFETY: read-only FSCTL, buffer large enough for USN_JOURNAL_DATA_V2.
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
        return Err(unsafe { GetLastError() });
    }
    Ok(JournalData {
        journal_id: rd_u64(&out, 0),
        first_usn: rd_u64(&out, 8),
        next_usn: rd_u64(&out, 16),
    })
}

/// Issue one FSCTL_READ_USN_JOURNAL (V2 records pinned via Min=Max=2). Returns the raw
/// DeviceIoControl result (0 = failure; call GetLastError) and the bytes returned; the
/// output lands in `out` (first 8 bytes = next USN).
///
/// The accepted input length is version-dependent, so we detect it once (trying V1 packed,
/// V1 padded, then V0) and cache it. A non-`INVALID_PARAMETER` failure (e.g. a trimmed USN
/// -> `JOURNAL_ENTRY_DELETED`) is returned immediately; the caller inspects GetLastError.
fn read_journal(
    h: *mut c_void,
    start_usn: u64,
    journal_id: u64,
    bytes_to_wait_for: u64,
    timeout_100ns: i64,
    out: &mut [u8],
) -> (i32, u32) {
    // READ_USN_JOURNAL_DATA_V1:
    //   StartUsn(i64)@0, ReasonMask(u32)@8, ReturnOnlyOnClose(u32)@12, Timeout(u64)@16,
    //   BytesToWaitFor(u64)@24, UsnJournalID(u64)@32, MinMajor(u16)@40, MaxMajor(u16)@42.
    // Bytes 44..48 stay zero (padding for the 48-byte attempt); a 40-byte length omits the
    // version words, which drives the journal's default (V2 here); parse_v2 handles that.
    let mut input = [0u8; 48];
    input[0..8].copy_from_slice(&start_usn.to_le_bytes());
    input[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // all reasons
    input[12..16].copy_from_slice(&0u32.to_le_bytes()); // ReturnOnlyOnClose = false
    input[16..24].copy_from_slice(&timeout_100ns.to_le_bytes());
    input[24..32].copy_from_slice(&bytes_to_wait_for.to_le_bytes());
    input[32..40].copy_from_slice(&journal_id.to_le_bytes());
    input[40..42].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
    input[42..44].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion

    let cached = READ_LEN.load(Ordering::Relaxed);
    let mut last = (0i32, 0u32);
    for &len in &[44u32, 48, 40] {
        if cached != 0 && len != cached {
            continue;
        }
        let mut returned = 0u32;
        // SAFETY: read-only FSCTL; input/output buffers valid for the given sizes.
        let ok = unsafe {
            DeviceIoControl(
                h,
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
            READ_LEN.store(len, Ordering::Relaxed);
            return (ok, returned);
        }
        last = (ok, returned);
        if unsafe { GetLastError() } != ERROR_INVALID_PARAMETER {
            return last; // real error: caller inspects GetLastError
        }
    }
    last
}

/// Drain all records with USN >= `start_usn` into a vec (non-blocking), returning the
/// records and the final next-USN cursor.
fn drain_events(h: *mut c_void, start_usn: u64, journal_id: u64) -> (Vec<EventRec>, u64) {
    let mut events = Vec::new();
    let mut cursor = start_usn;
    let mut out = vec![0u8; 256 * 1024];
    loop {
        let (ok, returned) = read_journal(h, cursor, journal_id, 0, 0, &mut out);
        if ok == 0 {
            let err = unsafe { GetLastError() };
            println!("  READ failed at usn {cursor}: GetLastError = {err}");
            break;
        }
        if returned <= 8 {
            break; // only the 8-byte next-USN header: caught up
        }
        let next = rd_u64(&out, 0);
        let mut pos = 8usize;
        while pos + 60 <= returned as usize {
            match parse_v2(&out[pos..returned as usize]) {
                Some((e, len)) => {
                    events.push(e);
                    pos += len;
                }
                None => break,
            }
        }
        if next == cursor {
            break; // no forward progress
        }
        cursor = next;
    }
    (events, cursor)
}

struct Scratch {
    root: PathBuf,
}
impl Scratch {
    fn new(letter: &str) -> std::io::Result<Self> {
        let root = PathBuf::from(format!(r"{letter}:\ef_probe_journal_tmp"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    fn p(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// P10: run a scripted sequence of ordinary FS operations, then dump every USN record the
/// journal produced, in order, revealing return unit, REASON OR-ing, and RENAME pairing.
fn probe_p10(h: *mut c_void, letter: &str) {
    println!("\n== P10: READ_USN_JOURNAL record shape / REASON OR / RENAME pairing ==");
    let j = match query_journal(h) {
        Ok(j) => j,
        Err(e) => {
            println!("  QUERY_USN_JOURNAL failed (err {e}) - does {letter}: have a journal?");
            return;
        }
    };
    println!(
        "  journal_id={:#x} first_usn={} next_usn={} (reading from next_usn)",
        j.journal_id, j.first_usn, j.next_usn
    );

    let scratch = match Scratch::new(letter) {
        Ok(s) => s,
        Err(e) => {
            println!("  could not create scratch dir on {letter}: {e}");
            return;
        }
    };
    let start = j.next_usn;

    // Scripted operations, each labeled so the dump below can be correlated to it.
    use std::io::Write;
    let s = &scratch;
    let label = |t: &str| println!("    - {t}");
    println!("\n  operations performed (in order):");

    label("create file f1.txt");
    let _ = std::fs::write(s.p("f1.txt"), b"hello");

    label("append to f1.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(s.p("f1.txt")) {
        let _ = f.write_all(b" world");
    }

    label("rename f1.txt -> f2.txt (same dir)");
    let _ = std::fs::rename(s.p("f1.txt"), s.p("f2.txt"));

    label("create dir sub");
    let _ = std::fs::create_dir(s.p("sub"));

    label("move f2.txt -> sub\\f2.txt (cross-dir)");
    let _ = std::fs::rename(s.p("f2.txt"), s.p("sub").join("f2.txt"));

    label("rename dir sub -> sub2");
    let _ = std::fs::rename(s.p("sub"), s.p("sub2"));

    label("delete sub2\\f2.txt");
    let _ = std::fs::remove_file(s.p("sub2").join("f2.txt"));

    label("delete dir sub2");
    let _ = std::fs::remove_dir(s.p("sub2"));

    // Give NTFS a moment to flush CLOSE records for the ops above.
    std::thread::sleep(Duration::from_millis(300));

    let (events, cursor) = drain_events(h, start, j.journal_id);
    println!(
        "\n  {} record(s) read from usn {start} to {cursor} (each row = one USN_RECORD_V2):",
        events.len()
    );
    for e in &events {
        println!(
            "    usn={:<10} frn#{:<8} parent#{:<8} attr={:#06x} {:<22} {}",
            e.usn,
            e.frn,
            e.parent_frn,
            e.attributes,
            e.name,
            decode_reason(e.reason)
        );
    }
    println!("\n  Look for: (a) return unit = 8-byte USN header + packed V2 records;");
    println!("            (b) whether CREATE and CLOSE are one coalesced record or separate;");
    println!(
        "            (c) RENAME emitted as OLD_NAME then NEW_NAME, and which parent each carries;"
    );
    println!("            (d) dir-rename appears once for the dir (children not re-emitted).");
}

/// P12: idle poll cost (non-blocking) vs. a single blocking wait; and no-loss across a gap.
fn probe_p12(h: *mut c_void, letter: &str) {
    println!("\n== P12: poll cadence vs. blocking wait ==");
    let j = match query_journal(h) {
        Ok(j) => j,
        Err(e) => {
            println!("  QUERY failed (err {e})");
            return;
        }
    };

    // (a) Non-blocking idle poll cost: many empty reads back-to-back.
    let mut out = vec![0u8; 64 * 1024];
    let n = 2000;
    let t0 = Instant::now();
    for _ in 0..n {
        let _ = read_journal(h, j.next_usn, j.journal_id, 0, 0, &mut out);
    }
    let per = t0.elapsed() / n;
    println!(
        "  (a) {n} non-blocking empty READs: {:.2?} total, {per:.2?}/read - idle CPU cost of polling",
        t0.elapsed()
    );

    // (b) No loss across a sleep gap: create files, sleep, then a single drain catches them.
    if let Ok(scratch) = Scratch::new(letter) {
        let start = query_journal(h).map(|q| q.next_usn).unwrap_or(j.next_usn);
        for i in 0..5 {
            let _ = std::fs::write(scratch.p(&format!("gap_{i}.txt")), b"x");
        }
        std::thread::sleep(Duration::from_millis(500));
        let (events, _) = drain_events(h, start, j.journal_id);
        let creates = events
            .iter()
            .filter(|e| e.name.starts_with("gap_") && e.reason & 0x100 != 0)
            .count();
        println!(
            "  (b) 5 files created, drained after a 500ms gap: {creates} CREATE record(s) seen (no loss expected)"
        );
    }

    // (c) Blocking wait: block in READ (BytesToWaitFor=1) while a helper thread creates a file.
    if let Ok(scratch) = Scratch::new(letter) {
        let start = query_journal(h).map(|q| q.next_usn).unwrap_or(j.next_usn);
        let file = scratch.p("blocking_wait.txt");
        let file2 = file.clone();
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            let _ = std::fs::write(&file2, b"wake");
        });
        // Timeout is a negative 100ns interval (relative). -30s guards against a hang.
        let timeout = -(30i64 * 10_000_000);
        let t = Instant::now();
        let (ok, returned) = read_journal(h, start, j.journal_id, 1, timeout, &mut out);
        let waited = t.elapsed();
        let _ = waker.join();
        if ok != 0 && returned > 8 {
            println!(
                "  (c) blocking READ (BytesToWaitFor=1) returned in {waited:.2?} after the write - \
                 wait-mode can drive idle CPU to ~0"
            );
        } else {
            println!(
                "  (c) blocking READ returned ok={ok} bytes={returned} in {waited:.2?} \
                 (GetLastError={})",
                unsafe { GetLastError() }
            );
        }
        // Drain the just-created record so it doesn't leak into later probes.
        let _ = std::fs::remove_file(&file);
    }
}

/// P11: overflow a TINY journal, then confirm the exact error reading from a trimmed USN.
/// Destructive churn: run ONLY on a `-TinyJournal` fixture volume.
fn probe_p11(h: *mut c_void, letter: &str) {
    println!("\n== P11: journal overflow (TINY-journal fixture only) ==");
    let j0 = match query_journal(h) {
        Ok(j) => j,
        Err(e) => {
            println!("  QUERY failed (err {e})");
            return;
        }
    };
    println!(
        "  before: journal_id={:#x} first_usn={} next_usn={}",
        j0.journal_id, j0.first_usn, j0.next_usn
    );
    let stale_usn = j0.next_usn; // will become < first_usn after we wrap the journal

    let Ok(scratch) = Scratch::new(letter) else {
        println!("  could not create scratch dir on {letter}:");
        return;
    };
    println!("  churning create+write+delete cycles to advance first_usn past {stale_usn} ...");
    let mut cycles = 0u32;
    let max_cycles = 200_000u32;
    loop {
        let p = scratch.p(&format!("churn_{}.tmp", cycles % 64));
        let _ = std::fs::write(&p, b"0123456789ABCDEF0123456789ABCDEF");
        let _ = std::fs::remove_file(&p);
        cycles += 1;
        if cycles.is_multiple_of(2000) {
            match query_journal(h) {
                Ok(j) if j.first_usn > stale_usn => {
                    println!(
                        "  wrapped after {cycles} cycles: first_usn advanced {} -> {} (journal_id={:#x})",
                        j0.first_usn, j.first_usn, j.journal_id
                    );
                    break;
                }
                _ => {}
            }
        }
        if cycles >= max_cycles {
            println!(
                "  gave up after {max_cycles} cycles - journal may be too large; use -TinyJournal"
            );
            return;
        }
    }

    let j1 = query_journal(h).unwrap_or(JournalData {
        journal_id: j0.journal_id,
        first_usn: 0,
        next_usn: 0,
    });
    println!(
        "  journal_id stable across wrap? {} ({:#x} -> {:#x})",
        j1.journal_id == j0.journal_id,
        j0.journal_id,
        j1.journal_id
    );

    // Now read from the stale (trimmed) USN and capture the exact error.
    let mut out = vec![0u8; 64 * 1024];
    let (ok, returned) = read_journal(h, stale_usn, j0.journal_id, 0, 0, &mut out);
    if ok == 0 {
        let err = unsafe { GetLastError() };
        println!(
            "  READ from trimmed usn {stale_usn} -> GetLastError = {err} {}",
            if err == ERROR_JOURNAL_ENTRY_DELETED {
                "(ERROR_JOURNAL_ENTRY_DELETED - CONFIRMED; recovery = full re-enum)"
            } else {
                "(UNEXPECTED - investigate before wiring recovery)"
            }
        );
    } else {
        println!(
            "  READ from trimmed usn {stale_usn} unexpectedly SUCCEEDED (bytes={returned}) - \
             journal larger than expected?"
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vol = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "C:".to_string());
    let letter = vol.trim_end_matches(['\\', ':']).to_string();
    let overflow = args.iter().any(|a| a == "--overflow");
    let p10_only = args.iter().any(|a| a == "--p10-only");

    println!("Everyfind journal probe - volume {vol}");
    let h = match open_volume(&letter) {
        Ok(h) => h,
        Err(e) => {
            let hint = if e == ERROR_ACCESS_DENIED {
                " -> ACCESS DENIED: run from an ELEVATED (admin) terminal."
            } else {
                ""
            };
            println!("open \\\\.\\{letter}: FAILED, GetLastError = {e}{hint}");
            std::process::exit(1);
        }
    };

    probe_p10(h, &letter);
    if !p10_only {
        probe_p12(h, &letter);
    }
    if overflow {
        probe_p11(h, &letter);
    } else {
        println!("\n(P11 skipped - pass --overflow on a -TinyJournal fixture to run it.)");
    }

    // SAFETY: h is a valid handle from open_volume.
    unsafe { CloseHandle(h) };
    println!("\ndone.");
}
