//! Two sync engines wired together in memory, each with a real folder on
//! disk. Messages are encoded and decoded on the way, as on the wire.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use nebula_core::DeviceId;
use nebula_core::manifest::FileEntry;
use nebula_core::path::RelPath;
use nebula_sync::engine::{Engine, Input, MAX_ACTIVE_DOWNLOADS, Output};
use nebula_sync::protocol::Message;
use nebula_sync::scanner::TEMP_PREFIX;
use nebula_sync::store::Store;
use proptest::prelude::*;

/// The transport's per-message limit (`nebula_net::transport::MAX_MESSAGE`).
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

struct Dev {
    dir: tempfile::TempDir,
    engine: Engine,
    id: DeviceId,
    /// The synced folder: the temp dir itself, or `files/` in it.
    root: PathBuf,
}

impl Dev {
    fn new(n: u8, name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        store.add_folder(dir.path(), "shared").unwrap();
        let id = DeviceId([n; 32]);
        let engine = Engine::new(store, id, name.to_string()).unwrap();
        let root = dir.path().to_path_buf();
        Self {
            dir,
            engine,
            id,
            root,
        }
    }

    /// A device whose database is a file, so a test can change its folders
    /// the way `nebula add` / `nebula remove` (another process) would. The
    /// synced folder is `files/`, labelled `label` if given.
    fn on_disk(n: u8, name: &str, label: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        fs::create_dir(&root).unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        if let Some(label) = label {
            store.add_folder(&root, label).unwrap();
        }
        let id = DeviceId([n; 32]);
        let engine = Engine::new(store, id, name.to_string()).unwrap();
        Self {
            dir,
            engine,
            id,
            root,
        }
    }

