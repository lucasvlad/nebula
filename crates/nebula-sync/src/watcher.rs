//! Watches synced folders and reports which ones changed, debounced.
//!
//! The watcher only says "something under folder N changed"; the engine
//! rescans that folder to find out what. Nebula's own in-progress downloads
//! (temp files) are ignored. Its finished writes are not: the engine records
//! a file in its manifest before renaming it into place, so the rescan they
//! trigger finds nothing new.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify_debouncer_mini::notify::{self, RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use tracing::{debug, warn};

use crate::scanner::TEMP_PREFIX;
use crate::store::Folder;

/// Changes closer together than this are reported once.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Files the OS litters folders with. Changes to them are not worth a rescan.
const CLUTTER: &[&str] = &[".DS_Store"];

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("could not start watching for changes: {0}")]
    Notify(#[from] notify::Error),
}

/// Keeps the OS watches alive. Dropping it stops them.
pub struct FolderWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

/// Watches each folder's root recursively and calls `on_change` with a
/// folder's ID at most once per debounce window in which it changed.
/// A root that cannot be watched (e.g. it was deleted) is logged and skipped.
pub fn watch(
    folders: &[Folder],
    on_change: impl Fn(i64) + Send + 'static,
) -> Result<FolderWatcher, WatchError> {
    // Events may arrive with the canonical path (macOS reports /private/var
    // for /var), so match against both spellings of each root.
    let mut roots: Vec<(i64, PathBuf)> = Vec::new();
    for f in folders {
        roots.push((f.id, f.root.clone()));
        if let Ok(canonical) = std::fs::canonicalize(&f.root)
            && canonical != f.root
        {
            roots.push((f.id, canonical));
        }
    }
    let handler_roots = roots.clone();
    let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| match res {
        Ok(events) => {
            let changed: BTreeSet<i64> = events
                .iter()
                .filter_map(|e| folder_for(&handler_roots, &e.path))
                .collect();
            for id in changed {
                debug!("folder {id} changed on disk");
                on_change(id);
            }
        }
        Err(e) => warn!("file watcher error: {e}"),
    })?;
    for f in folders {
        if let Err(e) = debouncer.watcher().watch(&f.root, RecursiveMode::Recursive) {
            warn!(
                "cannot watch {} for changes ({e}); it will still be rescanned periodically",
                f.root.display()
            );
        }
    }
    Ok(FolderWatcher {
        _debouncer: debouncer,
    })
}

