//! Path reconstruction: nested paths, the root, orphans, and reparse flags.

mod common;

use common::{sample_volume, DRIVE};
use everyfind::index::{build_from_volume, flags};

#[test]
fn nested_path_is_reconstructed_from_parent_chain() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(
        idx.path(common::KERNEL32),
        r"C:\Windows\System32\kernel32.dll"
    );
    assert_eq!(idx.path(common::SYSTEM32), r"C:\Windows\System32");
    assert_eq!(idx.path(common::WINDOWS), r"C:\Windows");
}

#[test]
fn top_level_items_sit_directly_under_the_drive_root() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(idx.path(common::README_TXT), r"C:\readme.txt");
}

#[test]
fn root_path_is_the_drive_root() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    assert_eq!(idx.path(idx.root()), r"C:\");
    assert_eq!(idx.root(), common::ROOT);
}

#[test]
fn orphan_path_uses_the_orphan_prefix_not_a_fake_drive_path() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Must NOT be reconstructed as `C:\orphan_file.txt` (that path does not exist).
    assert_eq!(idx.path(common::ORPHAN), r"<orphan>\orphan_file.txt");
    assert_ne!(
        idx.entries()[common::ORPHAN as usize].flags & flags::IS_ORPHAN,
        0
    );
}

#[test]
fn hardlink_is_kept_as_two_independent_entries() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    // Same FRN (12) as kernel32.dll, but a separate entry with its own path.
    assert_eq!(
        idx.path(common::KERNEL32),
        r"C:\Windows\System32\kernel32.dll"
    );
    assert_eq!(idx.path(common::K32LINK), r"C:\k32link.dll");
}

#[test]
fn reparse_flag_and_dir_flag_are_recorded() {
    let mut vol = sample_volume();
    let idx = build_from_volume(&mut vol, DRIVE).unwrap();

    let e = idx.entries()[common::SYMLINK_DIR as usize];
    assert_ne!(e.flags & flags::REPARSE_POINT, 0);
    assert_ne!(e.flags & flags::IS_DIR, 0);
}
