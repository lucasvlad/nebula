//! One task per encrypted session, in either direction, and the registry of
//! the sessions the engine talks through.
//!
//! After the Noise handshake both sides send `Hello`. A session whose first
//! message is `Hello` is a sync session: it is registered (at most one per
//! peer, see [`keep_new`]) and the engine is told `Connected`. A session
//! that opens with `Ping` or `Note` instead is `nebula ping`: it is answered
//! but never registered, so it cannot displace the real sync session.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use nebula_core::DeviceId;
use nebula_net::transport::{self, RecvHalf, SendHalf, Session, TransportError};
use nebula_sync::engine::Input;
use nebula_sync::protocol::Message;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::Daemon;
use super::engine::EngineHandle;
use crate::run::{check_protocol, hello, is_closed, is_hangup};

/// The peer must say hello within this long after the handshake.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Sent on every session this often, so a dead connection is noticed.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// A session that receives nothing for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Messages queued for one peer. A full queue means the peer is not reading
/// (each reply can be a 64 KiB chunk), so the session is closed rather than
/// buffering without limit.
const OUTBOUND: usize = 1024;
/// Chunk requests a peer may have unanswered. A well-behaved peer keeps at
/// most 32 in flight per download and runs at most 16 downloads at once
/// (512); the rest is slack. Past this the peer is not reading our replies.
const MAX_IN_FLIGHT: usize = 600;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Duplicate-session rule. Both devices may dial each other at once; each
/// side then sees two sessions for the same peer and must keep the same one.
/// Keep the session whose initiator has the lower device ID; both devices
/// compute the same answer. With the same initiator on both (the peer
/// redialled because it lost the old one), the new session wins.
pub fn keep_new(existing_initiator: &DeviceId, new_initiator: &DeviceId) -> bool {
    new_initiator <= existing_initiator
}

struct Live {
    id: u64,
    initiator: DeviceId,
    out: mpsc::Sender<Message>,
    /// Dropping this closes the session.
    _close: oneshot::Sender<()>,
}

/// Whether [`Registry::send`] queued a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Queued {
    Yes,
    NotConnected,
    /// The peer's queue was full, so its session was closed.
    Full,
}

/// Counts a session's chunk requests that have not been answered yet.
#[derive(Default)]
struct InFlight(AtomicUsize);

impl InFlight {
    /// A request arrived. `false` if the peer now has too many unanswered.
    fn request(&self) -> bool {
        self.0.fetch_add(1, Ordering::Relaxed) < MAX_IN_FLIGHT
    }

    /// A reply was sent. Saturates: a reply can belong to a request made on
    /// an earlier session with the same peer.
    fn answered(&self) {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

/// Registered sync sessions, one per peer.
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<HashMap<DeviceId, Live>>>);

impl Registry {
    fn lock(&self) -> MutexGuard<'_, HashMap<DeviceId, Live>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `msg` for `peer` without waiting. If its queue is full, the
    /// session is closed (unregistered here; the caller tells the engine).
    pub fn send(&self, peer: &DeviceId, msg: Message) -> Queued {
        let mut map = self.lock();
        let Some(live) = map.get(peer) else {
            return Queued::NotConnected;
        };
        match live.out.try_send(msg) {
            Ok(()) => Queued::Yes,
            // Its task is ending and will unregister it.
            Err(mpsc::error::TrySendError::Closed(_)) => Queued::NotConnected,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Dropping its close handle ends the session's task.
                map.remove(peer);
                Queued::Full
            }
        }
    }

    pub fn is_live(&self, peer: &DeviceId) -> bool {
        self.lock().contains_key(peer)
    }

    pub fn close_all(&self) {
        self.lock().clear();
    }

    /// Ends sessions with devices that are no longer paired, so `nebula
    /// unpair` takes effect on a running daemon too. Returns who was cut off.
    pub fn drop_unpaired(&self, paired: &[DeviceId], engine: &EngineHandle) -> Vec<DeviceId> {
        let mut map = self.lock();
        let gone: Vec<DeviceId> = map
            .keys()
            .filter(|d| !paired.contains(d))
            .copied()
            .collect();
        for peer in &gone {
            map.remove(peer);
            engine.send(Input::Disconnected { peer: *peer });
        }
        gone
    }

