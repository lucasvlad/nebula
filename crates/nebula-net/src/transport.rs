//! Encrypted, mutually authenticated sessions between paired devices.
//!
//! Every connection runs a Noise XX handshake (ADR 0004) with the prologue
//! `nebula/1`. The remote static key is checked against the caller's
//! allowlist as soon as it arrives, and an unknown key ends the connection
//! before any application data is exchanged:
//!
//! ```text
//! initiator                         responder
//!   -> e
//!                                   <- e, ee, s, es
//!   check responder key; on reject drop (our static is never sent)
//!   -> s, se
//!                                   check initiator key; on reject drop
//!                                   <- empty transport message ("accepted")
//! ```
//!
//! The final empty message lets [`connect`] return `Ok` only once the
//! responder has accepted us, so an unpaired initiator gets an error rather
//! than a session that dies on first use.
//!
//! Application messages may be larger than one Noise message. Each is sent as
//! one or more encrypted frames: the first plaintext starts with the total
//! length (u32 big-endian), and continuation frames follow until that total
//! is reached. Noise's per-direction nonce counters make a dropped, replayed
//! or reordered frame fail to decrypt, and any failure ends the session.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nebula_core::DeviceId;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};

use crate::frame::{self, MAX_NOISE_FRAME};
use crate::identity::{Identity, NOISE_PARAMS};
use crate::sas::SecurityCode;

/// Largest application message accepted by [`Session::send`] and
/// [`Session::recv`].
/// Large enough for the chunk list of a ~3.5 GB file (36 bytes per 8 KiB
/// chunk), still small enough that a hostile peer cannot exhaust memory.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// A peer that has not finished the handshake by then is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound into the handshake hash, so peers speaking another protocol version
/// fail the handshake instead of misreading each other.
const PROLOGUE: &[u8] = b"nebula/1";

const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const LEN_PREFIX: usize = 4;
/// Largest plaintext that fits in one Noise transport message.
const MAX_PLAINTEXT: usize = MAX_NOISE_FRAME - TAG_LEN;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("noise error: {0}")]
    Noise(#[from] snow::Error),
    #[error("peer {0} is not paired with this device")]
    NotAllowed(DeviceId),
    #[error("message of {0} bytes exceeds the {MAX_MESSAGE}-byte limit")]
    MessageTooLarge(usize),
    #[error("handshake did not finish within {HANDSHAKE_TIMEOUT:?}")]
    HandshakeTimeout,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}

/// An established session. Not cancel-safe: if a `send` or `recv` future is
/// dropped part-way, or any call fails, the session is closed for good.
pub struct Session<S> {
    stream: S,
    noise: snow::TransportState,
    remote: [u8; KEY_LEN],
    code: SecurityCode,
    buf: Vec<u8>,
    broken: bool,
}

/// Connect as the initiator. `is_allowed` is called with the responder's
/// static public key; returning `false` aborts before our own key is sent.
pub async fn connect<S>(
    stream: S,
    identity: &Identity,
    is_allowed: impl Fn(&[u8; KEY_LEN]) -> bool + Sync,
) -> Result<Session<S>, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    with_timeout(handshake(stream, identity, &is_allowed, true)).await
}

/// Accept as the responder. `is_allowed` is called with the initiator's
/// static public key; returning `false` drops the connection.
pub async fn accept<S>(
    stream: S,
    identity: &Identity,
    is_allowed: impl Fn(&[u8; KEY_LEN]) -> bool + Sync,
) -> Result<Session<S>, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    with_timeout(handshake(stream, identity, &is_allowed, false)).await
}

async fn with_timeout<T>(
    fut: impl Future<Output = Result<T, TransportError>>,
) -> Result<T, TransportError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .map_err(|_| TransportError::HandshakeTimeout)?
}