    /// A second connection to an `on_disk` device's database.
    fn other_store(&self) -> Store {
        Store::open(&self.dir.path().join("state.db")).unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
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

/// Every file in a folder (excluding our temp files) and its contents.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if !e.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[derive(Default)]
struct Traffic {
    messages: usize,
    chunk_bytes: usize,
    requests: usize,
}

/// Delivers messages between `a` and `b` until both are quiet, or until
/// `limit` messages have been delivered (then the link "drops").
fn run(
    a: &mut Dev,
    b: &mut Dev,
    queue: VecDeque<Output>,
    from_a: bool,
    limit: Option<usize>,
) -> Traffic {
    run_checked(a, b, queue, from_a, limit, &mut |_, _| {})
}

/// [`run`], calling `check` after every delivered message.
fn run_checked(
    a: &mut Dev,
    b: &mut Dev,
    mut queue: VecDeque<Output>,
    from_a: bool,
    limit: Option<usize>,
    check: &mut dyn FnMut(&Dev, &Dev),
) -> Traffic {
    // Each queued item remembers who sent it.
    let mut q: VecDeque<(bool, Output)> = queue.drain(..).map(|o| (from_a, o)).collect();
    let mut t = Traffic::default();
    while let Some((sender_is_a, out)) = q.pop_front() {
        if limit.is_some_and(|l| t.messages >= l) {
            break;
        }
        t.messages += 1;
        match &out.msg {
            Message::Chunk { data, .. } => t.chunk_bytes += data.len(),
            Message::Request { .. } => t.requests += 1,
            _ => {}
        }
        let msg = Message::decode(&out.msg.encode()).unwrap();
        let (to, from_id) = if sender_is_a {
            (&mut *b, a.id)
        } else {
            (&mut *a, b.id)
        };
        assert_eq!(out.peer, to.id, "message addressed to the wrong device");
        let replies = to.engine.handle(Input::Received { peer: from_id, msg });
        q.extend(replies.into_iter().map(|o| (!sender_is_a, o)));
        check(a, b);
    }
    t
}

fn connect(a: &mut Dev, b: &mut Dev) -> Traffic {
    connect_limited(a, b, None)
}

fn connect_limited(a: &mut Dev, b: &mut Dev, limit: Option<usize>) -> Traffic {
    let out_a = a.engine.handle(Input::Connected { peer: b.id });
    let out_b = b.engine.handle(Input::Connected { peer: a.id });
    let mut q: VecDeque<(bool, Output)> = out_a.into_iter().map(|o| (true, o)).collect();
    q.extend(out_b.into_iter().map(|o| (false, o)));
    let mut t = Traffic::default();
    // Reuse `run` by splitting the initial queue per sender.
    let (qa, qb): (Vec<_>, Vec<_>) = q.into_iter().partition(|(s, _)| *s);
    let ta = run(a, b, qa.into_iter().map(|(_, o)| o).collect(), true, limit);
    let tb = run(a, b, qb.into_iter().map(|(_, o)| o).collect(), false, limit);
    t.messages = ta.messages + tb.messages;
    t.chunk_bytes = ta.chunk_bytes + tb.chunk_bytes;
    t.requests = ta.requests + tb.requests;
    t
}

fn disconnect(a: &mut Dev, b: &mut Dev) {
    a.engine.handle(Input::Disconnected { peer: b.id });
    b.engine.handle(Input::Disconnected { peer: a.id });
}

/// Scans `a` while connected and delivers the resulting announcements.
fn change_on(a: &mut Dev, b: &mut Dev) -> Traffic {
    let out = a.scan();
    run(a, b, out.into(), true, None)
}

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

fn set_mtime(p: &Path, secs_ago: u64) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

#[test]
fn initial_sync_copies_everything() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("notes.txt", b"hello");
    a.write("photos/2026/trip.jpg", &noise(300_000, 1));
    a.write("empty.txt", b"");
    b.write("from-b.txt", b"b was here");
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    assert_eq!(tree(a.dir.path()), tree(b.dir.path()));
    assert_eq!(tree(a.dir.path()).len(), 4);
    assert_eq!(
        a.engine.pending_downloads() + b.engine.pending_downloads(),
        0
    );
}

#[test]
fn events_describe_what_happened() {
    use nebula_sync::engine::Event;
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let mut big = noise(3_000_000, 21);
    a.write("big.bin", &big);
    a.scan();
    assert!(matches!(
        a.engine.take_events()[..],
        [Event::LocalChange { added: 1, .. }]
    ));
    connect(&mut a, &mut b);
    let ev = b.engine.take_events();
    assert!(ev.iter().any(|e| matches!(
        e,
        Event::Progress {
            total: 3_000_000,
            ..
        }
    )));
    assert!(ev.iter().any(|e| matches!(
        e,
        Event::FileReceived {
            size: 3_000_000,
            transferred: 3_000_000,
            ..
        }
    )));

    // A small edit: the event reports how little crossed the network.
    big[1_500_000] ^= 1;
    a.write("big.bin", &big);
    change_on(&mut a, &mut b);
    let Some(Event::FileReceived { transferred, .. }) = b
        .engine
        .take_events()
        .into_iter()
        .find(|e| matches!(e, Event::FileReceived { .. }))
    else {
        panic!("no FileReceived event");
    };
    assert!(transferred < 150_000, "transferred {transferred}");

    fs::remove_file(a.path("big.bin")).unwrap();
    change_on(&mut a, &mut b);
    assert!(
        b.engine
            .take_events()
            .iter()
            .any(|e| matches!(e, Event::FileDeleted { from, .. } if *from == a.id))
    );
}

#[test]
fn small_edit_sends_only_changed_chunks() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let mut big = noise(4_000_000, 2);
    a.write("big.bin", &big);
    a.scan();
    connect(&mut a, &mut b);
    big[2_000_000] ^= 0xff;
    a.write("big.bin", &big);
    let t = change_on(&mut a, &mut b);
    assert_eq!(fs::read(b.path("big.bin")).unwrap(), big);
    assert!(
        t.chunk_bytes < 150_000,
        "sent {} bytes for a 1-byte edit",
        t.chunk_bytes
    );
    assert!(t.requests <= 2);
}

#[test]
fn insert_at_start_reuses_almost_everything() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let big = noise(4_000_000, 3);
    a.write("big.bin", &big);
    a.scan();
    connect(&mut a, &mut b);
    let mut shifted = b"INSERTED".to_vec();
    shifted.extend_from_slice(&big);
    a.write("big.bin", &shifted);
    let t = change_on(&mut a, &mut b);
    assert_eq!(fs::read(b.path("big.bin")).unwrap(), shifted);
    assert!(
        t.chunk_bytes < 150_000,
        "sent {} bytes after an insert",
        t.chunk_bytes
    );
}

#[test]
fn deletes_propagate_and_recreate_works() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("doomed.txt", b"bye");
    a.write("keep.txt", b"stay");
    a.scan();
    connect(&mut a, &mut b);
    fs::remove_file(a.path("doomed.txt")).unwrap();
    change_on(&mut a, &mut b);
    assert!(!b.path("doomed.txt").exists());
    assert!(b.path("keep.txt").exists());

    b.write("doomed.txt", b"back again");
    change_on(&mut b, &mut a);
    assert_eq!(fs::read(a.path("doomed.txt")).unwrap(), b"back again");
}