    /// Registers a session unless a better one exists. Engine inputs are sent
    /// under the lock, so they are queued in the order sessions change.
    fn register(&self, peer: DeviceId, live: Live, engine: &EngineHandle) -> bool {
        let mut map = self.lock();
        if let Some(old) = map.get(&peer) {
            if !keep_new(&old.initiator, &live.initiator) {
                return false;
            }
            // Replacing it drops its close handle, which ends its task.
            engine.send(Input::Disconnected { peer });
        }
        map.insert(peer, live);
        engine.send(Input::Connected { peer });
        true
    }

    fn unregister(&self, peer: DeviceId, id: u64, engine: &EngineHandle) {
        let mut map = self.lock();
        if map.get(&peer).is_some_and(|l| l.id == id) {
            map.remove(&peer);
            engine.send(Input::Disconnected { peer });
        }
    }
}

/// Accepts a connection: handshake with the allowlist, then [`run`].
/// `handshake` is held until the peer has proved it is paired.
pub async fn incoming(
    d: Arc<Daemon>,
    stream: TcpStream,
    addr: SocketAddr,
    handshake: OwnedSemaphorePermit,
) {
    let allowed = |key: &[u8; 32]| d.store().is_paired(key).unwrap_or(false);
    let accepted = transport::accept(stream, &d.identity, allowed).await;
    drop(handshake);
    let session = match accepted {
        Ok(s) => s,
        Err(TransportError::NotAllowed(device)) => {
            warn!(
                "refused a connection from {addr}: device {} is not paired with this one",
                device.short()
            );
            return;
        }
        Err(TransportError::Io(e)) if is_closed(&e) => {
            info!(
                "{addr} hung up during the secure handshake \
                 (probably a device that is not paired with this one)"
            );
            return;
        }
        Err(e) => {
            warn!("connection from {addr} failed during the secure handshake: {e}");
            return;
        }
    };
    let initiator = session.remote_device();
    run(d, session, addr, initiator).await;
}

/// Runs an established session, in either direction, until it ends.
pub async fn run(
    d: Arc<Daemon>,
    session: Session<TcpStream>,
    addr: SocketAddr,
    initiator: DeviceId,
) {
    let peer = session.remote_device();
    let peer_name = d.peer_name(&peer);
    let dir = if initiator == d.me { "to" } else { "from" };
    let conn = format!("connection {dir} {addr}");
    info!(
        "encrypted session with \"{peer_name}\" ({}) over the {conn}; security code: {}",
        peer.short(),
        session.security_code().to_string().to_uppercase()
    );
    match drive(&d, session, initiator, &peer_name, &conn).await {
        Ok(true) => info!("\"{peer_name}\" disconnected ({conn})"),
        Ok(false) => {}
        Err(e) => warn!("session with \"{peer_name}\" ended ({conn}): {e}"),
    }
}

async fn drive(
    d: &Daemon,
    session: Session<TcpStream>,
    initiator: DeviceId,
    peer_name: &str,
    conn: &str,
) -> Result<bool> {
    let peer = session.remote_device();
    let (mut tx, mut rx) = session.into_split();
    send(&mut tx, &hello(&d.name)).await?;
    let first = tokio::time::timeout(HELLO_TIMEOUT, recv(&mut rx))
        .await
        .map_err(|_| anyhow!("it did not say hello within {HELLO_TIMEOUT:?}"))??;
    let (out_tx, out_rx) = mpsc::channel(OUTBOUND);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (closed, pending) = match first {
        Message::Hello { protocol, .. } => {
            check_protocol(protocol)?;
            let (close_tx, close_rx) = oneshot::channel();
            let live = Live {
                id,
                initiator,
                out: out_tx.clone(),
                _close: close_tx,
            };
            if !d.sessions.register(peer, live, &d.engine) {
                info!("already connected to \"{peer_name}\"; closing the duplicate {conn}");
                return Ok(false);
            }
            (Some(close_rx), None)
        }
        // `nebula ping`: answered, never registered.
        m @ (Message::Ping { .. } | Message::Note { .. }) => {
            debug!("\"{peer_name}\" opened a ping session");
            (None, Some(m))
        }
        other => bail!(
            "it did not say hello first (sent {}); closing",
            kind(&other)
        ),
    };
    let sync = closed.is_some();
    let closed = async {
        match closed {
            Some(rx) => {
                let _ = rx.await;
            }
            None => std::future::pending().await,
        }
    };
    let in_flight = InFlight::default();
    let res = tokio::select! {
        r = reader(d, &mut rx, &out_tx, &in_flight, peer, peer_name, sync, pending) => r.map(|()| true),
        r = writer(&mut tx, out_rx, &in_flight) => r.map(|()| true),
        _ = closed => {
            // Replaced by a better session, or the daemon is stopping.
            if d.sessions.is_live(&peer) {
                info!("switched to another connection to \"{peer_name}\"; closing the {conn}");
            }
            Ok(false)
        }
    };
    if sync {
        d.sessions.unregister(peer, id, &d.engine);
    }
    res
}