async fn handshake<S>(
    mut stream: S,
    identity: &Identity,
    is_allowed: &(dyn Fn(&[u8; KEY_LEN]) -> bool + Sync),
    initiator: bool,
) -> Result<Session<S>, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let builder = snow::Builder::new(NOISE_PARAMS.parse()?)
        .local_private_key(identity.private_key())?
        .prologue(PROLOGUE)?;
    let mut buf = vec![0u8; MAX_NOISE_FRAME];

    if initiator {
        let mut hs = builder.build_initiator()?;
        write_handshake(&mut stream, &mut hs, &mut buf).await?; // -> e
        read_handshake(&mut stream, &mut hs, &mut buf).await?; // <- e, ee, s, es
        let remote = allowed_remote(&hs, is_allowed)?;
        write_handshake(&mut stream, &mut hs, &mut buf).await?; // -> s, se
        let mut session = Session::new(stream, hs, remote, buf)?;
        if !session.read_encrypted().await?.is_empty() {
            return Err(TransportError::Protocol("bad handshake confirmation"));
        }
        Ok(session)
    } else {
        let mut hs = builder.build_responder()?;
        read_handshake(&mut stream, &mut hs, &mut buf).await?; // -> e
        write_handshake(&mut stream, &mut hs, &mut buf).await?; // <- e, ee, s, es
        read_handshake(&mut stream, &mut hs, &mut buf).await?; // -> s, se
        let remote = allowed_remote(&hs, is_allowed)?;
        let mut session = Session::new(stream, hs, remote, buf)?;
        session.write_encrypted(&[]).await?;
        Ok(session)
    }
}

/// Handshake messages carry no payload.
async fn write_handshake<S: AsyncWrite + Unpin>(
    stream: &mut S,
    hs: &mut snow::HandshakeState,
    buf: &mut [u8],
) -> Result<(), TransportError> {
    let n = hs.write_message(&[], buf)?;
    Ok(frame::write_frame(stream, &buf[..n]).await?)
}

async fn read_handshake<S: AsyncRead + Unpin>(
    stream: &mut S,
    hs: &mut snow::HandshakeState,
    buf: &mut [u8],
) -> Result<(), TransportError> {
    let msg = frame::read_frame(stream, MAX_NOISE_FRAME).await?;
    if hs.read_message(&msg, buf)? != 0 {
        return Err(TransportError::Protocol("unexpected handshake payload"));
    }
    Ok(())
}

