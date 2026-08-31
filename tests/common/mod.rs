//! Shared fixtures for integration tests.
#![allow(dead_code)]

use everyfind::volume::{FakeRecord, FakeVolume};

/// Drive letter used by the sample volume.
pub const DRIVE: char = 'C';

/// A small but representative NTFS-like tree. Enumeration order == internal id, so
/// tests can reference entries by the ids documented here:
///
/// ```text
/// id frn parent  name                 kind
///  0   5    5    ""                   root
///  1  10    5    Windows              dir
///  2  11   10    System32             dir
///  3  12   11    kernel32.dll         file
///  4  13    5    readme.txt           file
///  5  14    5    README.MD            file
///  6  12    5    k32link.dll          file  (hardlink: shares FRN 12 with id 3)
///  7  20  999    orphan_file.txt      file  (parent 999 absent -> orphan)
///  8  30    5    symlink_dir          dir   (reparse point)
/// ```
pub fn sample_volume() -> FakeVolume {
    FakeVolume::new(vec![
        FakeRecord::dir(5, 5, ""),
        FakeRecord::dir(10, 5, "Windows"),
        FakeRecord::dir(11, 10, "System32"),
        FakeRecord::file(12, 11, "kernel32.dll"),
        FakeRecord::file(13, 5, "readme.txt"),
        FakeRecord::file(14, 5, "README.MD"),
        FakeRecord::file(12, 5, "k32link.dll"),
        FakeRecord::file(20, 999, "orphan_file.txt"),
        FakeRecord::dir(30, 5, "symlink_dir").reparse(),
    ])
}

// Entry ids for readability in assertions.
pub const ROOT: u32 = 0;
pub const WINDOWS: u32 = 1;
pub const SYSTEM32: u32 = 2;
pub const KERNEL32: u32 = 3;
pub const README_TXT: u32 = 4;
pub const README_MD: u32 = 5;
pub const K32LINK: u32 = 6;
pub const ORPHAN: u32 = 7;
pub const SYMLINK_DIR: u32 = 8;

/// Sort a search result for order-independent comparison.
pub fn sorted(mut v: Vec<u32>) -> Vec<u32> {
    v.sort_unstable();
    v
}