#[allow(clippy::too_many_arguments)]
async fn reader(
    d: &Daemon,
    rx: &mut RecvHalf<TcpStream>,
    out: &mpsc::Sender<Message>,
    in_flight: &InFlight,
    peer: DeviceId,
    peer_name: &str,
    sync: bool,
    mut pending: Option<Message>,
) -> Result<()> {
    loop {
        let msg = match pending.take() {
            Some(m) => m,
            None => match tokio::time::timeout(IDLE_TIMEOUT, recv(rx)).await {
                Err(_) => bail!("nothing received for {IDLE_TIMEOUT:?}; the connection seems dead"),
                Ok(Err(e)) if is_hangup(&e) => return Ok(()),
                Ok(r) => r?,
            },
        };
        match msg {
            Message::Ping { nonce } => {
                // Dropped if the queue is full; the peer only loses a pong.
                let _ = out.try_send(Message::Pong { nonce });
            }
            Message::Pong { .. } => {}
            Message::Hello { .. } => debug!("\"{peer_name}\" said hello again; ignored"),
            Message::Note { text } => {
                let text: String = text.chars().filter(|c| !c.is_control()).take(500).collect();
                info!("\"{peer_name}\" sent a message, decrypted here: \"{text}\"");
            }
            msg if sync => {
                if matches!(msg, Message::Request { .. }) && !in_flight.request() {
                    bail!(
                        "it has more than {MAX_IN_FLIGHT} chunk requests unanswered \
                         (it is not reading the replies); closing"
                    );
                }
                d.engine.received(peer, msg).await;
            }
            msg => debug!("ignored {} on a ping session", kind(&msg)),
        }
    }
}

/// Sends queued messages, plus a keepalive ping every [`KEEPALIVE`]. Runs
/// until the session ends (the reader stops, or the registry closes it).
async fn writer(
    tx: &mut SendHalf<TcpStream>,
    mut queue: mpsc::Receiver<Message>,
    in_flight: &InFlight,
) -> Result<()> {
    let start = tokio::time::Instant::now() + KEEPALIVE;
    let mut keepalive = tokio::time::interval_at(start, KEEPALIVE);
    loop {
        let msg = tokio::select! {
            m = queue.recv() => match m {
                Some(m) => m,
                None => return Ok(()),
            },
            _ = keepalive.tick() => Message::Ping { nonce: 0 },
        };
        match send(tx, &msg).await {
            Err(e) if is_too_large(&e) => warn!("not sending a {}: {e}", kind(&msg)),
            r => r?,
        }
        if matches!(msg, Message::Chunk { .. } | Message::Missing { .. }) {
            in_flight.answered();
        }
    }
}

async fn send(tx: &mut SendHalf<TcpStream>, m: &Message) -> Result<()> {
    tx.send(&m.encode()).await?;
    Ok(())
}

async fn recv(rx: &mut RecvHalf<TcpStream>) -> Result<Message> {
    let bytes = rx.recv().await?;
    Message::decode(&bytes).map_err(|e| anyhow!("peer sent a malformed message: {e}"))
}

fn is_too_large(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<TransportError>(),
        Some(TransportError::MessageTooLarge(_))
    )
}

/// A message's name, for logs (never its contents).
fn kind(m: &Message) -> &'static str {
    match m {
        Message::Hello { .. } => "Hello",
        Message::Ping { .. } => "Ping",
        Message::Pong { .. } => "Pong",
        Message::Note { .. } => "Note",
        Message::Index { .. } => "Index",
        Message::Request { .. } => "Request",
        Message::Chunk { .. } => "Chunk",
        Message::Missing { .. } => "Missing",
        Message::Resend { .. } => "Resend",
    }
}

#[cfg(test)]
mod tests {
    use super::super::engine::Job;
    use super::*;