fn allowed_remote(
    hs: &snow::HandshakeState,
    is_allowed: &(dyn Fn(&[u8; KEY_LEN]) -> bool + Sync),
) -> Result<[u8; KEY_LEN], TransportError> {
    let remote: [u8; KEY_LEN] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or(TransportError::Protocol("peer sent no static key"))?;
    if is_allowed(&remote) {
        Ok(remote)
    } else {
        Err(TransportError::NotAllowed(DeviceId::from_public_key(
            &remote,
        )))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    fn new(
        stream: S,
        hs: snow::HandshakeState,
        remote: [u8; KEY_LEN],
        buf: Vec<u8>,
    ) -> Result<Self, TransportError> {
        let code = SecurityCode::from_handshake_hash(hs.get_handshake_hash());
        Ok(Self {
            stream,
            noise: hs.into_transport_mode()?,
            remote,
            code,
            buf,
            broken: false,
        })
    }

    /// Four words derived from this session's handshake. They match on both
    /// devices only if nobody is in the middle (see [`crate::sas`]).
    pub fn security_code(&self) -> SecurityCode {
        self.code
    }

    pub async fn send(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        check_len(msg)?;
        guard(self.broken)?;
        self.broken = true;
        let noise = &mut self.noise;
        send_message(&mut self.stream, &mut self.buf, msg, &mut |p, out| {
            Ok(noise.write_message(p, out)?)
        })
        .await?;
        self.broken = false;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>, TransportError> {
        guard(self.broken)?;
        self.broken = true;
        let noise = &mut self.noise;
        let msg = recv_message(&mut self.stream, &mut self.buf, &mut |c, out| {
            Ok(noise.read_message(c, out)?)
        })
        .await?;
        self.broken = false;
        Ok(msg)
    }

    /// Splits the session so one task can send while another receives.
    /// The halves share the Noise state behind a lock that is held only to
    /// encrypt or decrypt one frame, never across an `.await`. Each half
    /// keeps the session's rules (limits, not cancel-safe), and an error on
    /// either half closes both.
    pub fn into_split(self) -> (SendHalf<S>, RecvHalf<S>) {
        let (read, write) = tokio::io::split(self.stream);
        let shared = Arc::new(Shared {
            noise: Mutex::new(self.noise),
            broken: AtomicBool::new(self.broken),
        });
        let recv = RecvHalf {
            stream: read,
            shared: shared.clone(),
            remote: self.remote,
            buf: self.buf,
            busy: false,
        };
        let send = SendHalf {
            stream: write,
            shared,
            remote: self.remote,
            buf: vec![0u8; MAX_NOISE_FRAME],
            busy: false,
        };
        (send, recv)
    }

    /// The peer's static public key, already checked against the allowlist.
    pub fn remote_static(&self) -> &[u8; KEY_LEN] {
        &self.remote
    }

    pub fn remote_device(&self) -> DeviceId {
        DeviceId::from_public_key(&self.remote)
    }

    async fn write_encrypted(&mut self, plaintext: &[u8]) -> Result<(), TransportError> {
        let noise = &mut self.noise;
        write_encrypted(&mut self.stream, &mut self.buf, plaintext, &mut |p, out| {
            Ok(noise.write_message(p, out)?)
        })
        .await
    }

    async fn read_encrypted(&mut self) -> Result<&[u8], TransportError> {
        let noise = &mut self.noise;
        read_encrypted(&mut self.stream, &mut self.buf, &mut |c, out| {
            Ok(noise.read_message(c, out)?)
        })
        .await
    }
}

/// Noise state shared by the two halves of a split [`Session`].
struct Shared {
    noise: Mutex<snow::TransportState>,
    /// Set when either half fails, so the other stops too.
    broken: AtomicBool,
}

impl Shared {
    fn guard(&self, busy: bool) -> Result<(), TransportError> {
        guard(busy || self.broken.load(Ordering::SeqCst))
    }

    /// One encrypt or decrypt step under the lock.
    fn crypt(
        &self,
        f: impl FnOnce(&mut snow::TransportState) -> Result<usize, snow::Error>,
    ) -> Result<usize, TransportError> {
        let mut noise = self
            .noise
            .lock()
            .map_err(|_| TransportError::Protocol("session state poisoned"))?;
        Ok(f(&mut noise)?)
    }

    fn finish<T>(&self, res: Result<T, TransportError>) -> Result<T, TransportError> {
        if res.is_err() {
            self.broken.store(true, Ordering::SeqCst);
        }
        res
    }
}

/// The sending half of a split [`Session`].
pub struct SendHalf<S> {
    stream: WriteHalf<S>,
    shared: Arc<Shared>,
    remote: [u8; KEY_LEN],
    buf: Vec<u8>,
    /// Set while a send is in progress, so a cancelled one breaks the half.
    busy: bool,
}

impl<S: AsyncWrite> SendHalf<S> {
    pub async fn send(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        check_len(msg)?;
        self.shared.guard(self.busy)?;
        self.busy = true;
        let shared = &self.shared;
        let res = send_message(&mut self.stream, &mut self.buf, msg, &mut |p, out| {
            shared.crypt(|n| n.write_message(p, out))
        })
        .await;
        self.shared.finish(res)?;
        self.busy = false;
        Ok(())
    }

    pub fn remote_device(&self) -> DeviceId {
        DeviceId::from_public_key(&self.remote)
    }
}

/// The receiving half of a split [`Session`].
pub struct RecvHalf<S> {
    stream: ReadHalf<S>,
    shared: Arc<Shared>,
    remote: [u8; KEY_LEN],
    buf: Vec<u8>,
    /// Set while a receive is in progress, so a cancelled one breaks the half.
    busy: bool,
}

impl<S: AsyncRead> RecvHalf<S> {
    pub async fn recv(&mut self) -> Result<Vec<u8>, TransportError> {
        self.shared.guard(self.busy)?;
        self.busy = true;
        let shared = &self.shared;
        let res = recv_message(&mut self.stream, &mut self.buf, &mut |c, out| {
            shared.crypt(|n| n.read_message(c, out))
        })
        .await;
        let msg = self.shared.finish(res)?;
        self.busy = false;
        Ok(msg)
    }

    pub fn remote_device(&self) -> DeviceId {
        DeviceId::from_public_key(&self.remote)
    }
}

fn guard(broken: bool) -> Result<(), TransportError> {
    if broken {
        Err(TransportError::Protocol(
            "session closed after an earlier error",
        ))
    } else {
        Ok(())
    }
}

fn check_len(msg: &[u8]) -> Result<(), TransportError> {
    if msg.len() > MAX_MESSAGE {
        Err(TransportError::MessageTooLarge(msg.len()))
    } else {
        Ok(())
    }
}

/// One encrypt (or decrypt) step: input in, bytes written to the buffer out.
type Crypt<'a> = dyn FnMut(&[u8], &mut [u8]) -> Result<usize, TransportError> + Send + 'a;

/// Sends `msg` as a first frame that starts with the total length, then
/// continuation frames.
async fn send_message<W: AsyncWrite + Unpin>(
    stream: &mut W,
    buf: &mut [u8],
    msg: &[u8],
    encrypt: &mut Crypt<'_>,
) -> Result<(), TransportError> {
    let total = u32::try_from(msg.len()).map_err(|_| TransportError::MessageTooLarge(msg.len()))?;
    let (head, mut rest) = msg.split_at(msg.len().min(MAX_PLAINTEXT - LEN_PREFIX));
    let mut first = Vec::with_capacity(LEN_PREFIX + head.len());
    first.extend_from_slice(&total.to_be_bytes());
    first.extend_from_slice(head);
    write_encrypted(stream, buf, &first, encrypt).await?;
    while !rest.is_empty() {
        let (part, tail) = rest.split_at(rest.len().min(MAX_PLAINTEXT));
        write_encrypted(stream, buf, part, encrypt).await?;
        rest = tail;
    }
    Ok(())
}

/// Reads one message written by [`send_message`], rejecting any claimed
/// length over [`MAX_MESSAGE`] before allocating.
async fn recv_message<R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut [u8],
    decrypt: &mut Crypt<'_>,
) -> Result<Vec<u8>, TransportError> {
    let first = read_encrypted(stream, buf, decrypt).await?;
    let (len, head) = first
        .split_first_chunk::<LEN_PREFIX>()
        .ok_or(TransportError::Protocol("message header too short"))?;
    let total = u32::from_be_bytes(*len) as usize;
    if total > MAX_MESSAGE {
        return Err(TransportError::MessageTooLarge(total));
    }
    if head.len() > total {
        return Err(TransportError::Protocol("message longer than its header"));
    }
    let mut msg = Vec::with_capacity(total);
    msg.extend_from_slice(head);
    while msg.len() < total {
        let part = read_encrypted(stream, buf, decrypt).await?;
        if part.is_empty() || part.len() > total - msg.len() {
            return Err(TransportError::Protocol("bad continuation frame"));
        }
        msg.extend_from_slice(part);
    }
    Ok(msg)
}

async fn write_encrypted<W: AsyncWrite + Unpin>(
    stream: &mut W,
    buf: &mut [u8],
    plaintext: &[u8],
    encrypt: &mut Crypt<'_>,
) -> Result<(), TransportError> {
    let n = encrypt(plaintext, buf)?;
    Ok(frame::write_frame(stream, &buf[..n]).await?)
}

async fn read_encrypted<'b, R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &'b mut [u8],
    decrypt: &mut Crypt<'_>,
) -> Result<&'b [u8], TransportError> {
    let msg = frame::read_frame(stream, MAX_NOISE_FRAME).await?;
    let n = decrypt(&msg, buf)?;
    Ok(&buf[..n])
}

