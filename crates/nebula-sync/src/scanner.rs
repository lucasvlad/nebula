//! Walks a synced folder and brings its manifest up to date: new and changed
//! files get chunked and a version bump, vanished files become tombstones.
//!
//! A tombstone deletes the file on every peer, so the scanner only records a
//! deletion when it is sure: a missing root is an error, and nothing under an
//! unreadable directory or a skipped path is ever marked deleted.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nebula_core::DeviceId;
use nebula_core::chunker::{RollingChunker, chunk_all};
use nebula_core::manifest::{FileEntry, missing_chunks};
use nebula_core::path::{PathError, RelPath, case_collisions};

use tracing::{debug, info, warn};

use crate::store::{Folder, Store, StoreError};

/// Prefix for Nebula's own in-progress downloads, which are never synced.
pub const TEMP_PREFIX: &str = ".nebula-tmp-";

/// Files whose mtime is this close to (or after) the previous scan are
/// rehashed even if size and mtime look unchanged: some filesystems only
/// store mtime to the nearest 2 seconds, so an edit made right after a scan
/// can keep the same mtime ("racy git" problem).
const RACY_WINDOW_NS: i64 = 2_000_000_000;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("folder {0} is missing or not a directory")]
    RootMissing(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub added: Vec<Added>,
    pub modified: Vec<Modified>,
    pub deleted: Vec<RelPath>,
    /// Files whose content is the same as at the last scan.
    pub unchanged: usize,
    /// How many of the unchanged files were trusted from size and mtime
    /// alone, without reading them.
    pub trusted: usize,
    pub skipped: Vec<Skipped>,
    /// Files read and chunked this scan, and their total size.
    pub read_files: usize,
    pub read_bytes: u64,
}

impl ScanReport {
    pub fn has_changes(&self) -> bool {
        !(self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty())
    }
}

#[derive(Debug)]
pub struct Added {
    pub path: RelPath,
    pub size: u64,
    pub chunks: usize,
}

/// A changed file, with how much of it would actually need to be sent.
#[derive(Debug)]
pub struct Modified {
    pub path: RelPath,
    pub chunks_total: usize,
    pub chunks_new: usize,
    pub new_bytes: u64,
    pub size: u64,
}

#[derive(Debug)]
pub struct Skipped {
    /// Path relative to the folder root, as the OS reported it.
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug, thiserror::Error)]
pub enum SkipReason {
    #[error("name is not valid UTF-8")]
    NotUtf8,
    #[error("{0}")]
    InvalidName(PathError),
    #[error("differs only in letter case from another path")]
    CaseCollision,
    #[error("symbolic links are not synced")]
    Symlink,
    #[error("changed while being read; will retry next scan")]
    Changing,
    #[error("could not read: {0}")]
    Unreadable(io::Error),
}

/// A regular file found on disk.
struct Found {
    path: RelPath,
    abs: PathBuf,
    size: u64,
    mtime_ns: i64,
}

#[derive(Default)]
struct Walk {
    files: Vec<Found>,
    skipped: Vec<Skipped>,
    /// Paths that exist on disk but are not synced this round.
    skipped_paths: HashSet<RelPath>,
    /// Directories we could not list. Their manifest entries are left alone.
    blind_dirs: Vec<RelPath>,
}

