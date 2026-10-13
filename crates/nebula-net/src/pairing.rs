//! One-time pairing with a short code (brief §4, ADR 0005).
//!
//! 1. Both sides run SPAKE2 (the `spake2` crate, Ed25519 group,
//!    <https://docs.rs/spake2>) with the 6-digit code as the password. A
//!    passive or active attacker learns nothing that allows an offline guess.
//! 2. The 32-byte SPAKE2 key becomes the PSK of a Noise `XXpsk2` handshake
//!    run with each device's long-term static key. A wrong code gives a
//!    different PSK, so the handshake's AEAD fails: that is key confirmation.
//!    Static keys and device names travel inside this handshake, so each side
//!    learns exactly the peer key it will pin.
//! 3. The listener sends one encrypted, empty transport message so the joiner
//!    knows the listener accepted the pairing too.
//!
//! Storing the resulting [`PairedPeer`] in the allowlist is the caller's job.

use std::fmt;
use std::io;
use std::time::Duration;

use nebula_core::DeviceId;
use spake2::{Ed25519Group, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::time::{Instant, timeout, timeout_at};

use crate::frame::{read_frame, write_frame};
use crate::identity::Identity;

/// How long a code stays valid after `listen` starts.
pub const CODE_LIFETIME: Duration = Duration::from_secs(120);
/// Failed attempts after which the code dies.
pub const MAX_ATTEMPTS: u32 = 3;
/// Limit on one connection, so a silent peer cannot hold the code until expiry.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest device name, in UTF-8 bytes.
pub const MAX_NAME_LEN: usize = 64;

const CODE_SPACE: u32 = 1_000_000;
// XXpsk2, not XXpsk3: see ADR 0005 and `xxpsk3_leaves_responder_static_unauthenticated`.
const NOISE_PARAMS: &str = "Noise_XXpsk2_25519_ChaChaPoly_BLAKE2s";
const PSK_LOCATION: u8 = 2;
const PROLOGUE: &[u8] = b"nebula-pair/1";
const SPAKE_ID_LISTENER: &[u8] = b"nebula-pair-v1/listener";
const SPAKE_ID_JOINER: &[u8] = b"nebula-pair-v1/joiner";
/// A SPAKE2 Ed25519 message: one side byte plus a 32-byte point.
const MAX_SPAKE_MSG: usize = 33;
/// The largest handshake message is msg 2: e (32) + encrypted s (48) +
/// encrypted name (64 + 16) = 160 bytes.
const MAX_HANDSHAKE_MSG: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("pairing I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("wrong pairing code")]
    WrongCode,
    #[error("pairing code expired")]
    Expired,
    #[error("too many failed pairing attempts; start pairing again for a new code")]
    TooManyAttempts,
    #[error("pairing timed out")]
    Timeout,
    #[error("pairing protocol error: {0}")]
    Protocol(String),
    #[error("pairing code must be 6 digits, like 123-456")]
    BadCode,
    #[error("device name must be 1 to {MAX_NAME_LEN} bytes of UTF-8 without control characters")]
    BadName,
    #[error("system random number generator failed: {0}")]
    Rng(getrandom::Error),
    #[error("noise error: {0}")]
    Noise(#[from] snow::Error),
}

/// A 6-digit one-time code, shown as `123-456`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PairingCode(u32);

impl PairingCode {
    /// A uniformly random code from the OS CSPRNG.
    pub fn generate() -> Result<Self, PairingError> {
        // Rejection sampling avoids modulo bias.
        let limit = u32::MAX - u32::MAX % CODE_SPACE;
        loop {
            let n = getrandom::u32().map_err(PairingError::Rng)?;
            if n < limit {
                return Ok(Self(n % CODE_SPACE));
            }
        }
    }

    /// Accepts exactly 6 ASCII digits; dashes and whitespace are ignored.
    pub fn parse(s: &str) -> Result<Self, PairingError> {
        let mut value = 0u32;
        let mut digits = 0;
        for c in s.chars() {
            if c == '-' || c.is_whitespace() {
                continue;
            }
            let d = c.to_digit(10).filter(|_| c.is_ascii_digit());
            match d {
                Some(d) if digits < 6 => {
                    value = value * 10 + d;
                    digits += 1;
                }
                _ => return Err(PairingError::BadCode),
            }
        }
        if digits == 6 {
            Ok(Self(value))
        } else {
            Err(PairingError::BadCode)
        }
    }

    fn password(&self) -> Password {
        Password::new(format!("{:06}", self.0))
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:03}-{:03}", self.0 / 1000, self.0 % 1000)
    }
}

/// The device on the other end of a successful pairing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PairedPeer {
    pub public_key: [u8; 32],
    pub device_id: DeviceId,
    pub name: String,
}