impl<S> fmt::Debug for Session<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Session({})",
            DeviceId::from_public_key(&self.remote).short()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use tokio::io::{DuplexStream, ReadBuf};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    const MARKER: &[u8] = b"NEBULA-PLAINTEXT-MARKER-123";

    fn ids() -> (Identity, Identity) {
        (Identity::generate().unwrap(), Identity::generate().unwrap())
    }

    fn only(key: [u8; KEY_LEN]) -> impl Fn(&[u8; KEY_LEN]) -> bool + Sync {
        move |k| *k == key
    }

    /// Deterministic pseudo-random bytes (xorshift), so splitting is tested
    /// with data that has no repeating structure.
    fn noise_bytes(len: usize) -> Vec<u8> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    async fn pair_over<A, B>(a: A, b: B, ia: &Identity, ib: &Identity) -> (Session<A>, Session<B>)
    where
        A: AsyncRead + AsyncWrite + Unpin,
        B: AsyncRead + AsyncWrite + Unpin,
    {
        let (ka, kb) = (*ia.public_key(), *ib.public_key());
        let (sa, sb) = tokio::join!(connect(a, ia, only(kb)), accept(b, ib, only(ka)));
        (sa.unwrap(), sb.unwrap())
    }

    /// Records every byte written and can flip one bit at a chosen offset.
    #[derive(Default)]
    struct Tap {
        written: AtomicUsize,
        flip_at: AtomicUsize,
        captured: Mutex<Vec<u8>>,
    }

    impl Tap {
        fn new() -> Arc<Self> {
            let tap = Self::default();
            tap.flip_at.store(usize::MAX, Ordering::SeqCst);
            Arc::new(tap)
        }

        /// Flip a bit in the `n`th byte written from now on.
        fn flip_after(&self, n: usize) {
            let at = self.written.load(Ordering::SeqCst) + n;
            self.flip_at.store(at, Ordering::SeqCst);
        }
    }

    struct Wire {
        inner: DuplexStream,
        tap: Arc<Tap>,
    }

    impl AsyncRead for Wire {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Wire {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let start = self.tap.written.load(Ordering::SeqCst);
            let mut data = buf.to_vec();
            if let Some(i) = self.tap.flip_at.load(Ordering::SeqCst).checked_sub(start)
                && i < data.len()
            {
                data[i] ^= 0x01;
            }
            let poll = Pin::new(&mut self.inner).poll_write(cx, &data);
            if let Poll::Ready(Ok(n)) = poll {
                self.tap.written.fetch_add(n, Ordering::SeqCst);
                self.tap
                    .captured
                    .lock()
                    .unwrap()
                    .extend_from_slice(&data[..n]);
            }
            poll
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    fn tapped_pipe() -> (Wire, Wire, Arc<Tap>, Arc<Tap>) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (ta, tb) = (Tap::new(), Tap::new());
        let wa = Wire {
            inner: a,
            tap: ta.clone(),
        };
        let wb = Wire {
            inner: b,
            tap: tb.clone(),
        };
        (wa, wb, ta, tb)
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[tokio::test]
    async fn paired_handshake_and_messages() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 20);
        let (mut sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        assert_eq!(sa.remote_device(), ib.device_id());
        assert_eq!(sb.remote_device(), ia.device_id());
        assert_eq!(sa.remote_static(), ib.public_key());
        assert_eq!(sa.security_code(), sb.security_code(), "codes must match");

        sa.send(b"hello").await.unwrap();
        assert_eq!(sb.recv().await.unwrap(), b"hello");
        sb.send(b"back").await.unwrap();
        assert_eq!(sa.recv().await.unwrap(), b"back");
        sa.send(b"").await.unwrap();
        assert_eq!(sb.recv().await.unwrap(), b"");
    }

    #[tokio::test]
    async fn large_messages_are_split_and_reassembled() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (mut sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        for len in [
            MAX_PLAINTEXT - LEN_PREFIX,
            MAX_PLAINTEXT,
            1 << 20,
            MAX_MESSAGE,
        ] {
            let msg = noise_bytes(len);
            let (sent, got) = tokio::join!(sa.send(&msg), sb.recv());
            sent.unwrap();
            assert_eq!(got.unwrap(), msg, "length {len}");
        }
    }

    #[tokio::test]
    async fn oversized_send_is_rejected_and_session_survives() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (mut sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        let big = vec![0u8; MAX_MESSAGE + 1];
        assert!(matches!(
            sa.send(&big).await,
            Err(TransportError::MessageTooLarge(n)) if n == MAX_MESSAGE + 1
        ));
        sa.send(b"still fine").await.unwrap();
        assert_eq!(sb.recv().await.unwrap(), b"still fine");
    }

    #[tokio::test]
    async fn oversized_length_header_is_rejected_before_allocating() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (mut sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        let claim = u32::MAX.to_be_bytes();
        sa.write_encrypted(&claim).await.unwrap();
        assert!(matches!(
            sb.recv().await,
            Err(TransportError::MessageTooLarge(n)) if n == u32::MAX as usize
        ));
    }

    #[tokio::test]
    async fn responder_rejects_unpaired_initiator() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let kb = *ib.public_key();
        let (sa, sb) = tokio::join!(connect(a, &ia, only(kb)), accept(b, &ib, |_| false));
        assert!(matches!(sb, Err(TransportError::NotAllowed(id)) if id == ia.device_id()));
        assert!(sa.is_err(), "initiator must not get a session: {sa:?}");
    }

    #[tokio::test]
    async fn initiator_rejects_unpaired_responder_without_revealing_itself() {
        let (ia, ib) = ids();
        let (a, b, ta, _) = tapped_pipe();
        let ka = *ia.public_key();
        let (sa, sb) = tokio::join!(connect(a, &ia, |_| false), accept(b, &ib, only(ka)));
        assert!(matches!(sa, Err(TransportError::NotAllowed(id)) if id == ib.device_id()));
        assert!(sb.is_err(), "responder must not get a session: {sb:?}");
        // The initiator stopped after `-> e`: only its ephemeral key was sent.
        assert_eq!(ta.written.load(Ordering::SeqCst), 4 + KEY_LEN);
    }

    #[tokio::test]
    async fn flipped_bit_after_handshake_is_rejected() {
        let (ia, ib) = ids();
        let (a, b, ta, _) = tapped_pipe();
        let (mut sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        ta.flip_after(10);
        sa.send(b"some file metadata").await.unwrap();
        assert!(matches!(sb.recv().await, Err(TransportError::Noise(_))));
        // No resync: the session stays closed.
        assert!(matches!(sb.recv().await, Err(TransportError::Protocol(_))));
    }

    #[tokio::test]
    async fn flipped_bit_during_handshake_fails_it() {
        let (ia, ib) = ids();
        let (a, b, _, tb) = tapped_pipe();
        // Byte 50 of the responder's output is inside its encrypted static key.
        tb.flip_after(50);
        let (ka, kb) = (*ia.public_key(), *ib.public_key());
        let (sa, sb) = tokio::join!(connect(a, &ia, only(kb)), accept(b, &ib, only(ka)));
        assert!(sa.is_err());
        assert!(sb.is_err());
    }

    /// Passive eavesdropper: the captured wire bytes (both directions) never
    /// contain the plaintext marker, and with XX neither device's static
    /// public key appears in cleartext either.
    #[tokio::test]
    async fn wire_never_carries_plaintext_or_static_keys() {
        let (ia, ib) = ids();
        let (a, b, ta, tb) = tapped_pipe();
        let (mut sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        let mut msg = noise_bytes(100_000);
        msg.extend_from_slice(MARKER);
        sa.send(&msg).await.unwrap();
        assert_eq!(sb.recv().await.unwrap(), msg);
        sb.send(MARKER).await.unwrap();
        assert_eq!(sa.recv().await.unwrap(), MARKER);

        assert!(ta.written.load(Ordering::SeqCst) > msg.len());
        for tap in [ta, tb] {
            let wire = tap.captured.lock().unwrap();
            assert!(!contains(&wire, MARKER));
            assert!(!contains(&wire, ia.public_key()));
            assert!(!contains(&wire, ib.public_key()));
        }
    }

    #[tokio::test]
    async fn silent_peer_times_out() {
        tokio::time::pause();
        let (ia, ib) = ids();
        let (a, _keep_open) = tokio::io::duplex(1024);
        let res = connect(a, &ia, |_| true).await;
        assert!(matches!(res, Err(TransportError::HandshakeTimeout)));
        let (b, _keep_open) = tokio::io::duplex(1024);
        let res = accept(b, &ib, |_| true).await;
        assert!(matches!(res, Err(TransportError::HandshakeTimeout)));
    }

    #[tokio::test]
    async fn each_session_gets_fresh_security_code() {
        let (ia, ib) = ids();
        let (a1, b1) = tokio::io::duplex(1 << 16);
        let (a2, b2) = tokio::io::duplex(1 << 16);
        let (s1, _) = pair_over(a1, b1, &ia, &ib).await;
        let (s2, _) = pair_over(a2, b2, &ia, &ib).await;
        // Fresh ephemeral keys every time, so the words change per session.
        assert_ne!(s1.security_code(), s2.security_code());
    }

    /// Both directions at once, from separate tasks, with messages that
    /// span many frames, so frames from the two directions interleave.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn split_halves_work_concurrently_both_ways() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (sa, sb) = pair_over(a, b, &ia, &ib).await;
        assert_eq!(sa.security_code(), sb.security_code());
        let (mut a_tx, mut a_rx) = sa.into_split();
        let (mut b_tx, mut b_rx) = sb.into_split();
        assert_eq!(a_tx.remote_device(), ib.device_id());
        assert_eq!(b_rx.remote_device(), ia.device_id());

        let sizes = [0, 10, MAX_PLAINTEXT, 1 << 20, 3 << 20];
        let msgs: Vec<Vec<u8>> = sizes.iter().map(|&n| noise_bytes(n)).collect();
        let (m1, m2, m3, m4) = (msgs.clone(), msgs.clone(), msgs.clone(), msgs);
        let tasks = [
            tokio::spawn(async move {
                for m in &m1 {
                    a_tx.send(m).await.unwrap();
                }
            }),
            tokio::spawn(async move {
                for m in m2.iter().rev() {
                    b_tx.send(m).await.unwrap();
                }
            }),
            tokio::spawn(async move {
                for m in &m3 {
                    assert_eq!(&b_rx.recv().await.unwrap(), m);
                }
            }),
            tokio::spawn(async move {
                for m in m4.iter().rev() {
                    assert_eq!(&a_rx.recv().await.unwrap(), m);
                }
            }),
        ];
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test]
    async fn split_detects_tampering_and_closes_both_halves() {
        let (ia, ib) = ids();
        let (a, b, ta, _) = tapped_pipe();
        let (sa, sb) = pair_over(a, b, &ia, &ib).await;
        let (mut a_tx, _a_rx) = sa.into_split();
        let (mut b_tx, mut b_rx) = sb.into_split();
        ta.flip_after(10);
        a_tx.send(b"some file metadata").await.unwrap();
        assert!(matches!(b_rx.recv().await, Err(TransportError::Noise(_))));
        assert!(matches!(
            b_rx.recv().await,
            Err(TransportError::Protocol(_))
        ));
        assert!(matches!(
            b_tx.send(b"x").await,
            Err(TransportError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn split_oversized_send_is_rejected_and_session_survives() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (sa, mut sb) = pair_over(a, b, &ia, &ib).await;
        let (mut a_tx, _a_rx) = sa.into_split();
        let big = vec![0u8; MAX_MESSAGE + 1];
        assert!(matches!(
            a_tx.send(&big).await,
            Err(TransportError::MessageTooLarge(_))
        ));
        a_tx.send(b"still fine").await.unwrap();
        assert_eq!(sb.recv().await.unwrap(), b"still fine");
    }

    #[tokio::test]
    async fn dropping_one_half_leaves_the_other_working() {
        let (ia, ib) = ids();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (sa, sb) = pair_over(a, b, &ia, &ib).await;
        let (a_tx, mut a_rx) = sa.into_split();
        let (mut b_tx, b_rx) = sb.into_split();
        drop(a_tx);
        drop(b_rx);
        b_tx.send(b"after drop").await.unwrap();
        assert_eq!(a_rx.recv().await.unwrap(), b"after drop");
        // Once the peer is gone entirely, recv reports it instead of panicking.
        drop(b_tx);
        assert!(matches!(a_rx.recv().await, Err(TransportError::Io(_))));
    }

    #[tokio::test]
    async fn tcp_smoke() {
        let (ia, ib) = ids();
        let (ka, kb) = (*ia.public_key(), *ib.public_key());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sa, sb) = tokio::join!(
            async { connect(TcpStream::connect(addr).await.unwrap(), &ia, only(kb)).await },
            async {
                let (stream, _) = listener.accept().await.unwrap();
                accept(stream, &ib, only(ka)).await
            }
        );
        let (mut sa, mut sb) = (sa.unwrap(), sb.unwrap());
        let msg = noise_bytes(200_000);
        let (sent, got) = tokio::join!(sa.send(&msg), sb.recv());
        sent.unwrap();
        assert_eq!(got.unwrap(), msg);
        sb.send(b"ok").await.unwrap();
        assert_eq!(sa.recv().await.unwrap(), b"ok");
    }
}
