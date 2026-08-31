//! In-memory [`UsnVolume`] for unit/integration tests. Lets tests inject arbitrary
//! record trees for [`enum_records`](UsnVolume::enum_records) (root, orphans, hardlinks,
//! reparse points) and arbitrary journal events for
//! [`read_journal`](UsnVolume::read_journal) (create/delete/rename, plus wrap/id-change
//! discontinuities).

use std::collections::HashMap;

use super::{
    reason, JournalError, JournalInfo, RawRecord, UsnEvent, UsnVolume, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT,
};

/// A single fake record. Construct via [`FakeRecord::dir`] / [`FakeRecord::file`],
/// optionally chaining [`FakeRecord::reparse`] / [`FakeRecord::size`].
#[derive(Debug, Clone)]
pub struct FakeRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub attributes: u32,
    /// M5 size pass: `Some((allocated_clusters, truncated))` if this record carries a size, so
    /// [`FakeVolume::enum_sizes`] yields it. `None` = the size pass skipped it (stays 0), which is
    /// how tests exercise the fail-soft "unresolved" case.
    pub size: Option<(u32, bool)>,
}

impl FakeRecord {
    /// A directory record.
    pub fn dir(frn: u64, parent_frn: u64, name: &str) -> Self {
        Self {
            frn,
            parent_frn,
            name: name.to_owned(),
            attributes: FILE_ATTRIBUTE_DIRECTORY,
            size: None,
        }
    }

    /// A regular file record.
    pub fn file(frn: u64, parent_frn: u64, name: &str) -> Self {
        Self {
            frn,
            parent_frn,
            name: name.to_owned(),
            attributes: 0,
            size: None,
        }
    }

    /// Mark this record as a reparse point (symlink/junction).
    #[must_use]
    pub fn reparse(mut self) -> Self {
        self.attributes |= FILE_ATTRIBUTE_REPARSE_POINT;
        self
    }

    /// Attach an allocated size (in clusters) so the M5 size pass yields it for this FRN.
    #[must_use]
    pub fn size(mut self, alloc_clusters: u32) -> Self {
        self.size = Some((alloc_clusters, false));
        self
    }

    /// Attach a size that is flagged as truncated (saturated `u32::MAX`, i.e. a > 16 TiB file).
    #[must_use]
    pub fn size_truncated(mut self, alloc_clusters: u32) -> Self {
        self.size = Some((alloc_clusters, true));
        self
    }
}

/// A single fake USN journal event. Construct via [`FakeEvent::create`] /
/// [`FakeEvent::delete`] / [`FakeEvent::rename_old`] / [`FakeEvent::rename_new`], then
/// chain [`FakeEvent::dir`] / [`FakeEvent::reparse`] / [`FakeEvent::close`] /
/// [`FakeEvent::also`] to mirror real cumulative-reason records (measured P10).
#[derive(Debug, Clone)]
pub struct FakeEvent {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: u64,
    pub reason: u32,
    pub name: String,
    pub attributes: u32,
}

impl FakeEvent {
    fn base(usn: u64, frn: u64, parent_frn: u64, name: &str, reason: u32) -> Self {
        Self {
            frn,
            parent_frn,
            usn,
            reason,
            name: name.to_owned(),
            attributes: 0,
        }
    }

    /// A `FILE_CREATE` event.
    pub fn create(usn: u64, frn: u64, parent_frn: u64, name: &str) -> Self {
        Self::base(usn, frn, parent_frn, name, reason::FILE_CREATE)
    }

    /// A `FILE_DELETE` event.
    pub fn delete(usn: u64, frn: u64, parent_frn: u64, name: &str) -> Self {
        Self::base(usn, frn, parent_frn, name, reason::FILE_DELETE)
    }

    /// The pre-image of a rename (`RENAME_OLD_NAME`): carries the old name/parent.
    pub fn rename_old(usn: u64, frn: u64, parent_frn: u64, name: &str) -> Self {
        Self::base(usn, frn, parent_frn, name, reason::RENAME_OLD_NAME)
    }

    /// The post-image of a rename/move (`RENAME_NEW_NAME`): carries the new name/parent.
    pub fn rename_new(usn: u64, frn: u64, parent_frn: u64, name: &str) -> Self {
        Self::base(usn, frn, parent_frn, name, reason::RENAME_NEW_NAME)
    }

    /// Mark this event's file as a directory.
    #[must_use]
    pub fn dir(mut self) -> Self {
        self.attributes |= FILE_ATTRIBUTE_DIRECTORY;
        self
    }

