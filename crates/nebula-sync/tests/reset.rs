//! A device that loses its sync history (folder removed and re-added, or
//! `state.db` deleted) must not lose or hide edits afterwards. Its version
//! counters used to restart at 1 and collide with values peers had already
//! seen (audit finding 2).
#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use nebula_core::DeviceId;
use nebula_sync::engine::{Engine, Input, Output};
use nebula_sync::protocol::Message;
use nebula_sync::scanner::TEMP_PREFIX;
use nebula_sync::store::Store;

struct Dev {
    id: DeviceId,
    root: PathBuf,
    db: PathBuf,
    engine: Engine,
    _dir: tempfile::TempDir,
}

impl Dev {
    fn new(n: u8) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        fs::create_dir(&root).unwrap();
        let db = dir.path().join("state.db");
        let id = DeviceId([n; 32]);
        let engine = fresh_engine(&db, &root, id);
        Self {
            id,
            root,
            db,
            engine,
            _dir: dir,
        }
    }

    /// Forgets all sync history, like `nebula remove` + `nebula add`.
    fn lose_history(&mut self) {
        // Replace the engine first so the old database handle is closed.
        self.engine = Engine::new(Store::open_in_memory().unwrap(), self.id, "tmp".into()).unwrap();
        for ext in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{ext}", self.db.display()));
        }
        self.engine = fresh_engine(&self.db, &self.root, self.id);
    }

    fn write(&self, name: &str, data: &[u8]) {
        fs::write(self.root.join(name), data).unwrap();
    }

    fn read(&self, name: &str) -> Vec<u8> {
        fs::read(self.root.join(name)).unwrap()
    }

    fn scan(&mut self) {
        self.engine.handle(Input::LocalChange { folder: 1 });
    }

    /// Every synced file's contents, ignoring temp files.
    fn contents(&self) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = fs::read_dir(&self.root)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| !e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .map(|e| fs::read(e.path()).unwrap())
            .collect();
        v.sort();
        v
    }
}

fn fresh_engine(db: &Path, root: &Path, id: DeviceId) -> Engine {
    let store = Store::open(db).unwrap();
    store.add_folder(root, "shared").unwrap();
    Engine::new(store, id, format!("dev{}", id.0[0])).unwrap()
}

fn sync(a: &mut Dev, b: &mut Dev) {
    let mut q: VecDeque<(bool, Output)> = a
        .engine
        .handle(Input::Connected { peer: b.id })
        .into_iter()
        .map(|o| (true, o))
        .collect();
    q.extend(
        b.engine
            .handle(Input::Connected { peer: a.id })
            .into_iter()
            .map(|o| (false, o)),
    );
    while let Some((from_a, out)) = q.pop_front() {
        let msg = Message::decode(&out.msg.encode()).unwrap();
        let (to, from) = if from_a {
            (&mut *b, a.id)
        } else {
            (&mut *a, b.id)
        };
        q.extend(
            to.engine
                .handle(Input::Received { peer: from, msg })
                .into_iter()
                .map(|o| (!from_a, o)),
        );
    }
    a.engine.handle(Input::Disconnected { peer: b.id });
    b.engine.handle(Input::Disconnected { peer: a.id });
}

#[test]
fn edit_after_losing_history_is_not_overwritten() {
    let mut a = Dev::new(1);
    let mut b = Dev::new(2);
    b.write("doc.txt", b"original");
    b.scan();
    sync(&mut a, &mut b);
    // A edits, so its vector for doc.txt is ahead of B's old one.
    a.write("doc.txt", b"A's edit");
    a.scan();

    b.lose_history();
    b.write("doc.txt", b"B's important edit after re-adding the folder");
    b.scan();
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);

    // Both edits survive on both devices (one as a conflict copy).
    let want = {
        let mut v = vec![
            b"A's edit".to_vec(),
            b"B's important edit after re-adding the folder".to_vec(),
        ];
        v.sort();
        v
    };
    assert_eq!(a.contents(), want, "A lost an edit");
    assert_eq!(b.contents(), want, "B lost an edit");
}

#[test]
fn equal_counters_after_losing_history_do_not_diverge() {
    let mut a = Dev::new(1);
    let mut b = Dev::new(2);
    b.write("doc.txt", b"original");
    b.scan();
    sync(&mut a, &mut b);

    b.lose_history();
    b.write("doc.txt", b"B's edit after re-adding the folder");
    b.scan();
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);

    // B's new edit is newer than its own old version, so A takes it.
    assert_eq!(a.read("doc.txt"), b"B's edit after re-adding the folder");
    assert_eq!(a.contents(), b.contents(), "devices diverged");
}

#[test]
fn rescan_after_losing_history_keeps_peer_edits() {
    let mut a = Dev::new(1);
    let mut b = Dev::new(2);
    b.write("doc.txt", b"original");
    b.scan();
    sync(&mut a, &mut b);
    a.write("doc.txt", b"A edited while B was being reset");
    a.scan();

    // B re-adds the folder without touching the file.
    b.lose_history();
    b.scan();
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);

    // A's edit must not be replaced by B's stale copy.
    assert!(
        a.contents()
            .contains(&b"A edited while B was being reset".to_vec())
    );
    assert_eq!(a.contents(), b.contents(), "devices diverged");
}