/// The folder `path` belongs to, or `None` if it is outside every root or is
/// a temp file or OS clutter.
fn folder_for(roots: &[(i64, PathBuf)], path: &Path) -> Option<i64> {
    // Longest root first would matter for nested roots; `nebula add` forbids them.
    let (id, rest) = roots
        .iter()
        .find_map(|(id, root)| path.strip_prefix(root).ok().map(|rest| (*id, rest)))?;
    let ignored = rest.components().any(|c| match c {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|n| n.starts_with(TEMP_PREFIX) || CLUTTER.contains(&n)),
        _ => false,
    });
    (!ignored).then_some(id)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    fn roots() -> Vec<(i64, PathBuf)> {
        vec![(1, PathBuf::from("/a/docs")), (2, PathBuf::from("/a/pics"))]
    }

    #[test]
    fn maps_paths_to_folders() {
        let r = roots();
        assert_eq!(folder_for(&r, Path::new("/a/docs/x.txt")), Some(1));
        assert_eq!(folder_for(&r, Path::new("/a/docs/sub/y")), Some(1));
        assert_eq!(folder_for(&r, Path::new("/a/docs")), Some(1));
        assert_eq!(folder_for(&r, Path::new("/a/pics/z.jpg")), Some(2));
        assert_eq!(folder_for(&r, Path::new("/a/docsx/z")), None);
        assert_eq!(folder_for(&r, Path::new("/elsewhere")), None);
    }

    #[test]
    fn ignores_temp_files_and_clutter() {
        let r = roots();
        let tmp = format!("/a/docs/{TEMP_PREFIX}abc");
        assert_eq!(folder_for(&r, Path::new(&tmp)), None);
        let in_tmp_dir = format!("/a/docs/{TEMP_PREFIX}dir/file");
        assert_eq!(folder_for(&r, Path::new(&in_tmp_dir)), None);
        assert_eq!(folder_for(&r, Path::new("/a/docs/sub/.DS_Store")), None);
        assert_eq!(folder_for(&r, Path::new("/a/docs/.DS_Store.txt")), Some(1));
    }

    fn folder(id: i64, root: &Path) -> Folder {
        Folder {
            id,
            root: root.to_path_buf(),
            label: format!("f{id}"),
            last_scan_ns: 0,
        }
    }

    /// Collects callbacks for `window` and returns the folder IDs seen.
    fn changes_within(rx: &mpsc::Receiver<i64>, window: Duration) -> Vec<i64> {
        let deadline = Instant::now() + window;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(id) => seen.push(id),
                Err(_) => break,
            }
        }
        seen
    }

    fn wait_for(rx: &mpsc::Receiver<i64>, id: i64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(got) if got == id => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    /// Lets the OS settle after creating the watch (FSEvents can report
    /// changes from just before it started) and drains what arrived.
    fn settle(rx: &mpsc::Receiver<i64>) {
        let _ = changes_within(rx, Duration::from_millis(1500));
    }

    #[test]
    fn create_modify_delete_trigger_their_folder() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let _w = watch(&[folder(1, a.path()), folder(2, b.path())], move |id| {
            let _ = tx.lock().unwrap().send(id);
        })
        .unwrap();
        settle(&rx);

        let file = a.path().join("note.txt");
        std::fs::write(&file, b"one").unwrap();
        assert!(wait_for(&rx, 1), "create not reported");
        settle(&rx);

        std::fs::write(&file, b"two, longer").unwrap();
        assert!(wait_for(&rx, 1), "modify not reported");
        settle(&rx);

        std::fs::remove_file(&file).unwrap();
        assert!(wait_for(&rx, 1), "delete not reported");
        settle(&rx);

        std::fs::write(b.path().join("pic.jpg"), b"x").unwrap();
        let seen = changes_within(&rx, Duration::from_secs(3));
        assert!(seen.contains(&2), "second folder not reported: {seen:?}");
        assert!(!seen.contains(&1), "wrong folder reported: {seen:?}");
    }

    #[test]
    fn temp_files_and_bursts() {
        let a = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let _w = watch(&[folder(1, a.path())], move |id| {
            let _ = tx.lock().unwrap().send(id);
        })
        .unwrap();
        settle(&rx);

        std::fs::write(a.path().join(format!("{TEMP_PREFIX}123")), b"partial").unwrap();
        std::fs::write(a.path().join(".DS_Store"), b"junk").unwrap();
        let seen = changes_within(&rx, Duration::from_secs(3));
        assert!(seen.is_empty(), "temp file reported: {seen:?}");

        // A burst of writes gives a few callbacks, not one per file. (The OS
        // may deliver it across several debounce windows.)
        for i in 0..50 {
            std::fs::write(a.path().join(format!("f{i}")), b"x").unwrap();
        }
        let seen = changes_within(&rx, Duration::from_secs(3));
        assert!(!seen.is_empty(), "burst not reported");
        assert!(seen.len() <= 6, "burst not debounced: {seen:?}");
    }

    #[test]
    fn missing_root_is_skipped() {
        let a = tempfile::tempdir().unwrap();
        let gone = a.path().join("does-not-exist");
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let _w = watch(&[folder(1, &gone), folder(2, a.path())], move |id| {
            let _ = tx.lock().unwrap().send(id);
        })
        .unwrap();
        settle(&rx);
        std::fs::write(a.path().join("x"), b"x").unwrap();
        assert!(wait_for(&rx, 2));
    }
}
