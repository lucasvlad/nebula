//! The local state database: which folders are synced and the manifest of
//! every file in them. SQLite in WAL mode, so a crash mid-write leaves the
//! previous committed state intact.

use std::path::{Path, PathBuf};

use nebula_core::manifest::FileEntry;
use nebula_core::path::{PathError, RelPath};
use nebula_core::{DeviceId, Digest};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// Schema migrations, applied in order. `PRAGMA user_version` records how
/// many have run.
const MIGRATIONS: &[&str] = &[
    // 1: folders and manifests
    "
CREATE TABLE folders (
    id           INTEGER PRIMARY KEY,
    root         TEXT NOT NULL UNIQUE,
    last_scan_ns INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE files (
    folder_id    INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    content_hash BLOB NOT NULL,
    chunks       BLOB NOT NULL,
    version      BLOB NOT NULL,
    modified_by  BLOB NOT NULL,
    deleted      INTEGER NOT NULL,
    PRIMARY KEY (folder_id, path)
) WITHOUT ROWID;
",
    // 2: the allowlist of paired devices
    "
CREATE TABLE peers (
    device_id  BLOB PRIMARY KEY,
    public_key BLOB NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    last_addr  TEXT,
    paired_at  INTEGER NOT NULL
) WITHOUT ROWID;
",
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt manifest record: {0}")]
    Decode(#[from] postcard::Error),
    #[error("corrupt manifest path: {0}")]
    Path(#[from] PathError),
    #[error("folder path is not valid UTF-8: {0}")]
    NonUtf8Root(PathBuf),
    #[error("{0} is already synced")]
    AlreadySynced(PathBuf),
    #[error("database schema version {0} is newer than this build supports")]
    UnknownSchema(i64),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A folder the user opted into syncing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub root: PathBuf,
    /// When the last completed scan started, in ns since the Unix epoch.
    /// 0 if never scanned.
    pub last_scan_ns: i64,
}

/// A device on the allowlist. Only these may connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub device_id: DeviceId,
    pub public_key: [u8; 32],
    pub name: String,
    /// Where we last reached it, as `host:port`.
    pub last_addr: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let applied = usize::try_from(version).unwrap_or(usize::MAX);
        if applied > MIGRATIONS.len() {
            return Err(StoreError::UnknownSchema(version));
        }
        let mut conn = conn;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// Opts a folder in. `root` should already be canonical.
    pub fn add_folder(&self, root: &Path) -> Result<Folder> {
        let text = root_text(root)?;
        let inserted = self.conn.execute(
            "INSERT INTO folders (root) VALUES (?1) ON CONFLICT (root) DO NOTHING",
            [text],
        )?;
        if inserted == 0 {
            return Err(StoreError::AlreadySynced(root.to_path_buf()));
        }
        Ok(Folder {
            id: self.conn.last_insert_rowid(),
            root: root.to_path_buf(),
            last_scan_ns: 0,
        })
    }

    /// Stops syncing a folder and forgets its manifest. Files on disk are
    /// untouched. Returns false if the folder was not synced.
    pub fn remove_folder(&self, root: &Path) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM folders WHERE root = ?1", [root_text(root)?])?;
        Ok(n > 0)
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, root, last_scan_ns FROM folders ORDER BY root")?;
        let rows = stmt.query_map([], folder_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn folder(&self, root: &Path) -> Result<Option<Folder>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, root, last_scan_ns FROM folders WHERE root = ?1",
                [root_text(root)?],
                folder_from_row,
            )
            .optional()?)
    }

    /// Every entry in a folder's manifest, tombstones included, by path.
    pub fn files(&self, folder_id: i64) -> Result<Vec<FileEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size, mtime_ns, content_hash, chunks, version, modified_by, deleted
             FROM files WHERE folder_id = ?1 ORDER BY path",
        )?;
        let mut rows = stmt.query([folder_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(entry_from_row(row)?);
        }
        Ok(out)
    }

    /// Writes the result of a scan in one transaction: either every change
    /// lands, or none do.
    pub fn save_scan(
        &mut self,
        folder_id: i64,
        changed: &[FileEntry],
        scan_started_ns: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO files
                 (folder_id, path, size, mtime_ns, content_hash, chunks, version, modified_by, deleted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for e in changed {
                stmt.execute(params![
                    folder_id,
                    e.path.as_str(),
                    // SQLite integers are i64; real files never reach 2^63 bytes.
                    i64::try_from(e.size).unwrap_or(i64::MAX),
                    e.mtime_ns,
                    &e.content_hash.0[..],
                    postcard::to_stdvec(&e.chunks)?,
                    postcard::to_stdvec(&e.version)?,
                    &e.modified_by.0[..],
                    e.deleted,
                ])?;
            }
        }
        tx.execute(
            "UPDATE folders SET last_scan_ns = ?2 WHERE id = ?1",
            params![folder_id, scan_started_ns],
        )?;
        tx.commit()?;
        Ok(())
    }
}