/// Device A: accept connections on `listener` until one pairs successfully,
/// the code expires, or [`MAX_ATTEMPTS`] attempts fail. Connections are
/// handled one at a time. The listener is dropped on return.
pub async fn listen(
    listener: TcpListener,
    identity: &Identity,
    my_name: &str,
    code: &PairingCode,
) -> Result<PairedPeer, PairingError> {
    listen_for(listener, identity, my_name, code, CODE_LIFETIME).await
}

async fn listen_for(
    listener: TcpListener,
    identity: &Identity,
    my_name: &str,
    code: &PairingCode,
    lifetime: Duration,
) -> Result<PairedPeer, PairingError> {
    check_name(my_name)?;
    let deadline = Instant::now() + lifetime;
    let mut failures = 0;
    loop {
        if Instant::now() >= deadline {
            return Err(PairingError::Expired);
        }
        let (stream, _) = timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| PairingError::Expired)??;
        let attempt_deadline = deadline.min(Instant::now() + ATTEMPT_TIMEOUT);
        let mut counted = false;
        let result = timeout_at(
            attempt_deadline,
            run_listener(stream, identity, my_name.as_bytes(), code, &mut counted),
        )
        .await;
        match result {
            Ok(Ok(peer)) => return Ok(peer),
            // Only connections that reached SPAKE2 cost an attempt, so a
            // port scan does not burn the code.
            _ if counted => {
                failures += 1;
                if failures >= MAX_ATTEMPTS {
                    return Err(PairingError::TooManyAttempts);
                }
            }
            _ => {}
        }
    }
}

/// Device A's side of a single pairing attempt over an already-accepted
/// stream. [`listen`] wraps this with attempt counting and timeouts.
pub async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    identity: &Identity,
    my_name: &str,
    code: &PairingCode,
) -> Result<PairedPeer, PairingError> {
    check_name(my_name)?;
    let mut counted = false;
    run_listener(stream, identity, my_name.as_bytes(), code, &mut counted).await
}

/// Device B: pair over `stream`, already connected to a listening device.
pub async fn join<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    identity: &Identity,
    my_name: &str,
    code: &PairingCode,
) -> Result<PairedPeer, PairingError> {
    check_name(my_name)?;
    timeout(
        ATTEMPT_TIMEOUT,
        run_joiner(stream, identity, my_name.as_bytes(), code),
    )
    .await
    .map_err(|_| PairingError::Timeout)?
}

/// Sets `*counted` once the joiner's SPAKE2 message has arrived.
async fn run_listener<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    identity: &Identity,
    my_name: &[u8],
    code: &PairingCode,
    counted: &mut bool,
) -> Result<PairedPeer, PairingError> {
    let (spake, my_msg) = Spake2::<Ed25519Group>::start_a(
        &code.password(),
        &spake2::Identity::new(SPAKE_ID_LISTENER),
        &spake2::Identity::new(SPAKE_ID_JOINER),
    );
    let their_msg = read_frame(&mut stream, MAX_SPAKE_MSG).await?;
    *counted = true;
    write_frame(&mut stream, &my_msg).await?;
    let psk = finish_spake(spake, &their_msg)?;

    let mut hs = noise_builder(identity, &psk)?.build_responder()?;
    let mut buf = [0u8; MAX_HANDSHAKE_MSG];
    let mut payload = [0u8; MAX_HANDSHAKE_MSG];

    // -> e
    let msg = read_frame(&mut stream, MAX_HANDSHAKE_MSG).await?;
    hs.read_message(&msg, &mut payload).map_err(read_error)?;
    // <- e, ee, s, es, psk + my name
    let n = hs.write_message(my_name, &mut buf)?;
    write_frame(&mut stream, &buf[..n]).await?;
    // -> s, se + their name
    let msg = read_frame(&mut stream, MAX_HANDSHAKE_MSG).await?;
    let n = hs.read_message(&msg, &mut payload).map_err(read_error)?;
    let peer = paired_peer(hs.get_remote_static(), &payload[..n])?;

    // Tell the joiner we accepted.
    let mut transport = hs.into_transport_mode()?;
    let n = transport.write_message(&[], &mut buf)?;
    write_frame(&mut stream, &buf[..n]).await?;
    Ok(peer)
}