#[test]
fn concurrent_edits_keep_both_versions() {
    let mut a = Dev::new(1, "Laptop");
    let mut b = Dev::new(2, "Desktop");
    a.write("report.txt", b"original");
    a.scan();
    connect(&mut a, &mut b);
    disconnect(&mut a, &mut b);

    a.write("report.txt", b"edited on the laptop");
    set_mtime(&a.path("report.txt"), 120);
    b.write("report.txt", b"edited on the desktop, later");
    set_mtime(&b.path("report.txt"), 60);
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    // A second round lets the merged version settle on both sides.
    disconnect(&mut a, &mut b);
    connect(&mut a, &mut b);

    let ta = tree(a.dir.path());
    assert_eq!(ta, tree(b.dir.path()), "devices did not converge");
    assert_eq!(ta["report.txt"], b"edited on the desktop, later");
    let copies: Vec<_> = ta
        .keys()
        .filter(|k| k.contains("conflict copy Laptop"))
        .collect();
    assert_eq!(copies.len(), 1, "{:?}", ta.keys());
    assert_eq!(
        ta[copies[0]], b"edited on the laptop",
        "the losing edit was lost"
    );
}

#[test]
fn edit_beats_concurrent_delete() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("draft.txt", b"v1");
    a.scan();
    connect(&mut a, &mut b);
    disconnect(&mut a, &mut b);
    fs::remove_file(a.path("draft.txt")).unwrap();
    b.write("draft.txt", b"v2, still needed");
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    disconnect(&mut a, &mut b);
    connect(&mut a, &mut b);
    assert_eq!(fs::read(a.path("draft.txt")).unwrap(), b"v2, still needed");
    assert_eq!(tree(a.dir.path()), tree(b.dir.path()));
}

#[test]
fn dropped_connection_resumes_without_resending() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let big = noise(3_000_000, 4);
    a.write("big.bin", &big);
    a.scan();
    // Let part of the transfer through, then cut the link.
    let first = connect_limited(&mut a, &mut b, Some(120));
    disconnect(&mut a, &mut b);
    assert!(!b.path("big.bin").exists());
    assert!(first.chunk_bytes > 0, "nothing transferred before the drop");

    let second = connect(&mut a, &mut b);
    assert_eq!(fs::read(b.path("big.bin")).unwrap(), big);
    assert!(
        first.chunk_bytes + second.chunk_bytes < big.len() + 200_000,
        "resent too much: {} + {} for {}",
        first.chunk_bytes,
        second.chunk_bytes,
        big.len()
    );
}

#[test]
fn local_edit_during_download_is_not_overwritten() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("shared.txt", b"from a");
    a.scan();
    // B creates the same file before it has scanned or synced.
    b.write("shared.txt", b"typed on b just now");
    connect(&mut a, &mut b);
    assert_eq!(
        fs::read(b.path("shared.txt")).unwrap(),
        b"typed on b just now"
    );
}

#[test]
fn changes_recorded_by_another_process_are_announced() {
    // A's engine uses a database file that `nebula scan` (another process)
    // also writes to.
    let a_dir = tempfile::tempdir().unwrap();
    let db = a_dir.path().join("state.db");
    let root = a_dir.path().join("files");
    fs::create_dir(&root).unwrap();
    let store = Store::open(&db).unwrap();
    store.add_folder(&root, "shared").unwrap();
    let mut a = Dev {
        dir: a_dir,
        engine: Engine::new(store, DeviceId([1; 32]), "A".into()).unwrap(),
        id: DeviceId([1; 32]),
        root: root.clone(),
    };
    let mut b = Dev::new(2, "B");
    connect(&mut a, &mut b);

    fs::write(root.join("by-hand.txt"), b"scanned by nebula scan").unwrap();
    let mut other = Store::open(&db).unwrap();
    let folder = other.folder_by_label("shared").unwrap().unwrap();
    nebula_sync::scanner::scan(&mut other, &folder, a.id).unwrap();

    // The daemon's own rescan finds nothing new on disk, but must still
    // tell B about the change the other process recorded.
    change_on(&mut a, &mut b);
    assert_eq!(
        fs::read(b.path("by-hand.txt")).unwrap(),
        b"scanned by nebula scan"
    );
}

