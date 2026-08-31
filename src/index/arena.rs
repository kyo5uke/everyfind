//! A byte arena for concatenated UTF-8 strings.
//!
//! Two instances back the index: the original-name arena (no separators) and the
//! case-folded shadow arena (a `0x00` separator after each name). The separator lets
//! a later "one memmem pass over the whole buffer" search drop in without rebuilding,
//! a query never contains `0x00`, so a match can't span two names.

/// Offsets are `u32` and lengths `u16`. NTFS file names are at most 255 UTF-16 code
/// units (<= 765 UTF-8 bytes), so a name length always fits in `u16`.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct StringArena {
    buf: Vec<u8>,
    separator: bool,
}

impl StringArena {
    /// Create an arena pre-reserving `cap` bytes (`cap == 0` for an empty arena). If
    /// `separator` is set, a `0x00` byte is appended after every pushed string (the fold
    /// arena). Pre-reserving lets a bulk build make one allocation with no pow2 growth
    /// history for the allocator to retain (M2 memory; see `build`).
    pub fn with_capacity(separator: bool, cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            separator,
        }
    }

    /// Append `bytes`; return `(offset, len)`. `len` excludes any separator byte.
    pub fn push(&mut self, bytes: &[u8]) -> (u32, u16) {
        debug_assert!(
            bytes.len() <= u16::MAX as usize,
            "name longer than u16::MAX"
        );
        debug_assert!(
            self.buf.len() <= u32::MAX as usize,
            "arena exceeds u32 offsets"
        );
        let off = self.buf.len() as u32;
        let len = bytes.len() as u16;
        self.buf.extend_from_slice(bytes);
        if self.separator {
            self.buf.push(0);
        }
        (off, len)
    }

    /// Borrow the bytes previously stored at `(off, len)`.
    pub fn get(&self, off: u32, len: u16) -> &[u8] {
        let start = off as usize;
        &self.buf[start..start + len as usize]
    }

    /// Borrow the whole backing buffer (names + `0x00` separators). Used by the single-pass
    /// "one memmem over the entire fold buffer" search (see `Index::search`).
    pub fn buf(&self) -> &[u8] {
        &self.buf
    }

    /// Total bytes held (including separators), for memory accounting.
    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_get_roundtrip() {
        let mut a = StringArena::with_capacity(false, 0);
        let (o1, l1) = a.push(b"hello");
        let (o2, l2) = a.push(b"world");
        assert_eq!(a.get(o1, l1), b"hello");
        assert_eq!(a.get(o2, l2), b"world");
        assert_eq!(a.byte_len(), 10); // no separators
    }

    #[test]
    fn separator_is_appended_but_excluded_from_len() {
        let mut a = StringArena::with_capacity(true, 0);
        let (o, l) = a.push(b"abc");
        assert_eq!(a.get(o, l), b"abc");
        assert_eq!(l, 3);
        assert_eq!(a.byte_len(), 4); // "abc\0"
    }
}
