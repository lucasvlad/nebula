//! Fault-injection TCP proxy: client -> proxy -> server.
//!
//! Byte offsets in [`Fault`] are per connection and per direction: every new
//! connection starts counting at 0. `captured` and `bytes_forwarded` are
//! cumulative across all connections.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    fn index(self) -> usize {
        match self {
            Direction::ClientToServer => 0,
            Direction::ServerToClient => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Fault {
    None,
    /// Close both sides once this many bytes have passed in `dir`.
    DropAfter {
        dir: Direction,
        bytes: u64,
    },
    /// Delay every forwarded read by this long in `dir`.
    Delay {
        dir: Direction,
        by: Duration,
    },
    /// Forward only the first `bytes` bytes in `dir`, then stop forwarding that
    /// direction but keep the connection open.
    Truncate {
        dir: Direction,
        bytes: u64,
    },
    /// XOR the byte at stream offset `offset` in `dir` with `mask`.
    Corrupt {
        dir: Direction,
        offset: u64,
        mask: u8,
    },
}

struct Shared {
    fault: Mutex<Fault>,
    captured: [Mutex<Vec<u8>>; 2],
    forwarded: [AtomicU64; 2],
    connections: AtomicUsize,
}

/// Locks, ignoring poisoning (the data stays valid either way).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Proxy {
    addr: SocketAddr,
    shared: Arc<Shared>,
    listener: JoinHandle<()>,
}

impl Proxy {
    /// Listen on 127.0.0.1:0 and forward each accepted connection to `target`.
    pub async fn start(target: SocketAddr, fault: Fault) -> io::Result<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            fault: Mutex::new(fault),
            captured: [Mutex::new(Vec::new()), Mutex::new(Vec::new())],
            forwarded: [AtomicU64::new(0), AtomicU64::new(0)],
            connections: AtomicUsize::new(0),
        });
        let task_shared = shared.clone();
        // The JoinSet lives in the listener task, so aborting that task also
        // aborts every live connection task.
        let listener = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            while let Ok((client, _)) = listener.accept().await {
                task_shared.connections.fetch_add(1, Ordering::SeqCst);
                let fault = lock(&task_shared.fault).clone();
                conns.spawn(connection(client, target, fault, task_shared.clone()));
            }
        });
        Ok(Proxy {
            addr,
            shared,
            listener,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Change the fault for connections accepted from now on.
    pub fn set_fault(&self, fault: Fault) {
        *lock(&self.shared.fault) = fault;
    }

    /// All bytes forwarded so far in `dir` (post-corruption), in order.
    pub fn captured(&self, dir: Direction) -> Vec<u8> {
        lock(&self.shared.captured[dir.index()]).clone()
    }

    /// Total bytes forwarded in `dir`.
    pub fn bytes_forwarded(&self, dir: Direction) -> u64 {
        self.shared.forwarded[dir.index()].load(Ordering::SeqCst)
    }

    /// Number of connections accepted.
    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn connection(client: TcpStream, target: SocketAddr, fault: Fault, shared: Arc<Shared>) {
    let Ok(server) = TcpStream::connect(target).await else {
        return;
    };
    let (cr, cw) = client.into_split();
    let (sr, sw) = server.into_split();
    let a = pump(Direction::ClientToServer, cr, sw, &fault, &shared);
    let b = pump(Direction::ServerToClient, sr, cw, &fault, &shared);
    tokio::pin!(a, b);
    // Returning drops all four halves, closing both sockets. A pump that
    // ends with plain EOF only half-closes, so wait for the other one.
    tokio::select! {
        dropped = &mut a => if !dropped { b.await; },
        dropped = &mut b => if !dropped { a.await; },
    }
}

/// Forwards `dir` until EOF/error (returns false) or a drop fault (returns true).
async fn pump(
    dir: Direction,
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    fault: &Fault,
    shared: &Shared,
) -> bool {
    let mut buf = vec![0u8; 16 * 1024];
    let mut offset: u64 = 0; // bytes read so far on this connection
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                let _ = to.shutdown().await;
                return false;
            }
            Ok(n) => n,
        };
        let read = n as u64;
        let mut send = n;
        let mut drop_now = false;
        match *fault {
            Fault::Delay { dir: d, by } if d == dir => tokio::time::sleep(by).await,
            Fault::Corrupt {
                dir: d,
                offset: at,
                mask,
            } if d == dir && at >= offset && at - offset < read => {
                buf[(at - offset) as usize] ^= mask;
            }
            Fault::Truncate { dir: d, bytes } if d == dir => {
                send = bytes.saturating_sub(offset).min(read) as usize;
            }
            Fault::DropAfter { dir: d, bytes } if d == dir => {
                send = bytes.saturating_sub(offset).min(read) as usize;
                drop_now = offset + read >= bytes;
            }
            _ => {}
        }
        offset += read;
        if send > 0 {
            if to.write_all(&buf[..send]).await.is_err() {
                return false;
            }
            lock(&shared.captured[dir.index()]).extend_from_slice(&buf[..send]);
            shared.forwarded[dir.index()].fetch_add(send as u64, Ordering::SeqCst);
        }
        if drop_now {
            return true;
        }
    }
}

/// True if `needle` occurs in `haystack` (an empty needle always matches).
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}
