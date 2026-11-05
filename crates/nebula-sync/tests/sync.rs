//! Two sync engines wired together in memory, each with a real folder on
//! disk. Messages are encoded and decoded on the way, as on the wire.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use nebula_core::DeviceId;
use nebula_sync::engine::{Engine, Input, Output};
use nebula_sync::protocol::Message;
use nebula_sync::scanner::TEMP_PREFIX;
use nebula_sync::store::Store;
use proptest::prelude::*;

struct Dev {
    dir: tempfile::TempDir,
    engine: Engine,
    id: DeviceId,
}

impl Dev {
    fn new(n: u8, name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        store.add_folder(dir.path(), "shared").unwrap();
        let id = DeviceId([n; 32]);
        let engine = Engine::new(store, id, name.to_string()).unwrap();
        Self { dir, engine, id }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
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
    mut queue: VecDeque<Output>,
    from_a: bool,
    limit: Option<usize>,
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