#[test]
fn hostile_index_and_requests_are_refused() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("secret.txt", b"only for shared");
    a.scan();
    b.engine.handle(Input::Connected { peer: a.id });
    a.engine.handle(Input::Connected { peer: b.id });

    // An entry whose chunks don't add up is ignored.
    let mut bad = nebula_core::manifest::FileEntry {
        path: nebula_core::path::RelPath::new("evil.bin").unwrap(),
        size: 10,
        mtime_ns: 0,
        content_hash: nebula_core::Digest::of(b"x"),
        chunks: vec![],
        version: Default::default(),
        modified_by: a.id,
        deleted: false,
    };
    bad.version.increment(a.id);
    let out = b.engine.handle(Input::Received {
        peer: a.id,
        msg: Message::Index {
            folder: "shared".into(),
            entries: vec![bad],
        },
    });
    assert!(out.is_empty());
    assert_eq!(b.engine.pending_downloads(), 0);

    // Requests for another folder, or an unknown hash, get nothing.
    for (folder, hash) in [
        ("other", nebula_core::Digest::of(b"only for shared")),
        ("shared", nebula_core::Digest::of(b"not a chunk we have")),
    ] {
        let out = a.engine.handle(Input::Received {
            peer: b.id,
            msg: Message::Request {
                id: 1,
                folder: folder.into(),
                hash,
            },
        });
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].msg, Message::Missing { id: 1 }));
    }

    // Messages from a peer that is not connected are dropped.
    let out = a.engine.handle(Input::Received {
        peer: DeviceId([9; 32]),
        msg: Message::Request {
            id: 2,
            folder: "shared".into(),
            hash: nebula_core::Digest::of(b"only for shared"),
        },
    });
    assert!(out.is_empty());
}

/// Audit #1: hundreds of new files used to open a temp file each at once,
/// exhausting file descriptors (EMFILE) on macOS.
#[test]
fn many_new_files_download_within_the_cap() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    for i in 0..320 {
        a.write(
            &format!("many/f{i:03}.txt"),
            format!("file number {i}").as_bytes(),
        );
    }
    a.scan();
    let out_a = a.engine.handle(Input::Connected { peer: b.id });
    let out_b = b.engine.handle(Input::Connected { peer: a.id });
    let mut peak = 0;
    let mut check = |_: &Dev, b: &Dev| peak = peak.max(b.engine.active_downloads());
    run_checked(&mut a, &mut b, out_a.into(), true, None, &mut check);
    run_checked(&mut a, &mut b, out_b.into(), false, None, &mut check);
    assert!(
        peak <= MAX_ACTIVE_DOWNLOADS,
        "{peak} downloads were active at once"
    );
    assert!(peak > 0);
    assert_eq!(tree(a.dir.path()), tree(b.dir.path()));
    assert_eq!(b.engine.pending_downloads(), 0);
}

/// Audit #2: the batch size estimate ignored version vectors, so an Index
/// could exceed the transport limit and be dropped by the daemon.
#[test]
fn index_batches_with_large_vectors_fit_in_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let folder = store.add_folder(dir.path(), "shared").unwrap();
    // A history touched by 100 devices: ~3.3 KB of vector per entry.
    let mut version = nebula_core::version::VersionVector::default();
    for d in 0..100 {
        version.increment(DeviceId([d; 32]));
    }
    let entries: Vec<FileEntry> = (0..6000)
        .map(|i| FileEntry {
            path: RelPath::new(&format!("gone/{i:05}.txt")).unwrap(),
            size: 0,
            mtime_ns: 0,
            content_hash: nebula_core::Digest::of(b""),
            chunks: vec![],
            version: version.clone(),
            modified_by: DeviceId([1; 32]),
            deleted: true,
        })
        .collect();
    store.put_files(folder.id, &entries).unwrap();
    let mut a = Dev {
        root: dir.path().to_path_buf(),
        dir,
        engine: Engine::new(store, DeviceId([1; 32]), "A".into()).unwrap(),
        id: DeviceId([1; 32]),
    };
    let mut b = Dev::on_disk(2, "B", Some("shared"));

    let out_a = a.engine.handle(Input::Connected { peer: b.id });
    b.engine.handle(Input::Connected { peer: a.id });
    assert!(out_a.len() > 1, "expected several Index batches");
    for o in &out_a {
        let len = o.msg.encode().len();
        assert!(
            len <= MAX_MESSAGE,
            "an Index of {len} bytes would be dropped"
        );
    }
    run(&mut a, &mut b, out_a.into(), true, None);
    let other = b.other_store();
    let id = other.folder_by_label("shared").unwrap().unwrap().id;
    assert_eq!(other.files(id).unwrap().len(), 6000);
}

