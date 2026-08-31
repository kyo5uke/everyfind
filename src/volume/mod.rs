//! Volume abstraction: the boundary between platform-specific USN/MFT parsing and
//! the pure index logic.
//!
//! [`UsnVolume`] yields a stream of normalized [`RawRecord`]s. The real backend
//! ([`Win32Volume`], added at M1 step 4) parses `USN_RECORD` buffers from
//! `DeviceIoControl`; `FakeVolume` injects records directly for tests (gated behind
//! the `test-util` feature, a test double, not part of the public API).

#[cfg(any(test, feature = "test-util"))]
pub mod fake;
pub mod win32;

#[cfg(any(test, feature = "test-util"))]
pub use fake::{FakeEvent, FakeRecord, FakeVolume};
pub use win32::Win32Volume;

/// Standard Win32 file attribute bits we care about, defined locally so the index
/// layer does not depend on `windows-sys`. These match the Win32 constants.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// `USN_REASON_*` bits (from `FSCTL_READ_USN_JOURNAL`) that the index acts on. Measured
/// M2/P10: reasons are cumulative (OR-ed) across records for one open handle until `CLOSE`,
/// so event apply is idempotent.
pub mod reason {
    /// A file/dir was created.
    pub const FILE_CREATE: u32 = 0x0000_0100;
    /// The named data stream grew.
    pub const DATA_EXTEND: u32 = 0x0000_0002;
    /// The named data stream shrank.
    pub const DATA_TRUNCATION: u32 = 0x0000_0004;
    /// Data was written over existing data. Allocation can change (sparse, compressed), so it
    /// counts as a size event.
    pub const DATA_OVERWRITE: u32 = 0x0000_0001;
    /// The reasons that can change a file's *allocated* size, and so the only ones worth a
    /// stat. Everything else (a security change, a basic-info change, a close, the pre-image
    /// of a rename) leaves the allocation exactly where it was.
    pub const SIZE_AFFECTING: u32 = FILE_CREATE | DATA_EXTEND | DATA_TRUNCATION | DATA_OVERWRITE;
    /// A file/dir was deleted. The only reason that removes an entry.
    pub const FILE_DELETE: u32 = 0x0000_0200;
    /// The pre-image of a rename (old name + old parent). **Skipped**: the following
    /// `RENAME_NEW_NAME` carries the authoritative new name/parent.
    pub const RENAME_OLD_NAME: u32 = 0x0000_1000;
    /// The post-image of a rename/move (new name + new parent).
    pub const RENAME_NEW_NAME: u32 = 0x0000_2000;
    /// A hardlink was added/removed. Ignored in M2 v1 (known limitation).
    pub const HARD_LINK_CHANGE: u32 = 0x0001_0000;
    /// The handle that produced the accumulated reasons was closed.
    pub const CLOSE: u32 = 0x8000_0000;
}

/// USN journal metadata (from `FSCTL_QUERY_USN_JOURNAL`). M2 uses `next_usn` as the
/// tail cursor; M1 only reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: u64,
    pub next_usn: u64,
}

/// One MFT/USN record, normalized. `name_utf16` borrows from the backend's buffer,
/// so records are consumed via a callback rather than collected.
#[derive(Debug, Clone, Copy)]
pub struct RawRecord<'a> {
    /// File reference number (NTFS: 64-bit; V3/`FILE_ID_128` is pinned out; see probe P3).
    pub frn: u64,
    /// Parent directory's FRN. Always a directory (NTFS has no directory hardlinks).
    pub parent_frn: u64,
    /// File name as a UTF-16 slice (as NTFS stores it).
    pub name_utf16: &'a [u16],
    /// Win32 file attributes (`FILE_ATTRIBUTE_*`).
    pub attributes: u32,
}

/// One USN change-journal event (from `FSCTL_READ_USN_JOURNAL`). Like [`RawRecord`] but
/// carries the record's `usn` cursor and its `reason` bitmask (see [`reason`]).
#[derive(Debug, Clone, Copy)]
pub struct UsnEvent<'a> {
    /// File reference number of the changed file (masked to the low 48 bits).
    pub frn: u64,
    /// Parent FRN as of this record (a move's `RENAME_NEW_NAME` carries the new parent).
    pub parent_frn: u64,
    /// This record's USN (its position in the journal; strictly increasing).
    pub usn: u64,
    /// `USN_REASON_*` bitmask, cumulative for the open handle until `CLOSE`.
    pub reason: u32,
    /// File name as a UTF-16 slice.
    pub name_utf16: &'a [u16],
    /// Win32 file attributes (`FILE_ATTRIBUTE_*`).
    pub attributes: u32,
}