async fn run_joiner<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    identity: &Identity,
    my_name: &[u8],
    code: &PairingCode,
) -> Result<PairedPeer, PairingError> {
    let (spake, my_msg) = Spake2::<Ed25519Group>::start_b(
        &code.password(),
        &spake2::Identity::new(SPAKE_ID_LISTENER),
        &spake2::Identity::new(SPAKE_ID_JOINER),
    );
    write_frame(&mut stream, &my_msg).await?;
    let their_msg = read_frame(&mut stream, MAX_SPAKE_MSG).await?;
    let psk = finish_spake(spake, &their_msg)?;

    let mut hs = noise_builder(identity, &psk)?.build_initiator()?;
    let mut buf = [0u8; MAX_HANDSHAKE_MSG];
    let mut payload = [0u8; MAX_HANDSHAKE_MSG];

    let n = hs.write_message(&[], &mut buf)?;
    write_frame(&mut stream, &buf[..n]).await?;
    let msg = read_frame(&mut stream, MAX_HANDSHAKE_MSG).await?;
    // The PSK is mixed before this payload is decrypted, so success here
    // authenticates the listener's static key against the code.
    let n = hs.read_message(&msg, &mut payload).map_err(read_error)?;
    let peer = paired_peer(hs.get_remote_static(), &payload[..n])?;
    let n = hs.write_message(my_name, &mut buf)?;
    write_frame(&mut stream, &buf[..n]).await?;

    let mut transport = hs.into_transport_mode()?;
    let msg = read_frame(&mut stream, MAX_HANDSHAKE_MSG).await?;
    let n = transport
        .read_message(&msg, &mut payload)
        .map_err(read_error)?;
    if n != 0 {
        return Err(PairingError::Protocol(
            "unexpected confirmation payload".into(),
        ));
    }
    Ok(peer)
}

fn finish_spake(spake: Spake2<Ed25519Group>, their_msg: &[u8]) -> Result<[u8; 32], PairingError> {
    let key = spake
        .finish(their_msg)
        .map_err(|e| PairingError::Protocol(format!("bad SPAKE2 message: {e}")))?;
    key.as_slice()
        .try_into()
        .map_err(|_| PairingError::Protocol("SPAKE2 key is not 32 bytes".into()))
}

fn noise_builder<'a>(
    identity: &'a Identity,
    psk: &'a [u8; 32],
) -> Result<snow::Builder<'a>, PairingError> {
    let params = NOISE_PARAMS.parse()?;
    Ok(snow::Builder::new(params)
        .local_private_key(identity.private_key())?
        .psk(PSK_LOCATION, psk)?
        .prologue(PROLOGUE)?)
}

/// An AEAD failure during pairing means the two sides derived different
/// PSKs, which (absent tampering) means different codes.
fn read_error(e: snow::Error) -> PairingError {
    match e {
        snow::Error::Decrypt => PairingError::WrongCode,
        other => PairingError::Protocol(format!("bad handshake message: {other}")),
    }
}

fn paired_peer(remote_static: Option<&[u8]>, name: &[u8]) -> Result<PairedPeer, PairingError> {
    let public_key: [u8; 32] = remote_static
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| PairingError::Protocol("peer static key missing".into()))?;
    let name = std::str::from_utf8(name)
        .map_err(|_| PairingError::Protocol("peer name is not UTF-8".into()))?;
    check_name(name).map_err(|_| PairingError::Protocol("peer name is invalid".into()))?;
    Ok(PairedPeer {
        public_key,
        device_id: DeviceId::from_public_key(&public_key),
        name: name.to_owned(),
    })
}