/// Audit #3: SQLite reuses a removed folder's id, and the engine's caches
/// (peer offers, known versions, downloads) used to carry over.
#[test]
fn reused_folder_id_does_not_leak_old_state() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::on_disk(2, "B", Some("shared"));
    a.write("old.txt", b"from the old folder");
    a.write("later.bin", &noise(50_000, 7));
    a.scan();
    connect(&mut a, &mut b);
    assert!(b.path("old.txt").exists());

    // `nebula remove` then `nebula add` of another directory.
    let other = b.other_store();
    let old_id = other.folder_by_label("shared").unwrap().unwrap().id;
    assert!(other.remove_folder(&b.root).unwrap());
    let new_root = b.dir.path().join("elsewhere");
    fs::create_dir(&new_root).unwrap();
    // Same bytes as old.txt, so a leaked offer could even complete locally.
    fs::write(new_root.join("copy.txt"), b"from the old folder").unwrap();
    let folder = other.add_folder(&new_root, "elsewhere").unwrap();
    assert_eq!(folder.id, old_id, "test premise: SQLite reused the id");

    let out = b.engine.handle(Input::Tick);
    assert!(
        !out.iter().any(|o| matches!(o.msg, Message::Request { .. })),
        "requested chunks of the old folder's files"
    );
    run(&mut a, &mut b, out.into(), false, None);
    let names: Vec<String> = tree(&new_root).into_keys().collect();
    assert_eq!(names, ["copy.txt"]);
    assert_eq!(b.engine.pending_downloads(), 0);
}

/// Audit #10: removing a folder mid-download must not finish the download
/// into the folder that is no longer synced.
#[test]
fn removed_folder_stops_its_downloads() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::on_disk(2, "B", Some("shared"));
    a.write("big.bin", &noise(300_000, 11));
    a.scan();
    let out_a = a.engine.handle(Input::Connected { peer: b.id });
    b.engine.handle(Input::Connected { peer: a.id });
    let mut requests = Vec::new();
    for o in out_a {
        requests.extend(b.engine.handle(Input::Received {
            peer: a.id,
            msg: o.msg,
        }));
    }
    assert!(!requests.is_empty());

    assert!(b.other_store().remove_folder(&b.root).unwrap());
    b.engine.handle(Input::Tick);
    run(&mut a, &mut b, requests.into(), false, None);
    assert!(!b.path("big.bin").exists());
    assert_eq!(b.engine.pending_downloads(), 0);
}

/// Audit #4: a folder added while connected only learned the peer's files
/// on the next reconnect.
#[test]
fn folder_added_while_connected_gets_existing_files() {
    let mut a = Dev::new(1, "A");
    a.write("existing.txt", b"already on a");
    a.write("sub/more.bin", &noise(100_000, 8));
    a.scan();
    let mut b = Dev::on_disk(2, "B", None);
    connect(&mut a, &mut b);

    b.other_store().add_folder(&b.root, "shared").unwrap();
    let out = b.engine.handle(Input::Tick);
    assert!(
        out.iter()
            .any(|o| matches!(&o.msg, Message::Resend { folder } if folder == "shared"))
    );
    run(&mut a, &mut b, out.into(), false, None);
    assert_eq!(tree(a.dir.path()), tree(&b.root));
}

/// Audit #5: a case-only rename arrives as a tombstone and a new entry in
/// one Index. Applied in path order, the new name was skipped as a case
/// clash with the old one (still present), so the file vanished.
#[test]
fn case_only_rename_applies_in_one_exchange() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("a.txt", b"letter case");
    a.scan();
    connect(&mut a, &mut b);
    assert!(b.path("a.txt").exists());

    fs::rename(a.path("a.txt"), a.path("A.txt")).unwrap();
    change_on(&mut a, &mut b);
    assert_eq!(
        tree(b.dir.path()),
        BTreeMap::from([("A.txt".to_string(), b"letter case".to_vec())])
    );
    assert!(b.scan().is_empty(), "rescan found churn");
}