    /// Mark this event's file as a reparse point.
    #[must_use]
    pub fn reparse(mut self) -> Self {
        self.attributes |= FILE_ATTRIBUTE_REPARSE_POINT;
        self
    }

    /// OR in `CLOSE` (the record that flushes an open handle's accumulated reasons).
    #[must_use]
    pub fn close(mut self) -> Self {
        self.reason |= reason::CLOSE;
        self
    }

    /// OR in extra reason bits (e.g. `DATA_EXTEND` alongside `FILE_CREATE`).
    #[must_use]
    pub fn also(mut self, extra: u32) -> Self {
        self.reason |= extra;
        self
    }
}

/// A fake volume backed by a fixed list of enum records and a fixed list of journal events.
#[derive(Debug, Clone)]
pub struct FakeVolume {
    records: Vec<FakeRecord>,
    events: Vec<FakeEvent>,
    journal: JournalInfo,
    cluster_bytes: u32,
    /// M5 live size refresh: `frn -> (allocated_clusters, truncated)` returned by
    /// [`alloc_clusters`](UsnVolume::alloc_clusters), so a test can simulate a live-created /
    /// grown file getting sized after its USN event is applied.
    live_sizes: HashMap<u64, (u32, bool)>,
}

impl FakeVolume {
    /// Build a fake volume from `records`, in the order they will be enumerated. No journal
    /// events until [`with_events`](Self::with_events) is chained.
    pub fn new(records: Vec<FakeRecord>) -> Self {
        Self {
            records,
            events: Vec::new(),
            journal: JournalInfo {
                journal_id: 1,
                first_usn: 0,
                next_usn: 0,
            },
            cluster_bytes: 4096,
            live_sizes: HashMap::new(),
        }
    }

    /// Override the cluster size used to convert the injected cluster counts to bytes.
    #[must_use]
    pub fn with_cluster_bytes(mut self, cluster_bytes: u32) -> Self {
        self.cluster_bytes = cluster_bytes;
        self
    }

    /// Inject live sizes (`frn -> (allocated_clusters, truncated)`) returned by
    /// [`alloc_clusters`](UsnVolume::alloc_clusters): the M5 live-size refresh path.
    #[must_use]
    pub fn with_live_sizes(mut self, sizes: impl IntoIterator<Item = (u64, (u32, bool))>) -> Self {
        self.live_sizes = sizes.into_iter().collect();
        self
    }

    /// Attach journal `events` (assumed in ascending USN order). `next_usn` advances to the
    /// last event's USN + 1 so a caught-up read resumes at the tail.
    #[must_use]
    pub fn with_events(mut self, events: Vec<FakeEvent>) -> Self {
        if let Some(last) = events.last() {
            self.journal.next_usn = last.usn + 1;
        }
        self.events = events;
        self
    }

    /// Simulate a journal that has trimmed everything below `first_usn`: a
    /// [`read_journal`](UsnVolume::read_journal) from a lower USN returns
    /// [`JournalError::EntryDeleted`].
    #[must_use]
    pub fn trim_to(mut self, first_usn: u64) -> Self {
        self.journal.first_usn = first_usn;
        self
    }

    /// Set the journal id (to simulate a delete+recreate -> [`JournalError::IdChanged`]).
    #[must_use]
    pub fn with_journal_id(mut self, journal_id: u64) -> Self {
        self.journal.journal_id = journal_id;
        self
    }

    /// Overwrite the live journal metadata *after* construction, for tests that need to
    /// simulate a recreation (new id) or a wrap/trim (advanced `first_usn`) between an
    /// initial enumerate and a later `read_journal`/sync.
    pub fn set_journal(&mut self, journal_id: u64, first_usn: u64, next_usn: u64) {
        self.journal = JournalInfo {
            journal_id,
            first_usn,
            next_usn,
        };
    }
}

impl UsnVolume for FakeVolume {
    fn journal_info(&self) -> anyhow::Result<JournalInfo> {
        Ok(self.journal)
    }