/// Names are shown in terminals, so control characters (escape sequences)
/// are rejected along with empty and overlong names.
fn check_name(name: &str) -> Result<(), PairingError> {
    if name.is_empty() || name.len() > MAX_NAME_LEN || name.chars().any(char::is_control) {
        return Err(PairingError::BadName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use tokio::io::{DuplexStream, ReadBuf, duplex};
    use tokio::net::TcpStream;

    use super::*;

    fn code(s: &str) -> PairingCode {
        PairingCode::parse(s).unwrap()
    }

    async fn pair_over_duplex(
        a: &Identity,
        b: &Identity,
        code_a: &PairingCode,
        code_b: &PairingCode,
    ) -> (
        Result<PairedPeer, PairingError>,
        Result<PairedPeer, PairingError>,
    ) {
        let (sa, sb) = duplex(4096);
        tokio::join!(
            respond(sa, a, "alice-laptop", code_a),
            join(sb, b, "bob-desktop", code_b)
        )
    }

    #[test]
    fn code_display_and_parse_round_trip() {
        assert_eq!(code("123456").to_string(), "123-456");
        assert_eq!(code("123-456"), code("123456"));
        assert_eq!(code(" 012 345 "), code("012-345"));
        assert_eq!(code("000-007").to_string(), "000-007");
        for _ in 0..100 {
            let c = PairingCode::generate().unwrap();
            let shown = c.to_string();
            assert_eq!(shown.len(), 7);
            assert_eq!(code(&shown), c);
        }
    }

    #[test]
    fn code_parse_rejects_bad_input() {
        for bad in [
            "",
            "12345",
            "1234567",
            "12a456",
            "123_456",
            "+12345",
            "١٢٣٤٥٦",
            "123-45",
        ] {
            assert!(
                matches!(PairingCode::parse(bad), Err(PairingError::BadCode)),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn same_code_pairs_both_ways() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let c = PairingCode::generate().unwrap();
        let (ra, rb) = pair_over_duplex(&a, &b, &c, &c).await;
        let (pa, pb) = (ra.unwrap(), rb.unwrap());
        assert_eq!(pa.public_key, *b.public_key());
        assert_eq!(pa.device_id, b.device_id());
        assert_eq!(pa.name, "bob-desktop");
        assert_eq!(pb.public_key, *a.public_key());
        assert_eq!(pb.device_id, a.device_id());
        assert_eq!(pb.name, "alice-laptop");
    }

    #[tokio::test]
    async fn wrong_code_pairs_neither_side() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let (ra, rb) = pair_over_duplex(&a, &b, &code("111-111"), &code("111-112")).await;
        assert!(matches!(rb, Err(PairingError::WrongCode)), "{rb:?}");
        assert!(ra.is_err(), "{ra:?}");
    }

    #[tokio::test]
    async fn bad_peer_names_are_protocol_errors() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let c = code("123-456");
        let long = [b'x'; MAX_NAME_LEN + 1];
        for bad in [&long[..], b"\xff\xfe", b"\x1b[2J", b""] {
            // Joiner sends the bad name.
            let (sa, sb) = duplex(4096);
            let (ra, rb) = tokio::join!(respond(sa, &a, "alice", &c), run_joiner(sb, &b, bad, &c));
            assert!(matches!(ra, Err(PairingError::Protocol(_))), "{ra:?}");
            assert!(rb.is_err());
            // Listener sends the bad name.
            let (sa, sb) = duplex(4096);
            let mut counted = false;
            let (ra, rb) = tokio::join!(
                run_listener(sa, &a, bad, &c, &mut counted),
                join(sb, &b, "bob", &c)
            );
            assert!(matches!(rb, Err(PairingError::Protocol(_))), "{rb:?}");
            assert!(ra.is_err());
        }
    }

    #[tokio::test]
    async fn local_name_is_validated() {
        let a = Identity::generate().unwrap();
        let (sa, _sb) = duplex(64);
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let r = respond(sa, &a, &long, &code("123456")).await;
        assert!(matches!(r, Err(PairingError::BadName)));
    }

    /// Records every byte written through it.
    struct Recorder {
        inner: DuplexStream,
        log: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncRead for Recorder {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Recorder {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
            if let Poll::Ready(Ok(n)) = poll {
                self.log.lock().unwrap().extend_from_slice(&buf[..n]);
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

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[tokio::test]
    async fn wire_bytes_hide_code_and_names() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let c = code("482-913");
        let log = Arc::new(Mutex::new(Vec::new()));
        let (sa, sb) = duplex(4096);
        let sa = Recorder {
            inner: sa,
            log: log.clone(),
        };
        let sb = Recorder {
            inner: sb,
            log: log.clone(),
        };
        let (ra, rb) = tokio::join!(
            respond(sa, &a, "alice-laptop", &c),
            join(sb, &b, "bob-desktop", &c)
        );
        ra.unwrap();
        rb.unwrap();
        let wire = log.lock().unwrap().clone();
        assert!(wire.len() > 200);
        for secret in [&b"482913"[..], b"482-913", b"alice-laptop", b"bob-desktop"] {
            assert!(
                !contains(&wire, secret),
                "{:?}",
                String::from_utf8_lossy(secret)
            );
        }
    }

    async fn join_tcp(
        addr: std::net::SocketAddr,
        id: &Identity,
        c: &PairingCode,
    ) -> Result<PairedPeer, PairingError> {
        let stream = TcpStream::connect(addr).await?;
        join(stream, id, "bob", c).await
    }

    #[tokio::test]
    async fn listen_locks_out_after_three_wrong_codes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let a = Arc::new(Identity::generate().unwrap());
        let right = code("246-810");
        let task = tokio::spawn({
            let a = a.clone();
            let right = right.clone();
            async move { listen(listener, &a, "alice", &right).await }
        });
        let b = Identity::generate().unwrap();
        for _ in 0..MAX_ATTEMPTS {
            let r = join_tcp(addr, &b, &code("000-000")).await;
            assert!(matches!(r, Err(PairingError::WrongCode)), "{r:?}");
        }
        let r = task.await.unwrap();
        assert!(matches!(r, Err(PairingError::TooManyAttempts)), "{r:?}");
        // The listener is gone: even the right code fails now.
        assert!(join_tcp(addr, &b, &right).await.is_err());
    }

    #[tokio::test]
    async fn listen_accepts_right_code_after_two_wrong() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let a = Arc::new(Identity::generate().unwrap());
        let right = code("135-791");
        let task = tokio::spawn({
            let a = a.clone();
            let right = right.clone();
            async move { listen(listener, &a, "alice", &right).await }
        });
        let b = Identity::generate().unwrap();
        for _ in 0..2 {
            let r = join_tcp(addr, &b, &code("999-999")).await;
            assert!(matches!(r, Err(PairingError::WrongCode)), "{r:?}");
        }
        let pb = join_tcp(addr, &b, &right).await.unwrap();
        let pa = task.await.unwrap().unwrap();
        assert_eq!(pb.public_key, *a.public_key());
        assert_eq!(pa.public_key, *b.public_key());
        assert_eq!(pa.name, "bob");
    }

    #[tokio::test]
    async fn connections_that_never_reach_spake2_do_not_count() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let a = Arc::new(Identity::generate().unwrap());
        let right = code("314-159");
        let task = tokio::spawn({
            let a = a.clone();
            let right = right.clone();
            async move { listen(listener, &a, "alice", &right).await }
        });
        for _ in 0..MAX_ATTEMPTS + 2 {
            drop(TcpStream::connect(addr).await.unwrap());
        }
        let b = Identity::generate().unwrap();
        join_tcp(addr, &b, &right).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn listen_expires() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = Identity::generate().unwrap();
        let r = listen_for(
            listener,
            &a,
            "alice",
            &code("123-456"),
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(r, Err(PairingError::Expired)), "{r:?}");
    }

    /// Why ADR 0005 uses XXpsk2: under XXpsk3 the initiator decrypts the
    /// responder's static key and name (msg 2) before any PSK is mixed, so a
    /// responder that does not know the code is accepted at that point.
    #[test]
    fn xxpsk3_leaves_responder_static_unauthenticated() {
        let run = |pattern: &str| -> bool {
            let params: snow::params::NoiseParams = pattern.parse().unwrap();
            let location = if pattern.contains("psk2") { 2 } else { 3 };
            let (i, r) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let mut init = snow::Builder::new(params.clone())
                .local_private_key(i.private_key())
                .unwrap()
                .psk(location, &[1; 32])
                .unwrap()
                .build_initiator()
                .unwrap();
            let mut resp = snow::Builder::new(params)
                .local_private_key(r.private_key())
                .unwrap()
                .psk(location, &[2; 32])
                .unwrap()
                .build_responder()
                .unwrap();
            let (mut buf, mut out) = ([0u8; 256], [0u8; 256]);
            let n = init.write_message(&[], &mut buf).unwrap();
            resp.read_message(&buf[..n], &mut out).unwrap();
            let n = resp.write_message(b"name", &mut buf).unwrap();
            init.read_message(&buf[..n], &mut out).is_ok()
        };
        assert!(run("Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s"));
        assert!(!run(NOISE_PARAMS));
    }
}
