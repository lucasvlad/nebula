//! TEMPORARY audit repros. Delete after use.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use nebula_core::DeviceId;
use nebula_sync::engine::{Engine, Input, Output};
use nebula_sync::protocol::Message;
use nebula_sync::scanner::TEMP_PREFIX;
use nebula_sync::store::Store;

struct Dev {
    dir: tempfile::TempDir,
    state: tempfile::TempDir,
    engine: Engine,
    id: DeviceId,
}

impl Dev {
    fn new(n: u8, name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store = Store::open(&state.path().join("state.db")).unwrap();
        store.add_folder(dir.path(), "shared").unwrap();
        let id = DeviceId([n; 32]);
        let engine = Engine::new(store, id, name.to_string()).unwrap();
        Self { dir, state, engine, id }
    }
    fn db(&self) -> Store {
        Store::open(&self.state.path().join("state.db")).unwrap()
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }
    fn write(&self, rel: &str, data: &[u8]) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    fn scan(&mut self) -> Vec<Output> {
        self.engine.handle(Input::LocalChange { folder: 1 })
    }
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if !e.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn run(a: &mut Dev, b: &mut Dev, q0: Vec<(bool, Output)>) {
    let mut q: VecDeque<(bool, Output)> = q0.into();
    while let Some((from_a, out)) = q.pop_front() {
        let enc = out.msg.encode();
        assert!(enc.len() <= 16 * 1024 * 1024, "message of {} bytes > MAX_MESSAGE", enc.len());
        let msg = Message::decode(&enc).unwrap();
        let (to, from_id) = if from_a { (&mut *b, a.id) } else { (&mut *a, b.id) };
        let replies = to.engine.handle(Input::Received { peer: from_id, msg });
        q.extend(replies.into_iter().map(|o| (!from_a, o)));
    }
}

fn connect(a: &mut Dev, b: &mut Dev) {
    let oa = a.engine.handle(Input::Connected { peer: b.id });
    let ob = b.engine.handle(Input::Connected { peer: a.id });
    let mut q: Vec<(bool, Output)> = oa.into_iter().map(|o| (true, o)).collect();
    q.extend(ob.into_iter().map(|o| (false, o)));
    run(a, b, q);
}

fn disconnect(a: &mut Dev, b: &mut Dev) {
    a.engine.handle(Input::Disconnected { peer: b.id });
    b.engine.handle(Input::Disconnected { peer: a.id });
}

fn tick(a: &mut Dev, b: &mut Dev, a_ticks: bool) {
    let out = if a_ticks { a.engine.handle(Input::Tick) } else { b.engine.handle(Input::Tick) };
    run(a, b, out.into_iter().map(|o| (a_ticks, o)).collect());
}

fn change_on_a(a: &mut Dev, b: &mut Dev) {
    let out = a.scan();
    run(a, b, out.into_iter().map(|o| (true, o)).collect());
}

/// B edits a file it created, removes and re-adds the folder (or loses its
/// state.db). Its fresh version {B:1} equals A's, so B's edit never reaches A.
#[test]
fn readd_folder_equal_version_diverges() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    b.write("doc.txt", b"original");
    b.scan();
    connect(&mut a, &mut b);
    assert_eq!(fs::read(a.path("doc.txt")).unwrap(), b"original");
    disconnect(&mut a, &mut b);
    // `nebula remove` + `nebula add` on B (daemon stopped), then B edits.
    let db = b.db();
    db.remove_folder(b.dir.path()).unwrap();
    b.write("doc.txt", b"B's important edit after re-adding");
    db.add_folder(b.dir.path(), "shared").unwrap();
    let fid = db.folders().unwrap()[0].id;
    drop(db);
    b.engine.handle(Input::LocalChange { folder: fid });
    connect(&mut a, &mut b);
    tick(&mut a, &mut b, true);
    tick(&mut a, &mut b, false);
    println!("A: {:?}", String::from_utf8_lossy(&fs::read(a.path("doc.txt")).unwrap()));
    println!("B: {:?}", String::from_utf8_lossy(&fs::read(b.path("doc.txt")).unwrap()));
    assert_eq!(tree(a.dir.path()), tree(b.dir.path()), "diverged");
}