impl Store {
    /// Adds a device to the allowlist, or updates its name if re-paired.
    pub fn add_peer(&self, peer: &Peer) -> Result<()> {
        self.conn.execute(
            "INSERT INTO peers (device_id, public_key, name, last_addr, paired_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())
             ON CONFLICT (device_id) DO UPDATE SET name = excluded.name",
            params![
                &peer.device_id.0[..],
                &peer.public_key[..],
                peer.name,
                peer.last_addr
            ],
        )?;
        Ok(())
    }

    /// Removes a device from the allowlist. Returns false if it was not there.
    pub fn remove_peer(&self, device: &DeviceId) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM peers WHERE device_id = ?1", [&device.0[..]])?;
        Ok(n > 0)
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        let mut stmt = self
            .conn
            .prepare("SELECT device_id, public_key, name, last_addr FROM peers ORDER BY name")?;
        let rows = stmt.query_map([], peer_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn peer(&self, device: &DeviceId) -> Result<Option<Peer>> {
        Ok(self
            .conn
            .query_row(
                "SELECT device_id, public_key, name, last_addr FROM peers WHERE device_id = ?1",
                [&device.0[..]],
                peer_from_row,
            )
            .optional()?)
    }

    /// Whether this static public key belongs to a paired device.
    pub fn is_paired(&self, public_key: &[u8; 32]) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM peers WHERE public_key = ?1",
                [&public_key[..]],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn set_peer_addr(&self, device: &DeviceId, addr: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE peers SET last_addr = ?2 WHERE device_id = ?1",
            params![&device.0[..], addr],
        )?;
        Ok(())
    }
}

fn peer_from_row(row: &Row<'_>) -> rusqlite::Result<Peer> {
    Ok(Peer {
        device_id: DeviceId(row.get(0)?),
        public_key: row.get(1)?,
        name: row.get(2)?,
        last_addr: row.get(3)?,
    })
}

fn root_text(root: &Path) -> Result<&str> {
    root.to_str()
        .ok_or_else(|| StoreError::NonUtf8Root(root.to_path_buf()))
}

fn folder_from_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get(0)?,
        root: PathBuf::from(row.get::<_, String>(1)?),
        last_scan_ns: row.get(2)?,
    })
}

