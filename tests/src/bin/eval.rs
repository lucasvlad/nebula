//! Evaluation harness for the paper (brief §6): delta efficiency and resume
//! after a dropped connection, measured on two in-process sync engines with
//! real folders on disk. Every message is encoded as on the wire, so byte
//! counts are exact protocol bytes (before encryption, which adds 16 bytes
//! per Noise frame and 4 bytes of framing).
//!
//! Run: `cargo run --release -p nebula-tests --bin eval`
#![allow(clippy::unwrap_used)]

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::time::Instant;

use nebula_core::{DeviceId, Digest};
use nebula_sync::engine::{Engine, Input, Output};
use nebula_sync::protocol::Message;
use nebula_sync::store::Store;

const MB: usize = 1024 * 1024;

type Edit = Box<dyn Fn(&[u8]) -> Vec<u8>>;

struct Dev {
    dir: tempfile::TempDir,
    engine: Engine,
    id: DeviceId,
}

impl Dev {
    fn new(n: u8) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        store.add_folder(dir.path(), "eval").unwrap();
        let id = DeviceId([n; 32]);
        let engine = Engine::new(store, id, format!("dev{n}")).unwrap();
        Self { dir, engine, id }
    }
    fn file(&self) -> std::path::PathBuf {
        self.dir.path().join("data.bin")
    }
}

#[derive(Default, Clone, Copy)]
struct Traffic {
    /// File data inside Chunk messages.
    chunk_bytes: usize,
    /// Every encoded message, both directions (indexes, requests, chunks).
    protocol_bytes: usize,
    chunks: usize,
}

/// Delivers messages until quiet, or until `stop` says the link drops.
fn pump(
    a: &mut Dev,
    b: &mut Dev,
    start: Vec<(bool, Output)>,
    stop: impl Fn(&Traffic) -> bool,
) -> Traffic {
    let mut q: VecDeque<(bool, Output)> = start.into();
    let mut t = Traffic::default();
    while let Some((from_a, out)) = q.pop_front() {
        if stop(&t) {
            break;
        }
        let bytes = out.msg.encode();
        t.protocol_bytes += bytes.len();
        if let Message::Chunk { data, .. } = &out.msg {
            t.chunk_bytes += data.len();
            t.chunks += 1;
        }
        let msg = Message::decode(&bytes).unwrap();
        let (to, from) = if from_a {
            (&mut *b, a.id)
        } else {
            (&mut *a, b.id)
        };
        let replies = to.engine.handle(Input::Received { peer: from, msg });
        q.extend(replies.into_iter().map(|o| (!from_a, o)));
    }
    t
}

fn connect(a: &mut Dev, b: &mut Dev, stop: impl Fn(&Traffic) -> bool) -> Traffic {
    let mut start: Vec<(bool, Output)> = a
        .engine
        .handle(Input::Connected { peer: b.id })
        .into_iter()
        .map(|o| (true, o))
        .collect();
    start.extend(
        b.engine
            .handle(Input::Connected { peer: a.id })
            .into_iter()
            .map(|o| (false, o)),
    );
    pump(a, b, start, stop)
}

fn disconnect(a: &mut Dev, b: &mut Dev) {
    a.engine.handle(Input::Disconnected { peer: b.id });
    b.engine.handle(Input::Disconnected { peer: a.id });
}

/// Rescans A and delivers the announcement and the resulting transfer.
fn change(a: &mut Dev, b: &mut Dev) -> Traffic {
    let out = a.engine.handle(Input::LocalChange { folder: 1 });
    pump(a, b, out.into_iter().map(|o| (true, o)).collect(), |_| {
        false
    })
}

/// Bytes that cannot already appear in a `noise` file: BLAKE3 output.
/// (All xorshift seeds walk the same cycle, so two `noise` streams can
/// overlap and make inserted data look already-present.)
fn fresh(len: usize, label: &str) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new()
        .update(label.as_bytes())
        .finalize_xof()
        .fill(&mut out);
    out
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

/// Bytes a fixed-size-block scheme would send: every 8 KiB block of `new`
/// whose hash is not among `old`'s blocks (deduplicated, so a generous baseline).
fn fixed_block_bytes(old: &[u8], new: &[u8]) -> usize {
    const BLOCK: usize = 8 * 1024;
    let have: HashSet<Digest> = old.chunks(BLOCK).map(Digest::of).collect();
    let mut sent = HashSet::new();
    new.chunks(BLOCK)
        .filter(|b| !have.contains(&Digest::of(b)) && sent.insert(Digest::of(b)))
        .map(<[u8]>::len)
        .sum()
}

fn pct(part: usize, whole: usize) -> String {
    let p = part as f64 * 100.0 / whole as f64;
    if p < 0.01 {
        format!("{p:.4} %")
    } else {
        format!("{p:.2} %")
    }
}

fn kib(n: usize) -> String {
    if n >= MB {
        format!("{:.1} MiB", n as f64 / MB as f64)
    } else {
        format!("{:.1} KiB", n as f64 / 1024.0)
    }
}

