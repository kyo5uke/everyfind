//! Index snapshot persistence (M2): the in-memory index plus the journal cursor,
//! serialized with bincode so a restart can *resume* USN tailing instead of
//! re-enumerating the whole MFT.
//!
//! Format: a small `(magic, version)` header, so a stale or foreign file is rejected fast
//! and the caller falls back to a full re-enumeration, followed by `(journal_id, next_usn,
//! index)`. Writes are atomic (temp file + rename).

use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::index::Index;

/// Magic marking an Everyfind snapshot.
const MAGIC: u32 = 0x4e53_6665;
/// Snapshot format version. Bump on any incompatible change to `Index` / `Entry` / arena
/// layout so old files are rejected rather than misread.
/// - v2 (M5): `Index` gained `alloc_clusters` / `cluster_bytes` / `sizes_resolved` (`ef du`).
const FORMAT_VERSION: u32 = 2;

/// A restored snapshot: the index plus the journal cursor to resume tailing from.
pub struct Loaded {
    pub index: Index,
    pub journal_id: u64,
    pub next_usn: u64,
}

/// Serialize `index` + the journal cursor to `path` atomically (temp file + rename).
///
/// Straight into the file, one pass. It used to build the whole thing in memory and then copy
/// it again: `body` held the serialized index and `extend_from_slice` put a second copy in
/// `bytes`, both live at once. For a six-million-entry index that is a transient doubling of
/// several hundred megabytes, taken at the moment this project's known weak point is build RAM,
/// and taken while holding the index read lock.
pub fn save(path: &Path, index: &Index, journal_id: u64, next_usn: u64) -> Result<()> {
    // Appended, not substituted: `--snapshot index.snapshot` and `--snapshot index.bak` both
    // produced `index.tmp` with `with_extension`, so two daemons on different volumes tore each
    // other's writes.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);

    {
        let f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut w = std::io::BufWriter::new(f);
        bincode::serialize_into(&mut w, &(MAGIC, FORMAT_VERSION))
            .context("encoding snapshot header")?;
        bincode::serialize_into(&mut w, &(journal_id, next_usn, index))
            .context("encoding snapshot")?;
        // Flush before the rename, or the rename can publish a short file.
        std::io::Write::flush(&mut w).with_context(|| format!("writing {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Load a snapshot from `path`. Returns an error (so the caller re-enumerates) if the
/// file is missing, has the wrong magic, or a different format version.
pub fn load(path: &Path) -> Result<Loaded> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut cur = Cursor::new(bytes);

    let (magic, version): (u32, u32) =
        bincode::deserialize_from(&mut cur).context("decoding snapshot header")?;
    if magic != MAGIC {
        bail!(
            "{} is not an Everyfind snapshot (magic {magic:#x})",
            path.display()
        );
    }
    if version != FORMAT_VERSION {
        bail!("snapshot format version {version} != {FORMAT_VERSION} (re-enumeration required)");
    }

    let (journal_id, next_usn, index): (u64, u64, Index) =
        bincode::deserialize_from(&mut cur).context("decoding snapshot body")?;
    // The version byte is the only thing standing between a changed `Index` layout and a
    // silently misread index, and it depends on somebody remembering to bump it. bincode
    // carries no schema: two same-width fields swapped, or a field removed, decodes cleanly
    // and stops early, and `Index` has `#[serde(default)]` fields, which makes a short read
    // likelier still. Leftover bytes mean the shape on disk is not the shape in this build,
    // and the honest response is to re-enumerate rather than to serve a wrong index.
    let read = cur.position() as usize;
    let total = cur.get_ref().len();
    if read != total {
        bail!(
            "snapshot decoded {read} of {total} bytes; the on-disk layout is not this build's              (re-enumeration required)"
        );
    }
    Ok(Loaded {
        index,
        journal_id,
        next_usn,
    })
}