/// A edits once more after B synced; B re-adds and edits: {B:1} < {B:1,A:1}.
#[test]
fn readd_folder_older_version_overwrites_edit() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    b.write("doc.txt", b"original");
    b.scan();
    connect(&mut a, &mut b);
    disconnect(&mut a, &mut b);
    let db = b.db();
    db.remove_folder(b.dir.path()).unwrap();
    b.write("doc.txt", b"B's important edit after re-adding");
    db.add_folder(b.dir.path(), "shared").unwrap();
    let fid = db.folders().unwrap()[0].id;
    drop(db);
    std::thread::sleep(std::time::Duration::from_millis(20));
    a.write("doc.txt", b"A's edit");
    a.scan();
    b.engine.handle(Input::LocalChange { folder: fid });
    connect(&mut a, &mut b);
    let tb = tree(b.dir.path());
    println!("B files: {:?}", tb.iter().map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).to_string())).collect::<Vec<_>>());
    assert!(
        tb.values().any(|v| v == b"B's important edit after re-adding"),
        "B's edit was lost without a conflict copy"
    );
}

/// While the daemon runs: remove folder "shared" and add another folder
/// (gets the same SQLite id). The next tick reapplies the peer's cached offers.
#[test]
fn folder_id_reuse_leaks_files_into_new_folder() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("secret-of-shared.txt", b"only for the shared folder");
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    assert!(b.path("secret-of-shared.txt").exists());
    let other = tempfile::tempdir().unwrap();
    let db = b.db();
    db.remove_folder(b.dir.path()).unwrap();
    let f = db.add_folder(other.path(), "photos").unwrap();
    println!("new folder id {}", f.id);
    drop(db);
    tick(&mut a, &mut b, false);
    let leaked = tree(other.path());
    println!("photos folder now: {:?}", leaked.keys().collect::<Vec<_>>());
    assert!(leaked.is_empty(), "files of \"shared\" were written into \"photos\"");
}

/// macOS (case-insensitive): A renames a.txt -> A.txt. What does B end with
/// right after the announcement (before any tick)?
#[test]
fn case_only_rename() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("a.txt", b"data");
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    fs::rename(a.path("a.txt"), a.path("tmpname")).unwrap();
    fs::rename(a.path("tmpname"), a.path("A.txt")).unwrap();
    change_on_a(&mut a, &mut b);
    println!("A: {:?}", tree(a.dir.path()).keys().collect::<Vec<_>>());
    println!("B after announce: {:?}", tree(b.dir.path()).keys().collect::<Vec<_>>());
    let b_after = tree(b.dir.path());
    tick(&mut a, &mut b, false);
    println!("B after tick: {:?}", tree(b.dir.path()).keys().collect::<Vec<_>>());
    assert!(!b_after.is_empty(), "file missing on B until the next tick");
}

/// B has "Docs/x.txt"; A (Linux-like) has "docs/y.txt". On B the file lands in
/// "Docs/", and B's rescan re-announces it as a delete + add under a new path.
#[test]
fn dir_case_mismatch_churn() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    b.write("Docs/x.txt", b"x");
    b.scan();
    a.write("docs/y.txt", b"y");
    a.scan();
    connect(&mut a, &mut b);
    println!("A: {:?}", tree(a.dir.path()).keys().collect::<Vec<_>>());
    println!("B: {:?}", tree(b.dir.path()).keys().collect::<Vec<_>>());
    let out = b.scan();
    for o in &out {
        if let Message::Index { entries, .. } = &o.msg {
            for e in entries {
                println!("B announces {} deleted={}", e.path, e.deleted);
            }
        }
    }
}

/// Many empty files: does each Index batch fit in a 16 MiB message?
#[test]
fn index_batch_fits_message_limit() {
    let mut a = Dev::new(1, "A");
    for i in 0..120_000 {
        a.write(&format!("d/f{i:06}"), b"");
    }
    a.scan();
    let out = a.engine.handle(Input::Connected { peer: DeviceId([2; 32]) });
    for o in out {
        let n = o.msg.encode().len();
        println!("Index message of {n} bytes");
        assert!(n <= 16 * 1024 * 1024, "Index of {n} bytes exceeds MAX_MESSAGE and is dropped by the writer");
    }
}

/// Two identical files in one directory share a temp file name.
#[test]
fn duplicate_content_same_dir() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    a.write("one.bin", &data);
    a.write("two.bin", &data);
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    println!("B: {:?} pending {}", tree(b.dir.path()).keys().collect::<Vec<_>>(), b.engine.pending_downloads());
    assert_eq!(tree(a.dir.path()), tree(b.dir.path()), "second copy missing until a tick");
}