    fn enum_records(&mut self, sink: &mut dyn FnMut(RawRecord<'_>)) -> anyhow::Result<()> {
        for r in &self.records {
            // Names arrive from NTFS as UTF-16; mirror that here so the builder's
            // decode path is exercised by tests.
            let name_utf16: Vec<u16> = r.name.encode_utf16().collect();
            sink(RawRecord {
                frn: r.frn,
                parent_frn: r.parent_frn,
                name_utf16: &name_utf16,
                attributes: r.attributes,
            });
        }
        Ok(())
    }

    fn cluster_bytes(&self) -> u32 {
        self.cluster_bytes
    }

    fn enum_sizes(&self, sink: &mut dyn FnMut(u64, u32, bool)) -> anyhow::Result<()> {
        for r in &self.records {
            if let Some((alloc_clusters, truncated)) = r.size {
                sink(r.frn, alloc_clusters, truncated);
            }
        }
        Ok(())
    }

    fn alloc_clusters(&self, frn: u64, _path: &str) -> Option<(u32, bool)> {
        self.live_sizes.get(&frn).copied()
    }

    fn read_journal(
        &self,
        start_usn: u64,
        journal_id: u64,
        sink: &mut dyn FnMut(UsnEvent<'_>),
    ) -> Result<u64, JournalError> {
        if journal_id != self.journal.journal_id {
            return Err(JournalError::IdChanged {
                expected: journal_id,
                found: self.journal.journal_id,
            });
        }
        if start_usn < self.journal.first_usn {
            return Err(JournalError::EntryDeleted);
        }
        let mut cursor = start_usn;
        for e in &self.events {
            if e.usn < start_usn {
                continue;
            }
            let name_utf16: Vec<u16> = e.name.encode_utf16().collect();
            sink(UsnEvent {
                frn: e.frn,
                parent_frn: e.parent_frn,
                usn: e.usn,
                reason: e.reason,
                name_utf16: &name_utf16,
                attributes: e.attributes,
            });
            cursor = e.usn + 1;
        }
        Ok(cursor.max(self.journal.next_usn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(
        vol: &FakeVolume,
        start: u64,
        jid: u64,
    ) -> Result<(Vec<(u64, String)>, u64), JournalError> {
        let mut got = Vec::new();
        let cursor = vol.read_journal(start, jid, &mut |e| {
            got.push((e.frn, String::from_utf16_lossy(e.name_utf16)));
        })?;
        Ok((got, cursor))
    }

    #[test]
    fn drains_events_at_or_after_start_and_reports_cursor() {
        let vol = FakeVolume::new(vec![]).with_events(vec![
            FakeEvent::create(100, 7, 5, "a.txt"),
            FakeEvent::rename_old(108, 7, 5, "a.txt"),
            FakeEvent::rename_new(112, 7, 5, "b.txt"),
        ]);
        // From the start: all three, cursor = last usn + 1.
        let (got, cursor) = drain(&vol, 0, 1).unwrap();
        assert_eq!(
            got,
            vec![
                (7, "a.txt".to_owned()),
                (7, "a.txt".to_owned()),
                (7, "b.txt".to_owned()),
            ]
        );
        assert_eq!(cursor, 113);
        // Resuming mid-stream skips already-seen USNs.
        let (got, cursor) = drain(&vol, 110, 1).unwrap();
        assert_eq!(got, vec![(7, "b.txt".to_owned())]);
        assert_eq!(cursor, 113);
    }

    #[test]
    fn carries_reason_parent_and_attributes() {
        let vol = FakeVolume::new(vec![])
            .with_events(vec![FakeEvent::create(10, 9, 5, "dir").dir().close()]);
        let mut seen = None;
        vol.read_journal(0, 1, &mut |e| {
            seen = Some((e.frn, e.parent_frn, e.reason, e.attributes));
        })
        .unwrap();
        let (frn, parent, rsn, attrs) = seen.unwrap();
        assert_eq!(frn, 9);
        assert_eq!(parent, 5);
        assert_eq!(rsn, reason::FILE_CREATE | reason::CLOSE);
        assert_eq!(attrs & FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_DIRECTORY);
    }

    #[test]
    fn trimmed_start_is_entry_deleted() {
        let vol = FakeVolume::new(vec![])
            .with_events(vec![FakeEvent::create(200, 7, 5, "a.txt")])
            .trim_to(150);
        match vol.read_journal(100, 1, &mut |_| {}) {
            Err(JournalError::EntryDeleted) => {}
            other => panic!("expected EntryDeleted, got {other:?}"),
        }
        // Reading at/after first_usn succeeds.
        assert!(vol.read_journal(150, 1, &mut |_| {}).is_ok());
    }

    #[test]
    fn journal_id_mismatch_is_id_changed() {
        let vol = FakeVolume::new(vec![]).with_journal_id(0xABCD);
        match vol.read_journal(0, 1, &mut |_| {}) {
            Err(JournalError::IdChanged { expected, found }) => {
                assert_eq!(expected, 1);
                assert_eq!(found, 0xABCD);
            }
            other => panic!("expected IdChanged, got {other:?}"),
        }
    }
}
