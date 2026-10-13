//! Manifest entries: what each device knows about each file in a folder.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::chunker::ChunkRef;
use crate::path::RelPath;
use crate::version::VersionVector;
use crate::{DeviceId, Digest};

/// The state of one file in a synced folder. A deleted file stays in the
/// manifest as a tombstone so the deletion itself can sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: RelPath,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch. Only used to
    /// skip rehashing unchanged files and to break ties between conflicts.
    pub mtime_ns: i64,
    pub content_hash: Digest,
    pub chunks: Vec<ChunkRef>,
    pub version: VersionVector,
    /// The device that made the latest change.
    pub modified_by: DeviceId,
    pub deleted: bool,
}

impl FileEntry {
    /// Turns this entry into a tombstone recording a deletion by `device`.
    pub fn mark_deleted(&mut self, device: DeviceId) {
        self.deleted = true;
        self.size = 0;
        self.content_hash = Digest::of(&[]);
        self.chunks.clear();
        self.version.increment(device);
        self.modified_by = device;
    }
}

/// Chunks in `want` whose hash is not in `have`, each listed once, in order.
/// This is exactly what has to cross the network to rebuild `want`.
pub fn missing_chunks<'a>(have: &[ChunkRef], want: &'a [ChunkRef]) -> Vec<&'a ChunkRef> {
    let mut seen: HashSet<Digest> = have.iter().map(|c| c.hash).collect();
    want.iter().filter(|c| seen.insert(c.hash)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(byte: u8, len: u32) -> ChunkRef {
        ChunkRef {
            hash: Digest::of(&[byte]),
            len,
        }
    }

    #[test]
    fn missing_chunks_skips_known_and_duplicates() {
        let have = [chunk(1, 10), chunk(2, 10)];
        let want = [chunk(1, 10), chunk(3, 5), chunk(3, 5), chunk(4, 7)];
        let missing = missing_chunks(&have, &want);
        assert_eq!(missing, vec![&want[1], &want[3]]);
    }

    #[test]
    fn missing_chunks_of_identical_lists_is_empty() {
        let list = [chunk(1, 10), chunk(2, 10)];
        assert!(missing_chunks(&list, &list).is_empty());
    }

    #[test]
    fn mark_deleted_makes_a_newer_tombstone() {
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        let mut version = VersionVector::new();
        version.increment(a);
        let mut entry = FileEntry {
            path: RelPath::new("notes.txt").unwrap(),
            size: 10,
            mtime_ns: 5,
            content_hash: Digest::of(b"0123456789"),
            chunks: vec![chunk(9, 10)],
            version: version.clone(),
            modified_by: a,
            deleted: false,
        };
        entry.mark_deleted(b);
        assert!(entry.deleted);
        assert_eq!(entry.size, 0);
        assert!(entry.chunks.is_empty());
        assert_eq!(entry.modified_by, b);
        assert_eq!(
            entry.version.compare(&version),
            crate::version::Causality::After
        );
    }
}
