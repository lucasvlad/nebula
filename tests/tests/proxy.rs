#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use nebula_tests::proxy::{Direction, Fault, Proxy, contains};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const C2S: Direction = Direction::ClientToServer;
const S2C: Direction = Direction::ServerToClient;

async fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

/// Sends everything received on each connection to the channel as one Vec,
/// once the connection ends or goes quiet for 300 ms.
async fn sink_server() -> (SocketAddr, mpsc::UnboundedReceiver<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match tokio::time::timeout(Duration::from_millis(300), s.read(&mut buf)).await {
                        Ok(Ok(n)) if n > 0 => got.extend_from_slice(&buf[..n]),
                        _ => break,
                    }
                }
                let _ = tx.send(got);
            });
        }
    });
    (addr, rx)
}

#[tokio::test]
async fn none_round_trips_one_mib() {
    let proxy = Proxy::start(echo_server().await, Fault::None)
        .await
        .unwrap();
    let data: Vec<u8> = (0..1usize << 20).map(|i| (i * 31 % 251) as u8).collect();
    let (mut r, mut w) = TcpStream::connect(proxy.addr()).await.unwrap().into_split();
    let d = data.clone();
    let writer = tokio::spawn(async move {
        w.write_all(&d).await.unwrap();
        w
    });
    let mut back = vec![0u8; data.len()];
    r.read_exact(&mut back).await.unwrap();
    writer.await.unwrap();
    assert_eq!(back, data);
    assert_eq!(proxy.bytes_forwarded(C2S), data.len() as u64);
    assert_eq!(proxy.bytes_forwarded(S2C), data.len() as u64);
    assert_eq!(proxy.captured(C2S), data);
    assert_eq!(proxy.captured(S2C), data);
    assert_eq!(proxy.connections(), 1);
}

#[tokio::test]
async fn drop_after_closes_connection() {
    let (server, mut rx) = sink_server().await;
    let fault = Fault::DropAfter {
        dir: C2S,
        bytes: 1000,
    };
    let proxy = Proxy::start(server, fault).await.unwrap();
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    let _ = c.write_all(&[7u8; 5000]).await;
    let mut buf = [0u8; 16];
    let end = tokio::time::timeout(Duration::from_secs(2), c.read(&mut buf))
        .await
        .unwrap();
    assert!(matches!(end, Ok(0) | Err(_)));
    assert_eq!(rx.recv().await.unwrap().len(), 1000);
}

#[tokio::test]
async fn delay_slows_round_trip() {
    let by = Duration::from_millis(50);
    let proxy = Proxy::start(echo_server().await, Fault::Delay { dir: C2S, by })
        .await
        .unwrap();
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    let start = Instant::now();
    c.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await.unwrap();
    assert!(start.elapsed() >= by);
    assert_eq!(&buf, b"ping");
}

#[tokio::test]
async fn truncate_stalls_but_stays_open() {
    let (server, mut rx) = sink_server().await;
    let fault = Fault::Truncate {
        dir: C2S,
        bytes: 100,
    };
    let proxy = Proxy::start(server, fault).await.unwrap();
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    c.write_all(&[1u8; 60]).await.unwrap();
    c.write_all(&[2u8; 200]).await.unwrap();
    // Connection stays open: the client sees no EOF in a short window.
    let mut buf = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_millis(100), c.read(&mut buf)).await;
    assert!(r.is_err());
    assert_eq!(rx.recv().await.unwrap().len(), 100);
    assert_eq!(proxy.bytes_forwarded(C2S), 100);
}

#[tokio::test]
async fn corrupt_flips_one_bit_across_write_boundary() {
    let (server, mut rx) = sink_server().await;
    let fault = Fault::Corrupt {
        dir: C2S,
        offset: 10,
        mask: 0x01,
    };
    let proxy = Proxy::start(server, fault).await.unwrap();
    let data: Vec<u8> = (0..20u8).collect();
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    c.write_all(&data[..7]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    c.write_all(&data[7..]).await.unwrap();
    drop(c);
    let got = rx.recv().await.unwrap();
    assert_eq!(got.len(), 20);
    let diffs: Vec<usize> = (0..20).filter(|&i| got[i] != data[i]).collect();
    assert_eq!(diffs, vec![10]);
    assert_eq!(got[10], data[10] ^ 1);
    assert_eq!(proxy.captured(C2S), got);
}

#[tokio::test]
async fn corrupt_within_single_write() {
    let (server, mut rx) = sink_server().await;
    let fault = Fault::Corrupt {
        dir: C2S,
        offset: 10,
        mask: 0x80,
    };
    let proxy = Proxy::start(server, fault).await.unwrap();
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    c.write_all(&[0u8; 20]).await.unwrap();
    drop(c);
    let got = rx.recv().await.unwrap();
    assert_eq!(got.iter().filter(|&&b| b != 0).count(), 1);
    assert_eq!(got[10], 0x80);
}

#[tokio::test]
async fn set_fault_heals_network() {
    let (server, mut rx) = sink_server().await;
    let fault = Fault::DropAfter {
        dir: C2S,
        bytes: 10,
    };
    let proxy = Proxy::start(server, fault).await.unwrap();
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    let _ = c.write_all(&[1u8; 100]).await;
    assert_eq!(rx.recv().await.unwrap().len(), 10);

    proxy.set_fault(Fault::None);
    let mut c2 = TcpStream::connect(proxy.addr()).await.unwrap();
    c2.write_all(&[2u8; 100]).await.unwrap();
    drop(c2);
    assert_eq!(rx.recv().await.unwrap(), vec![2u8; 100]);
    assert_eq!(proxy.connections(), 2);
}

#[test]
fn contains_works() {
    assert!(contains(b"hello world", b"lo wo"));
    assert!(contains(b"abc", b"abc"));
    assert!(contains(b"abc", b""));
    assert!(!contains(b"abc", b"abcd"));
    assert!(!contains(b"", b"a"));
    assert!(!contains(b"hello", b"world"));
}