pub fn scan(store: &mut Store, folder: &Folder, device: DeviceId) -> Result<ScanReport, ScanError> {
    let started_ns = now_ns();
    if !folder.root.is_dir() {
        return Err(ScanError::RootMissing(folder.root.clone()));
    }

    let mut walk = Walk::default();
    walk_dir(&folder.root, &folder.root, &mut walk);

    let collisions: HashSet<RelPath> = case_collisions(walk.files.iter().map(|f| &f.path))
        .into_iter()
        .cloned()
        .collect();

    let known: HashMap<RelPath, FileEntry> = store
        .files(folder.id)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    let mut report = ScanReport {
        skipped: walk.skipped,
        ..ScanReport::default()
    };
    let mut seen = walk.skipped_paths;
    let mut changed = Vec::new();

    for found in walk.files {
        seen.insert(found.path.clone());
        if collisions.contains(&found.path) {
            report
                .skipped
                .push(skip(&folder.root, &found.abs, SkipReason::CaseCollision));
            continue;
        }
        let old = known.get(&found.path).filter(|e| !e.deleted);
        if let Some(old) = old {
            let trusted = found.mtime_ns < folder.last_scan_ns.saturating_sub(RACY_WINDOW_NS);
            if trusted && old.size == found.size && old.mtime_ns == found.mtime_ns {
                debug!(
                    "{}: same size and modification time as last scan, so not re-reading it",
                    found.path
                );
                report.unchanged += 1;
                report.trusted += 1;
                continue;
            }
            if old.size == found.size && old.mtime_ns == found.mtime_ns {
                debug!(
                    "{}: looks unchanged, but was modified close to the last scan, so reading it to be sure",
                    found.path
                );
            }
        }

        let chunked =
            match File::open(&found.abs).and_then(|f| chunk_all(f, RollingChunker::default())) {
                Ok(c) => c,
                Err(e) => {
                    warn!("{}: could not read it ({e}), skipping", found.path);
                    report
                        .skipped
                        .push(skip(&folder.root, &found.abs, SkipReason::Unreadable(e)));
                    continue;
                }
            };
        // If the file moved under us, the hash may mix two versions.
        match fs::symlink_metadata(&found.abs) {
            Ok(m) if m.len() == found.size && mtime_ns(&m) == found.mtime_ns => {}
            _ => {
                debug!(
                    "{}: changed while being read, will retry next scan",
                    found.path
                );
                report
                    .skipped
                    .push(skip(&folder.root, &found.abs, SkipReason::Changing));
                continue;
            }
        }

        report.read_files += 1;
        report.read_bytes += chunked.size;
        debug!(
            "{}: read {} bytes and split them into {} chunks",
            found.path,
            chunked.size,
            chunked.chunks.len()
        );
        let mut entry = FileEntry {
            path: found.path.clone(),
            size: chunked.size,
            mtime_ns: found.mtime_ns,
            content_hash: chunked.content_hash,
            chunks: chunked.chunks,
            version: Default::default(),
            modified_by: device,
            deleted: false,
        };
        match old {
            Some(old) if old.content_hash == entry.content_hash => {
                // Touched but not edited: refresh the stat info, keep the version.
                debug!(
                    "{}: content identical to last scan, version unchanged",
                    entry.path
                );
                report.unchanged += 1;
                if old.mtime_ns != entry.mtime_ns {
                    entry.version = old.version.clone();
                    entry.modified_by = old.modified_by;
                    changed.push(entry);
                }
            }
            Some(old) => {
                let new = missing_chunks(&old.chunks, &entry.chunks);
                report.modified.push(Modified {
                    path: entry.path.clone(),
                    chunks_total: entry.chunks.len(),
                    chunks_new: new.len(),
                    new_bytes: new.iter().map(|c| u64::from(c.len)).sum(),
                    size: entry.size,
                });
                entry.version = old.version.clone();
                entry.version.increment(device);
                info!(
                    "{}: changed, so this device's edit count goes up to {}",
                    entry.path,
                    entry.version.get(&device)
                );
                changed.push(entry);
            }
            None => {
                // A recreated file continues from its tombstone's history.
                if let Some(tomb) = known.get(&found.path) {
                    entry.version = tomb.version.clone();
                }
                entry.version.increment(device);
                if known.contains_key(&found.path) {
                    info!(
                        "{}: recreated after being deleted, continuing its version history",
                        entry.path
                    );
                } else {
                    info!("{}: new file, starting its version history", entry.path);
                }
                report.added.push(Added {
                    path: entry.path.clone(),
                    size: entry.size,
                    chunks: entry.chunks.len(),
                });
                changed.push(entry);
            }
        }
    }

    for (path, entry) in &known {
        let under_blind_dir = walk
            .blind_dirs
            .iter()
            .any(|d| path.as_str().starts_with(&format!("{d}/")));
        if entry.deleted || seen.contains(path) {
            continue;
        }
        if under_blind_dir {
            debug!(
                "{path}: missing, but its folder could not be read, so not treating it as deleted"
            );
            continue;
        }
        info!("{path}: deleted, recording a tombstone so other devices delete it too");
        let mut tomb = entry.clone();
        tomb.mark_deleted(device);
        report.deleted.push(path.clone());
        changed.push(tomb);
    }

    report.added.sort_by(|a, b| a.path.cmp(&b.path));
    report.modified.sort_by(|a, b| a.path.cmp(&b.path));
    report.deleted.sort();
    store.save_scan(folder.id, &changed, started_ns)?;
    Ok(report)
}