fn delta_efficiency(size: usize) {
    println!("## Delta efficiency ({} MiB random file)\n", size / MB);
    println!(
        "| Change | File data sent | All protocol bytes | Sent / file size | Fixed 8 KiB blocks would send |"
    );
    println!("|---|---|---|---|---|");

    let original = noise(size, 7);
    let mid = size / 2;
    let scenarios: Vec<(&str, Edit)> = vec![
        (
            "Overwrite 1 byte in the middle",
            Box::new(move |d: &[u8]| {
                let mut v = d.to_vec();
                v[mid] ^= 0xff;
                v
            }),
        ),
        (
            "Insert 1 byte at the start",
            Box::new(|d: &[u8]| {
                let mut v = vec![b'!'];
                v.extend_from_slice(d);
                v
            }),
        ),
        (
            "Insert 1 KiB in the middle",
            Box::new(move |d: &[u8]| {
                let mut v = d[..mid].to_vec();
                v.extend_from_slice(&fresh(1024, "insert"));
                v.extend_from_slice(&d[mid..]);
                v
            }),
        ),
        // Not a multiple of the 8 KiB block size, so the fixed-block
        // baseline is not flattered by an aligned shift.
        (
            "Delete 1,000 bytes from the middle",
            Box::new(move |d: &[u8]| {
                let mut v = d[..mid].to_vec();
                v.extend_from_slice(&d[mid + 1000..]);
                v
            }),
        ),
        (
            "Append 1 MiB at the end",
            Box::new(|d: &[u8]| {
                let mut v = d.to_vec();
                v.extend_from_slice(&fresh(MB, "append"));
                v
            }),
        ),
    ];

    // Initial transfer as the baseline row.
    let mut a = Dev::new(1);
    let mut b = Dev::new(2);
    fs::write(a.file(), &original).unwrap();
    a.engine.handle(Input::LocalChange { folder: 1 });
    let t = connect(&mut a, &mut b, |_| false);
    assert_eq!(fs::read(b.file()).unwrap(), original);
    println!(
        "| First sync (no copy on the receiver) | {} | {} | {} | {} |",
        kib(t.chunk_bytes),
        kib(t.protocol_bytes),
        pct(t.chunk_bytes, size),
        kib(size)
    );

    for (name, edit) in scenarios {
        let mut a = Dev::new(1);
        let mut b = Dev::new(2);
        fs::write(a.file(), &original).unwrap();
        a.engine.handle(Input::LocalChange { folder: 1 });
        connect(&mut a, &mut b, |_| false);
        let edited = edit(&original);
        fs::write(a.file(), &edited).unwrap();
        let t = change(&mut a, &mut b);
        assert_eq!(fs::read(b.file()).unwrap(), edited, "{name}: copies differ");
        println!(
            "| {name} | {} | {} | {} | {} |",
            kib(t.chunk_bytes),
            kib(t.protocol_bytes),
            pct(t.chunk_bytes, edited.len()),
            kib(fixed_block_bytes(&original, &edited))
        );
    }
    println!(
        "\n\"All protocol bytes\" includes the index entry (36 bytes per chunk), chunk requests and \
         message headers, before Noise encryption (+20 bytes per frame of up to 64 KiB).\n"
    );
}

fn resume(size: usize) {
    println!(
        "## Resume after a dropped connection ({} MiB file)\n",
        size / MB
    );
    println!(
        "| Cut after | Data sent before the drop | Data sent after reconnecting | Total | Overhead vs. file size | Copy verified |"
    );
    println!("|---|---|---|---|---|---|");
    for fraction in [0.25, 0.5, 0.9] {
        let data = noise(size, 13);
        let mut a = Dev::new(1);
        let mut b = Dev::new(2);
        fs::write(a.file(), &data).unwrap();
        a.engine.handle(Input::LocalChange { folder: 1 });
        let cut = (size as f64 * fraction) as usize;
        let first = connect(&mut a, &mut b, |t| t.chunk_bytes >= cut);
        disconnect(&mut a, &mut b);
        let second = connect(&mut a, &mut b, |_| false);
        let ok = fs::read(b.file()).unwrap() == data;
        let total = first.chunk_bytes + second.chunk_bytes;
        println!(
            "| {:.0} % | {} | {} | {} | {} | {} |",
            fraction * 100.0,
            kib(first.chunk_bytes),
            kib(second.chunk_bytes),
            kib(total),
            pct(total.saturating_sub(size), size),
            if ok { "yes (BLAKE3 match)" } else { "NO" }
        );
    }
    println!(
        "\nOverhead is chunk data requested before the drop whose replies were lost in flight \
         (at most 32 requests).\n"
    );
}

fn main() {
    let started = Instant::now();
    println!("# Nebula evaluation: delta transfer and resume\n");
    println!(
        "Two sync engines in one process, real files on disk, every message encoded and decoded \
         as on the wire. Chunker: content-defined, 2 / 8 / 64 KiB (min / avg / max).\n"
    );
    delta_efficiency(100 * MB);
    resume(100 * MB);
    println!("_Generated in {:.1} s._", started.elapsed().as_secs_f64());
}