fn entry_from_row(row: &Row<'_>) -> Result<FileEntry> {
    let path: String = row.get(0)?;
    let size: i64 = row.get(1)?;
    Ok(FileEntry {
        path: RelPath::new(&path)?,
        size: u64::try_from(size).unwrap_or(0),
        mtime_ns: row.get(2)?,
        content_hash: Digest(row.get(3)?),
        chunks: postcard::from_bytes(&row.get::<_, Vec<u8>>(4)?)?,
        version: postcard::from_bytes(&row.get::<_, Vec<u8>>(5)?)?,
        modified_by: DeviceId(row.get(6)?),
        deleted: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::chunker::ChunkRef;
    use nebula_core::version::VersionVector;

    fn entry(path: &str) -> FileEntry {
        let dev = DeviceId([7; 32]);
        let mut version = VersionVector::new();
        version.increment(dev);
        FileEntry {
            path: RelPath::new(path).unwrap(),
            size: 3,
            mtime_ns: 1_000,
            content_hash: Digest::of(b"abc"),
            chunks: vec![ChunkRef {
                hash: Digest::of(b"abc"),
                len: 3,
            }],
            version,
            modified_by: dev,
            deleted: false,
        }
    }

    #[test]
    fn add_list_remove_folders() {
        let store = Store::open_in_memory().unwrap();
        let a = store.add_folder(Path::new("/home/me/a")).unwrap();
        store.add_folder(Path::new("/home/me/b")).unwrap();
        assert!(matches!(
            store.add_folder(Path::new("/home/me/a")),
            Err(StoreError::AlreadySynced(_))
        ));
        assert_eq!(store.folders().unwrap().len(), 2);
        assert_eq!(store.folder(Path::new("/home/me/a")).unwrap(), Some(a));
        assert!(store.remove_folder(Path::new("/home/me/a")).unwrap());
        assert!(!store.remove_folder(Path::new("/home/me/a")).unwrap());
        assert_eq!(store.folders().unwrap().len(), 1);
    }

    #[test]
    fn entries_round_trip() {
        let mut store = Store::open_in_memory().unwrap();
        let f = store.add_folder(Path::new("/r")).unwrap();
        let mut gone = entry("old.txt");
        gone.mark_deleted(DeviceId([8; 32]));
        let saved = vec![entry("docs/é.txt"), gone];
        store.save_scan(f.id, &saved, 42).unwrap();
        let mut loaded = store.files(f.id).unwrap();
        loaded.sort_by(|a, b| a.path.cmp(&b.path));
        let mut expected = saved.clone();
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(loaded, expected);
        assert_eq!(
            store.folder(Path::new("/r")).unwrap().unwrap().last_scan_ns,
            42
        );
    }

    #[test]
    fn save_replaces_existing_entry() {
        let mut store = Store::open_in_memory().unwrap();
        let f = store.add_folder(Path::new("/r")).unwrap();
        store.save_scan(f.id, &[entry("a")], 1).unwrap();
        let mut newer = entry("a");
        newer.size = 99;
        store.save_scan(f.id, &[newer.clone()], 2).unwrap();
        assert_eq!(store.files(f.id).unwrap(), vec![newer]);
    }

    #[test]
    fn removing_folder_drops_its_manifest() {
        let mut store = Store::open_in_memory().unwrap();
        let f = store.add_folder(Path::new("/r")).unwrap();
        store.save_scan(f.id, &[entry("a")], 1).unwrap();
        store.remove_folder(Path::new("/r")).unwrap();
        let g = store.add_folder(Path::new("/r")).unwrap();
        assert!(store.files(g.id).unwrap().is_empty());
    }

    fn peer(n: u8, name: &str) -> Peer {
        let public_key = [n; 32];
        Peer {
            device_id: DeviceId::from_public_key(&public_key),
            public_key,
            name: name.to_string(),
            last_addr: None,
        }
    }

    #[test]
    fn allowlist_add_check_remove() {
        let store = Store::open_in_memory().unwrap();
        let laptop = peer(1, "laptop");
        assert!(!store.is_paired(&laptop.public_key).unwrap());
        store.add_peer(&laptop).unwrap();
        store.add_peer(&peer(2, "desktop")).unwrap();
        assert!(store.is_paired(&laptop.public_key).unwrap());
        assert!(!store.is_paired(&[9; 32]).unwrap());
        let names: Vec<String> = store.peers().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["desktop", "laptop"]);

        store
            .set_peer_addr(&laptop.device_id, "192.168.1.5:7878")
            .unwrap();
        let got = store.peer(&laptop.device_id).unwrap().unwrap();
        assert_eq!(got.last_addr.as_deref(), Some("192.168.1.5:7878"));

        assert!(store.remove_peer(&laptop.device_id).unwrap());
        assert!(!store.remove_peer(&laptop.device_id).unwrap());
        assert!(!store.is_paired(&laptop.public_key).unwrap());
    }

    #[test]
    fn repairing_updates_name() {
        let store = Store::open_in_memory().unwrap();
        store.add_peer(&peer(1, "old")).unwrap();
        store.add_peer(&peer(1, "new")).unwrap();
        let peers = store.peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "new");
    }

    #[test]
    fn migrates_a_version_1_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute("INSERT INTO folders (root) VALUES ('/r')", [])
                .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.folders().unwrap().len(), 1);
        store.add_peer(&peer(1, "laptop")).unwrap();
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        Connection::open(&db)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(matches!(
            Store::open(&db),
            Err(StoreError::UnknownSchema(99))
        ));
    }

    #[test]
    fn reopening_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        {
            let mut store = Store::open(&db).unwrap();
            let f = store.add_folder(Path::new("/r")).unwrap();
            store.save_scan(f.id, &[entry("a")], 1).unwrap();
        }
        let store = Store::open(&db).unwrap();
        let f = store.folder(Path::new("/r")).unwrap().unwrap();
        assert_eq!(store.files(f.id).unwrap().len(), 1);
    }
}