fn walk_dir(root: &Path, dir: &Path, walk: &mut Walk) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return blind(root, dir, e, walk),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return blind(root, dir, e, walk),
        };
        let abs = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            walk.skipped.push(skip(root, &abs, SkipReason::NotUtf8));
            continue;
        };
        if is_ignored(&name) {
            continue;
        }
        let path = match rel_path(root, &abs) {
            Ok(path) => path,
            Err(reason) => {
                walk.skipped.push(skip(root, &abs, reason));
                continue;
            }
        };
        let meta = match fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(e) => {
                walk.skipped
                    .push(skip(root, &abs, SkipReason::Unreadable(e)));
                walk.skipped_paths.insert(path);
                continue;
            }
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            walk.skipped.push(skip(root, &abs, SkipReason::Symlink));
            walk.skipped_paths.insert(path);
        } else if kind.is_dir() {
            walk_dir(root, &abs, walk);
        } else if kind.is_file() {
            walk.files.push(Found {
                path,
                abs,
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
            });
        }
    }
}

/// Records a directory we could not list, so nothing under it is tombstoned.
fn blind(root: &Path, dir: &Path, e: io::Error, walk: &mut Walk) {
    if let Ok(path) = rel_path(root, dir) {
        walk.blind_dirs.push(path);
    }
    walk.skipped
        .push(skip(root, dir, SkipReason::Unreadable(e)));
}

fn rel_path(root: &Path, abs: &Path) -> Result<RelPath, SkipReason> {
    let rel = abs.strip_prefix(root).unwrap_or(abs);
    let mut parts = Vec::new();
    for c in rel.components() {
        parts.push(c.as_os_str().to_str().ok_or(SkipReason::NotUtf8)?);
    }
    RelPath::from_components(parts).map_err(SkipReason::InvalidName)
}

fn skip(root: &Path, abs: &Path, reason: SkipReason) -> Skipped {
    Skipped {
        path: abs.strip_prefix(root).unwrap_or(abs).to_path_buf(),
        reason,
    }
}

/// OS clutter, editor lock files and our own temp files.
fn is_ignored(name: &str) -> bool {
    const NAMES: &[&str] = &[
        ".DS_Store",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        "Thumbs.db",
        "desktop.ini",
        "$RECYCLE.BIN",
        "System Volume Information",
    ];
    const PREFIXES: &[&str] = &[TEMP_PREFIX, "~$", ".~lock."];
    const SUFFIXES: &[&str] = &[".swp", ".swx"];
    NAMES.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || SUFFIXES.iter().any(|s| name.ends_with(s))
}

fn mtime_ns(meta: &Metadata) -> i64 {
    meta.modified().map(system_time_ns).unwrap_or(0)
}

fn now_ns() -> i64 {
    system_time_ns(SystemTime::now())
}