/// Audit #6: `docs/y.txt` from a peer would land in the local `Docs/` on a
/// case-insensitive filesystem (and churn), or beside it elsewhere.
#[test]
fn differently_cased_directory_is_skipped() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    a.write("docs/y.txt", b"remote");
    b.write("Docs/x.txt", b"local");
    a.scan();
    b.scan();
    connect(&mut a, &mut b);
    let names = |d: &Dev| tree(&d.root).into_keys().collect::<Vec<_>>();
    assert_eq!(names(&b), ["Docs/x.txt"]);
    assert_eq!(names(&a), ["docs/y.txt"]);
    assert!(b.scan().is_empty(), "rescan found churn on b");
    assert!(a.scan().is_empty(), "rescan found churn on a");
}

/// Audit #7: identical files in one directory shared a temp file name.
#[test]
fn identical_files_in_one_directory_both_arrive() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let data = noise(200_000, 9);
    a.write("dup/one.bin", &data);
    a.write("dup/two.bin", &data);
    a.scan();
    connect(&mut a, &mut b);
    assert_eq!(tree(a.dir.path()), tree(b.dir.path()));
    assert_eq!(b.engine.pending_downloads(), 0);
    assert!(b.scan().is_empty(), "rescan found churn");
}

/// Audit #9: a Chunk or Missing reply must come from the peer the request
/// went to; anyone else could cancel or stall the download.
#[test]
fn chunk_reply_from_another_peer_is_ignored() {
    let mut a = Dev::new(1, "A");
    let mut b = Dev::new(2, "B");
    let big = noise(300_000, 10);
    a.write("big.bin", &big);
    a.scan();
    let intruder = DeviceId([3; 32]);
    b.engine.handle(Input::Connected { peer: intruder });
    let out_a = a.engine.handle(Input::Connected { peer: b.id });
    b.engine.handle(Input::Connected { peer: a.id });
    let mut requests = Vec::new();
    for o in out_a {
        requests.extend(b.engine.handle(Input::Received {
            peer: a.id,
            msg: o.msg,
        }));
    }
    let ids: Vec<u64> = requests
        .iter()
        .filter_map(|o| match o.msg {
            Message::Request { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert!(!ids.is_empty());
    for (n, &id) in ids.iter().enumerate() {
        let msg = if n % 2 == 0 {
            Message::Missing { id }
        } else {
            Message::Chunk {
                id,
                data: b"forged".to_vec(),
            }
        };
        b.engine.handle(Input::Received {
            peer: intruder,
            msg,
        });
    }
    assert_eq!(b.engine.pending_downloads(), 1, "the intruder cancelled it");
    run(&mut a, &mut b, requests.into(), false, None);
    assert_eq!(fs::read(b.path("big.bin")).unwrap(), big);
}

#[derive(Debug, Clone)]
enum Op {
    Write {
        on_a: bool,
        file: u8,
        seed: u32,
        len: u16,
    },
    Delete {
        on_a: bool,
        file: u8,
    },
    Sync,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<bool>(), 0u8..4, any::<u32>(), 0u16..20_000)
            .prop_map(|(on_a, file, seed, len)| Op::Write { on_a, file, seed, len }),
        2 => (any::<bool>(), 0u8..4).prop_map(|(on_a, file)| Op::Delete { on_a, file }),
        2 => Just(Op::Sync),
    ]
}

proptest! {
    // 24 cases by default; set PROPTEST_CASES for a longer evaluation run.
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(24),
        ..ProptestConfig::default()
    })]

    /// Brief §6: randomized operations on two peers always converge.
    #[test]
    fn random_operations_converge(ops in prop::collection::vec(op(), 1..14)) {
        let mut a = Dev::new(1, "A");
        let mut b = Dev::new(2, "B");
        for op in ops {
            match op {
                Op::Write { on_a, file, seed, len } => {
                    let d = if on_a { &a } else { &b };
                    d.write(&format!("dir{}/f{file}.bin", file % 2), &noise(len as usize, seed));
                }
                Op::Delete { on_a, file } => {
                    let d = if on_a { &a } else { &b };
                    let _ = fs::remove_file(d.path(&format!("dir{}/f{file}.bin", file % 2)));
                }
                Op::Sync => {
                    a.scan();
                    b.scan();
                    connect(&mut a, &mut b);
                    disconnect(&mut a, &mut b);
                }
            }
        }
        // Final reconciliation: a couple of rounds settle merged versions.
        for _ in 0..3 {
            a.scan();
            b.scan();
            connect(&mut a, &mut b);
            disconnect(&mut a, &mut b);
        }
        prop_assert_eq!(tree(a.dir.path()), tree(b.dir.path()));
    }
}