    const LOW: DeviceId = DeviceId([0x01; 32]);
    const HIGH: DeviceId = DeviceId([0xf0; 32]);

    #[test]
    fn lower_initiator_wins() {
        assert!(keep_new(&HIGH, &LOW));
        assert!(!keep_new(&LOW, &HIGH));
    }

    #[test]
    fn both_sides_agree() {
        // A dials B (initiator A) while B dials A (initiator B). Whichever
        // session each side registered first, both end up on the same one.
        for (a, b) in [(LOW, HIGH), (HIGH, LOW)] {
            let survivor = |first: DeviceId, second: DeviceId| {
                if keep_new(&first, &second) {
                    second
                } else {
                    first
                }
            };
            assert_eq!(survivor(a, b), survivor(b, a));
            assert_eq!(survivor(a, b), a.min(b));
        }
    }

    #[test]
    fn redial_by_same_initiator_replaces_stale_session() {
        assert!(keep_new(&HIGH, &HIGH));
        assert!(keep_new(&LOW, &LOW));
    }

    fn live(
        id: u64,
        initiator: DeviceId,
    ) -> (Live, mpsc::Receiver<Message>, oneshot::Receiver<()>) {
        let (out, rx) = mpsc::channel(2);
        let (close, closed) = oneshot::channel();
        (
            Live {
                id,
                initiator,
                out,
                _close: close,
            },
            rx,
            closed,
        )
    }

    #[test]
    fn registry_dedupes_and_unregisters_by_session() {
        let (engine, inputs) = EngineHandle::for_test();
        let reg = Registry::default();
        let peer = DeviceId([0x55; 32]);

        let (s1, _q1, mut closed1) = live(1, HIGH);
        assert!(reg.register(peer, s1, &engine));
        // A session from a higher initiator than the live one is refused.
        let (s2, _q2, _c2) = live(2, DeviceId([0xff; 32]));
        assert!(!reg.register(peer, s2, &engine));
        // A lower one replaces the live one, which is told to close.
        let (s3, mut q3, _c3) = live(3, LOW);
        assert!(reg.register(peer, s3, &engine));
        assert_eq!(
            closed1.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );

        assert_eq!(reg.send(&peer, Message::Ping { nonce: 9 }), Queued::Yes);
        assert_eq!(q3.try_recv().ok(), Some(Message::Ping { nonce: 9 }));
        // The replaced session ending must not unregister its successor.
        reg.unregister(peer, 1, &engine);
        assert!(reg.is_live(&peer));
        reg.unregister(peer, 3, &engine);
        assert!(!reg.is_live(&peer));
        assert_eq!(
            reg.send(&peer, Message::Ping { nonce: 1 }),
            Queued::NotConnected
        );

        let seen: Vec<&str> = inputs
            .try_iter()
            .map(|job| match job {
                Job::Input(Input::Connected { peer: p }, _) if p == peer => "connected",
                Job::Input(Input::Disconnected { peer: p }, _) if p == peer => "disconnected",
                _ => "other",
            })
            .collect();
        assert_eq!(
            seen,
            ["connected", "disconnected", "connected", "disconnected"]
        );
    }

    #[test]
    fn full_queue_closes_the_session() {
        let (engine, _inputs) = EngineHandle::for_test();
        let reg = Registry::default();
        let peer = DeviceId([0x66; 32]);
        let (s, _q, mut closed) = live(1, LOW);
        assert!(reg.register(peer, s, &engine));
        // The test queue holds two messages; nothing reads it.
        assert_eq!(reg.send(&peer, Message::Missing { id: 1 }), Queued::Yes);
        assert_eq!(reg.send(&peer, Message::Missing { id: 2 }), Queued::Yes);
        assert_eq!(reg.send(&peer, Message::Missing { id: 3 }), Queued::Full);
        assert!(!reg.is_live(&peer));
        assert_eq!(closed.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    }

    #[test]
    fn in_flight_requests_are_capped() {
        let f = InFlight::default();
        for _ in 0..MAX_IN_FLIGHT {
            assert!(f.request());
        }
        assert!(!f.request());
        // Answers make room again, and never go below zero.
        let f = InFlight::default();
        f.answered();
        for _ in 0..MAX_IN_FLIGHT {
            assert!(f.request());
            f.answered();
        }
        for _ in 0..MAX_IN_FLIGHT {
            assert!(f.request());
        }
        assert!(!f.request());
    }
}