/// A discontinuity that requires a full re-enumeration to recover (the M2 safety
/// principle). Non-discontinuity failures are carried in [`JournalError::Other`].
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// `start_usn` was trimmed away (journal wrapped past it): `ERROR_JOURNAL_ENTRY_DELETED`
    /// (1181), measured P11. The caller must re-enumerate from a fresh snapshot.
    #[error("USN journal entry deleted (trimmed past start_usn); re-enumeration required")]
    EntryDeleted,
    /// The journal was deleted and recreated (its id no longer matches): re-enumerate.
    #[error("USN journal id changed (expected {expected:#x}, found {found:#x}); re-enumeration required")]
    IdChanged { expected: u64, found: u64 },
    /// Any other failure (I/O, unexpected `GetLastError`).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A source of MFT records for a single NTFS volume.
pub trait UsnVolume {
    /// Query the USN journal metadata.
    fn journal_info(&self) -> anyhow::Result<JournalInfo>;

    /// An upper-bound estimate of the entry count, cheap to obtain before enumeration
    /// (from the MFT size). Used to pre-reserve index capacity so the build has no pow2
    /// growth history for the allocator to retain. `None` when unavailable.
    fn size_hint(&self) -> Option<u64> {
        None
    }

    /// Bytes per NTFS cluster, for converting the cluster counts from [`enum_sizes`] to bytes.
    /// The real backend reads it from `FSCTL_GET_NTFS_VOLUME_DATA`; the default is the NTFS
    /// default (4096) used by `FakeVolume` and any backend that does not override it.
    fn cluster_bytes(&self) -> u32 {
        4096
    }

    /// **M5 live size refresh.** The *current* allocated cluster count (+ truncated) of one file,
    /// or `None` if it cannot be stat'd (deleted in a race, or no size backend). The watch loop
    /// calls this after applying a create / data-extend / rename so `ef du` stays live without
    /// re-reading the whole MFT. Both the file's `frn` and its reconstructed `path` are supplied:
    /// the real backend stats **by path** (the `frn` we carry is masked to 48 bits, so it is not a
    /// valid `OpenFileById` reference, probe P5), while `FakeVolume` keys off `frn`. Fail-soft:
    /// `None` leaves the entry's size unchanged until the next full enumeration. Default `None`.
    fn alloc_clusters(&self, _frn: u64, _path: &str) -> Option<(u32, bool)> {
        None
    }

    /// **M5 size pass.** Yield `(frn, allocated_clusters, truncated)` for every file that carries
    /// an on-disk size, so the index can attach a size to the matching FRN. `allocated_clusters`
    /// is the on-disk cluster count (data-run counted: exact for sparse/compressed, probe P19/T:);
    /// `truncated` is set when the true count saturated `u32::MAX` (a > 16 TiB file). This is an
    /// **additive, fail-soft** pass: it is keyed by FRN into the already-built index, and a record
    /// it cannot parse is simply not yielded (that entry keeps size 0). Default no-op: a backend
    /// without size support (or a caller that skips sizing) leaves every entry at 0.
    fn enum_sizes(&self, _sink: &mut dyn FnMut(u64, u32, bool)) -> anyhow::Result<()> {
        Ok(())
    }

    /// Enumerate every MFT record, invoking `sink` once per record. Records may
    /// arrive with parents not yet seen (orphans); the index builder resolves
    /// parents in a second pass.
    fn enum_records(&mut self, sink: &mut dyn FnMut(RawRecord<'_>)) -> anyhow::Result<()>;

    /// Drain journal events with USN >= `start_usn` for the journal identified by
    /// `journal_id`, invoking `sink` per event in strict USN order, and return the
    /// next-USN cursor to resume from. **Non-blocking**: returns once caught up.
    ///
    /// Returns [`JournalError::EntryDeleted`] if `start_usn` was trimmed and
    /// [`JournalError::IdChanged`] if `journal_id` no longer matches the volume, either
    /// means the caller must re-enumerate.
    fn read_journal(
        &self,
        start_usn: u64,
        journal_id: u64,
        sink: &mut dyn FnMut(UsnEvent<'_>),
    ) -> Result<u64, JournalError>;
}