fn system_time_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::version::Causality;
    use std::time::Duration;

    const DEV: DeviceId = DeviceId([1; 32]);

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
        folder: Folder,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open_in_memory().unwrap();
            let folder = store.add_folder(dir.path(), "test").unwrap();
            Self { dir, store, folder }
        }

        fn write(&self, rel: &str, data: &[u8]) -> PathBuf {
            let p = self.dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, data).unwrap();
            p
        }

        fn scan(&mut self) -> ScanReport {
            let report = scan(&mut self.store, &self.folder, DEV).unwrap();
            self.folder = self.store.folder(&self.folder.root).unwrap().unwrap();
            report
        }

        fn entry(&self, rel: &str) -> FileEntry {
            let path = RelPath::new(rel).unwrap();
            self.store
                .files(self.folder.id)
                .unwrap()
                .into_iter()
                .find(|e| e.path == path)
                .unwrap()
        }
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn paths(v: &[RelPath]) -> Vec<&str> {
        v.iter().map(RelPath::as_str).collect()
    }

    fn added(r: &ScanReport) -> Vec<&str> {
        r.added.iter().map(|a| a.path.as_str()).collect()
    }

    #[test]
    fn first_scan_adds_everything() {
        let mut fx = Fixture::new();
        fx.write("a.txt", b"hello");
        fx.write("sub/dir/b.bin", &[7; 100_000]);
        let r = fx.scan();
        assert_eq!(added(&r), ["a.txt", "sub/dir/b.bin"]);
        assert_eq!((r.read_files, r.read_bytes), (2, 100_005));
        let b = fx.entry("sub/dir/b.bin");
        assert_eq!(b.size, 100_000);
        assert_eq!(b.version.get(&DEV), 1);
        assert_eq!(
            b.chunks.iter().map(|c| u64::from(c.len)).sum::<u64>(),
            100_000
        );
    }

    #[test]
    fn rescan_without_changes_changes_nothing() {
        let mut fx = Fixture::new();
        fx.write("a.txt", b"hello");
        fx.scan();
        let before = fx.entry("a.txt");
        let r = fx.scan();
        assert!(!r.has_changes());
        assert_eq!(r.unchanged, 1);
        // Scanned again right away, so it is re-read rather than trusted.
        assert_eq!((r.trusted, r.read_files), (0, 1));
        assert_eq!(fx.entry("a.txt"), before);
    }

    #[test]
    fn edit_bumps_version_and_reports_new_chunks() {
        let mut fx = Fixture::new();
        let mut data: Vec<u8> = (0..1_000_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        fx.write("big.bin", &data);
        fx.scan();
        let before = fx.entry("big.bin");
        data[500_000] ^= 0xff;
        fx.write("big.bin", &data);
        let r = fx.scan();
        assert_eq!(r.modified.len(), 1);
        let m = &r.modified[0];
        assert!(
            m.chunks_new <= 2,
            "one-byte edit changed {} chunks",
            m.chunks_new
        );
        assert!(m.new_bytes < 150_000);
        let after = fx.entry("big.bin");
        assert_eq!(after.version.compare(&before.version), Causality::After);
    }

    #[test]
    fn delete_leaves_tombstone_and_recreate_continues_history() {
        let mut fx = Fixture::new();
        let p = fx.write("a.txt", b"hello");
        fx.scan();
        fs::remove_file(&p).unwrap();
        let r = fx.scan();
        assert_eq!(paths(&r.deleted), ["a.txt"]);
        let tomb = fx.entry("a.txt");
        assert!(tomb.deleted);
        assert_eq!(tomb.version.get(&DEV), 2);

        // A second scan does not delete it again.
        assert!(!fx.scan().has_changes());

        fx.write("a.txt", b"back");
        let r = fx.scan();
        assert_eq!(added(&r), ["a.txt"]);
        let back = fx.entry("a.txt");
        assert!(!back.deleted);
        assert_eq!(back.version.compare(&tomb.version), Causality::After);
    }

    #[test]
    fn touch_without_edit_keeps_version() {
        let mut fx = Fixture::new();
        let p = fx.write("a.txt", b"hello");
        fx.scan();
        let before = fx.entry("a.txt");
        set_mtime(&p, SystemTime::now() - Duration::from_secs(3600));
        let r = fx.scan();
        assert!(!r.has_changes());
        let after = fx.entry("a.txt");
        assert_eq!(after.version, before.version);
        assert_ne!(after.mtime_ns, before.mtime_ns);
    }

    #[test]
    fn same_size_edit_with_same_mtime_is_caught_when_recent() {
        let mut fx = Fixture::new();
        let p = fx.write("a.txt", b"aaaa");
        let t = SystemTime::now();
        set_mtime(&p, t);
        fx.scan();
        fx.write("a.txt", b"bbbb");
        set_mtime(&p, t);
        let r = fx.scan();
        assert_eq!(r.modified.len(), 1, "racy edit was missed");
    }

    #[test]
    fn old_unchanged_stat_skips_hashing() {
        let mut fx = Fixture::new();
        let p = fx.write("a.txt", b"aaaa");
        let old = SystemTime::now() - Duration::from_secs(3600);
        set_mtime(&p, old);
        fx.scan();
        // Same size and mtime, well before the last scan: trusted without
        // reading. This is the documented trade-off for speed.
        fx.write("a.txt", b"bbbb");
        set_mtime(&p, old);
        let r = fx.scan();
        assert!(!r.has_changes());
        assert_eq!((r.unchanged, r.trusted, r.read_files), (1, 1, 0));
    }

    #[test]
    fn ignores_clutter_and_temp_files() {
        let mut fx = Fixture::new();
        fx.write(".DS_Store", b"x");
        fx.write("sub/Thumbs.db", b"x");
        fx.write(&format!("{TEMP_PREFIX}123"), b"x");
        fx.write("~$report.docx", b"x");
        fx.write("real.txt", b"x");
        let r = fx.scan();
        assert_eq!(added(&r), ["real.txt"]);
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn invalid_names_are_skipped_not_synced() {
        let mut fx = Fixture::new();
        fx.write("ok.txt", b"x");
        // Legal on macOS/Linux, illegal on Windows.
        #[cfg(unix)]
        fx.write("a:b.txt", b"x");
        let r = fx.scan();
        assert_eq!(added(&r), ["ok.txt"]);
        #[cfg(unix)]
        assert!(matches!(r.skipped[0].reason, SkipReason::InvalidName(_)));
    }

    #[test]
    fn nfd_names_are_stored_as_nfc() {
        let mut fx = Fixture::new();
        fx.write("e\u{301}.txt", b"x");
        let r = fx.scan();
        assert_eq!(added(&r), ["\u{e9}.txt"]);
        // And the file is still found on the next scan, not deleted.
        assert!(!fx.scan().has_changes());
    }

    #[test]
    fn missing_root_is_an_error_not_a_mass_delete() {
        let mut fx = Fixture::new();
        fx.write("a.txt", b"x");
        fx.scan();
        let root = fx.folder.root.clone();
        let moved = root.with_extension("moved");
        fs::rename(&root, &moved).unwrap();
        let err = scan(&mut fx.store, &fx.folder, DEV).unwrap_err();
        assert!(matches!(err, ScanError::RootMissing(_)));
        fs::rename(&moved, &root).unwrap();
        assert!(!fx.entry("a.txt").deleted);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped() {
        let mut fx = Fixture::new();
        fx.write("real.txt", b"x");
        std::os::unix::fs::symlink("/etc/passwd", fx.dir.path().join("link")).unwrap();
        let r = fx.scan();
        assert_eq!(added(&r), ["real.txt"]);
        assert!(matches!(r.skipped[0].reason, SkipReason::Symlink));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_dir_does_not_delete_its_files() {
        use std::os::unix::fs::PermissionsExt;
        let mut fx = Fixture::new();
        fx.write("locked/a.txt", b"x");
        fx.scan();
        let locked = fx.dir.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let r = fx.scan();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if fs::read_dir(&locked).is_ok() && r.skipped.is_empty() {
            return; // Running as root: permissions are not enforced.
        }
        assert!(r.deleted.is_empty());
        assert!(!fx.entry("locked/a.txt").deleted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn case_collisions_are_skipped() {
        let mut fx = Fixture::new();
        fx.write("Report.txt", b"1");
        fx.write("report.txt", b"2");
        fx.write("other.txt", b"3");
        let r = fx.scan();
        assert_eq!(added(&r), ["other.txt"]);
        assert_eq!(r.skipped.len(), 2);
    }
}
