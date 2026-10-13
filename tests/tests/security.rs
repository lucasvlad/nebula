//! The security evaluation from brief §6, run over real TCP through the
//! fault-injection proxy so it sees exactly what a LAN attacker would.
#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;

use nebula_net::identity::Identity;
use nebula_net::pairing::{self, PairingCode, PairingError};
use nebula_net::transport::{self, TransportError};
use nebula_tests::proxy::{Direction, Fault, Proxy, contains};
use tokio::net::{TcpListener, TcpStream};

const MARKER: &[u8] = b"NEBULA-PLAINTEXT-MARKER-7f3a";

fn allow(key: [u8; 32]) -> impl Fn(&[u8; 32]) -> bool + Sync {
    move |k| *k == key
}

/// Starts a responder that accepts one session (allowing `peer`), echoes one
/// message back, and returns what it received.
async fn echo_server(
    me: Identity,
    peer: [u8; 32],
) -> (
    SocketAddr,
    tokio::task::JoinHandle<Result<Vec<u8>, TransportError>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut s = transport::accept(stream, &me, allow(peer)).await?;
        let msg = s.recv().await?;
        s.send(&msg).await?;
        Ok(msg)
    });
    (addr, task)
}

#[tokio::test]
async fn eavesdropper_sees_no_plaintext_or_keys() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_pub, b_pub) = (*a.public_key(), *b.public_key());
    let (server, task) = echo_server(b, a_pub).await;
    let proxy = Proxy::start(server, Fault::None).await.unwrap();

    let stream = TcpStream::connect(proxy.addr()).await.unwrap();
    let mut s = transport::connect(stream, &a, allow(b_pub)).await.unwrap();
    let mut msg = b"file contents: ".to_vec();
    msg.extend_from_slice(MARKER);
    s.send(&msg).await.unwrap();
    assert_eq!(s.recv().await.unwrap(), msg);
    assert_eq!(task.await.unwrap().unwrap(), msg);

    for dir in [Direction::ClientToServer, Direction::ServerToClient] {
        let wire = proxy.captured(dir);
        assert!(!wire.is_empty());
        assert!(!contains(&wire, MARKER), "plaintext leaked {dir:?}");
        assert!(!contains(&wire, &a_pub), "A's static key in clear {dir:?}");
        assert!(!contains(&wire, &b_pub), "B's static key in clear {dir:?}");
    }
}

#[tokio::test]
async fn flipped_ciphertext_bit_is_rejected() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_pub, b_pub) = (*a.public_key(), *b.public_key());
    let (server, task) = echo_server(b, a_pub).await;
    // The XX handshake from the client is 4 + 32 then 4 + 64 bytes; offset
    // 200 lands in the first application message.
    let proxy = Proxy::start(
        server,
        Fault::Corrupt {
            dir: Direction::ClientToServer,
            offset: 200,
            mask: 0x01,
        },
    )
    .await
    .unwrap();
    let stream = TcpStream::connect(proxy.addr()).await.unwrap();
    let mut s = transport::connect(stream, &a, allow(b_pub)).await.unwrap();
    s.send(&[7u8; 1000]).await.unwrap();
    let server_result = task.await.unwrap();
    assert!(
        matches!(server_result, Err(TransportError::Noise(_))),
        "tampered message was accepted: {server_result:?}"
    );
}

#[tokio::test]
async fn flipped_handshake_bit_fails_the_handshake() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_pub, b_pub) = (*a.public_key(), *b.public_key());
    let (server, task) = echo_server(b, a_pub).await;
    let proxy = Proxy::start(
        server,
        Fault::Corrupt {
            dir: Direction::ServerToClient,
            offset: 40,
            mask: 0x80,
        },
    )
    .await
    .unwrap();
    let stream = TcpStream::connect(proxy.addr()).await.unwrap();
    assert!(transport::connect(stream, &a, allow(b_pub)).await.is_err());
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn unpaired_device_is_rejected_before_any_data() {
    let (a, b, stranger) = (
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
    );
    let (b_pub, stranger_pub) = (*b.public_key(), *stranger.public_key());
    // B only allows A, so the stranger is refused.
    let (server, task) = echo_server(b, *a.public_key()).await;
    let proxy = Proxy::start(server, Fault::None).await.unwrap();
    let stream = TcpStream::connect(proxy.addr()).await.unwrap();
    // The stranger has somehow learned B's key and allows it.
    let result = transport::connect(stream, &stranger, allow(b_pub)).await;
    assert!(result.is_err(), "stranger got a session");
    let server_result = task.await.unwrap();
    assert!(matches!(
        server_result,
        Err(TransportError::NotAllowed(id)) if id == nebula_core::DeviceId::from_public_key(&stranger_pub)
    ));
    // Nothing but handshake messages crossed: no encrypted application data
    // went from B to the stranger after the refusal.
    assert!(proxy.bytes_forwarded(Direction::ServerToClient) < 200);
}

/// Pairs `joiner` with a listener behind the proxy, using `code` on the
/// joiner side.
async fn join_through(
    proxy: &Proxy,
    joiner: &Identity,
    code: &PairingCode,
) -> Result<pairing::PairedPeer, PairingError> {
    let stream = TcpStream::connect(proxy.addr()).await?;
    pairing::join(stream, joiner, "joiner", code).await
}

#[tokio::test]
async fn pairing_wrong_codes_lock_out_and_wire_hides_code() {
    let listener_id = Identity::generate().unwrap();
    let joiner = Identity::generate().unwrap();
    let code = PairingCode::parse("482-913").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listen_code = code.clone();
    let task = tokio::spawn(async move {
        pairing::listen(listener, &listener_id, "listener", &listen_code).await
    });
    let proxy = Proxy::start(addr, Fault::None).await.unwrap();

    let wrong = PairingCode::parse("000-000").unwrap();
    for _ in 0..pairing::MAX_ATTEMPTS {
        let err = join_through(&proxy, &joiner, &wrong).await.unwrap_err();
        assert!(matches!(err, PairingError::WrongCode), "{err:?}");
    }
    assert!(matches!(
        task.await.unwrap(),
        Err(PairingError::TooManyAttempts)
    ));
    // The right code no longer works: the listener is gone.
    assert!(join_through(&proxy, &joiner, &code).await.is_err());

    for dir in [Direction::ClientToServer, Direction::ServerToClient] {
        let wire = proxy.captured(dir);
        assert!(!contains(&wire, b"482913"));
        assert!(!contains(&wire, b"000000"));
        assert!(!contains(&wire, b"listener"));
        assert!(!contains(&wire, b"joiner"));
    }
}

#[tokio::test]
async fn pairing_survives_a_dropped_attempt() {
    let listener_id = Identity::generate().unwrap();
    let listener_pub = *listener_id.public_key();
    let joiner = Identity::generate().unwrap();
    let code = PairingCode::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listen_code = code.clone();
    let task = tokio::spawn(async move {
        pairing::listen(listener, &listener_id, "listener", &listen_code).await
    });
    // First attempt dies mid-handshake; the second goes through cleanly.
    let proxy = Proxy::start(
        addr,
        Fault::DropAfter {
            dir: Direction::ServerToClient,
            bytes: 50,
        },
    )
    .await
    .unwrap();
    assert!(join_through(&proxy, &joiner, &code).await.is_err());
    proxy.set_fault(Fault::None);
    let paired = join_through(&proxy, &joiner, &code).await.unwrap();
    assert_eq!(paired.public_key, listener_pub);
    assert_eq!(
        task.await.unwrap().unwrap().public_key,
        *joiner.public_key()
    );
}
