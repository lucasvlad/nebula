//! `nebula run` accepts encrypted sessions from paired devices; `nebula ping`
//! opens one and measures the round trip. Syncing over these sessions
//! comes in M3.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nebula_net::discovery::{self, ServiceKind};
use nebula_net::identity::Identity;
use nebula_net::transport::{self, Session, TransportError};
use nebula_sync::protocol::{Message, PROTOCOL_VERSION};
use nebula_sync::store::Store;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::pair;

/// Port `nebula run` listens on.
pub const RUN_PORT: u16 = 7878;
const FIND_TIME: Duration = Duration::from_secs(4);

pub async fn run(identity: Identity, store: Store, port: u16) -> Result<()> {
    let me = identity.device_id();
    let name = pair::device_name(&store)?;
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("could not listen on port {port}; is Nebula already running?"))?;
    let _advert = match discovery::advertise(ServiceKind::Sync, &me, port) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!("could not announce on the local network ({e}); peers will need --addr");
            None
        }
    };
    let peers = store.peers()?.len();
    println!(
        "Nebula is running as \"{name}\" (device {}) on port {port}, with {peers} paired device{}.",
        me.short(),
        if peers == 1 { "" } else { "s" }
    );
    if let Some(ip) = pair::local_ips().first() {
        println!(
            "Other devices can reach it at {}.",
            SocketAddr::new(*ip, port)
        );
    }
    println!("Press Ctrl+C to stop.");

    let identity = Arc::new(identity);
    let store = Arc::new(Mutex::new(store));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Stopping.");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, addr) = accepted?;
                tokio::spawn(serve(stream, addr, identity.clone(), store.clone(), name.clone()));
            }
        }
    }
}

async fn serve(
    stream: TcpStream,
    addr: SocketAddr,
    identity: Arc<Identity>,
    store: Arc<Mutex<Store>>,
    my_name: String,
) {
    let allowed = |key: &[u8; 32]| {
        store
            .lock()
            .map(|s| s.is_paired(key).unwrap_or(false))
            .unwrap_or(false)
    };
    let mut session = match transport::accept(stream, &identity, allowed).await {
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
    let device = session.remote_device();
    let peer_name = store
        .lock()
        .ok()
        .and_then(|s| s.peer(&device).ok().flatten())
        .map_or_else(|| device.short(), |p| p.name);
    info!(
        "encrypted session with \"{peer_name}\" ({}) from {addr}; security code: {}",
        device.short(),
        session.security_code().to_string().to_uppercase()
    );
    match answer(&mut session, &my_name, &peer_name).await {
        Ok(()) => info!("\"{peer_name}\" disconnected"),
        Err(e) => warn!("session with \"{peer_name}\" ended: {e}"),
    }
}

/// Says hello, then answers pings until the peer hangs up.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut Session<S>,
    my_name: &str,
    peer_name: &str,
) -> Result<()> {
    send(session, &hello(my_name)).await?;
    loop {
        let msg = match recv(session).await {
            Ok(m) => m,
            Err(e) if is_hangup(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        match msg {
            Message::Hello { protocol, .. } => check_protocol(protocol)?,
            Message::Ping { nonce } => send(session, &Message::Pong { nonce }).await?,
            Message::Pong { .. } => {}
            Message::Note { text } => {
                let text: String = text.chars().filter(|c| !c.is_control()).take(500).collect();
                info!("\"{peer_name}\" sent a message, decrypted here: \"{text}\"");
            }
        }
    }
}

pub async fn ping(
    identity: Identity,
    store: Store,
    who: Option<&str>,
    addr: Option<&str>,
    count: u32,
    message: Option<&str>,
) -> Result<()> {
    if store.peers()?.is_empty() {
        bail!("this device is not paired with anything yet. Pair first with `nebula pair`");
    }
    let target = match who {
        Some(who) => Some(pair::find_peer(&store, who)?),
        None if addr.is_some() => None,
        None => match store.peers()?.as_slice() {
            [only] => Some(only.clone()),
            _ => bail!("which device? Name one (see `nebula peers`) or pass --addr"),
        },
    };

    let addrs = match (addr, &target) {
        (Some(addr), _) => pair::resolve(addr, RUN_PORT).await?,
        (None, Some(peer)) => {
            println!("Looking for \"{}\" on the local network...", peer.name);
            match discovery::find_device(&peer.device_id, FIND_TIME).await? {
                Some(found) => found.addrs,
                None => match &peer.last_addr {
                    Some(last) => {
                        println!("Not announced; trying its last address, {last}.");
                        pair::resolve(last, RUN_PORT).await?
                    }
                    None => bail!(
                        "could not find \"{}\". Make sure `nebula run` is running on it, \
                         or pass --addr if this network blocks discovery",
                        peer.name
                    ),
                },
            }
        }
        (None, None) => bail!("pass a device name or --addr"),
    };

    let stream = pair::connect_any(&addrs).await?;
    let remote = stream.peer_addr()?;
    // With a named target only that device may answer; with a bare --addr,
    // any paired device may.
    let allowed_keys: Vec<[u8; 32]> = match &target {
        Some(peer) => vec![peer.public_key],
        None => store.peers()?.iter().map(|p| p.public_key).collect(),
    };

    let allowed = |key: &[u8; 32]| allowed_keys.contains(key);
    let mut session = transport::connect(stream, &identity, allowed)
        .await
        .map_err(|e| explain_refusal(e, target.is_some()))?;
    let device = session.remote_device();
    send(&mut session, &hello(&pair::device_name(&store)?)).await?;
    match recv(&mut session).await? {
        Message::Hello { protocol, .. } => check_protocol(protocol)?,
        other => bail!("peer did not say hello first: {other:?}"),
    }
    store.set_peer_addr(&device, &remote.to_string())?;
    let name = store
        .peer(&device)?
        .map_or_else(|| device.short(), |p| p.name);
    println!(
        "Connected to \"{name}\" (device {}) at {remote}.",
        device.short()
    );
    println!(
        "The connection is encrypted, and both devices proved they are paired with each other."
    );
    println!();
    pair::print_security_code(&session.security_code());
    println!();
    if let Some(text) = message {
        let note = Message::Note {
            text: text.to_string(),
        };
        send(&mut session, &note).await?;
        println!("Sent \"{text}\" encrypted. The other device's log shows it decrypted.");
    }

    for nonce in 0..u64::from(count) {
        let started = Instant::now();
        send(&mut session, &Message::Ping { nonce }).await?;
        loop {
            if let Message::Pong { nonce: n } = recv(&mut session).await?
                && n == nonce
            {
                break;
            }
        }
        println!(
            "  reply {}: {:.1} ms",
            nonce + 1,
            started.elapsed().as_secs_f64() * 1000.0
        );
        if nonce + 1 < u64::from(count) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    Ok(())
}

fn hello(name: &str) -> Message {
    Message::Hello {
        name: name.to_string(),
        protocol: PROTOCOL_VERSION,
    }
}

fn check_protocol(theirs: u32) -> Result<()> {
    if theirs != PROTOCOL_VERSION {
        bail!(
            "the other device speaks protocol version {theirs}, this one {PROTOCOL_VERSION}; \
             update Nebula on both"
        );
    }
    Ok(())
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(s: &mut Session<S>, m: &Message) -> Result<()> {
    s.send(&m.encode()).await?;
    Ok(())
}

async fn recv<S: AsyncRead + AsyncWrite + Unpin>(s: &mut Session<S>) -> Result<Message> {
    let bytes = s.recv().await?;
    Message::decode(&bytes).map_err(|e| anyhow!("peer sent a malformed message: {e}"))
}

fn is_closed(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
    )
}

fn is_hangup(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<TransportError>(), Some(TransportError::Io(io)) if is_closed(io))
}

fn explain_refusal(e: TransportError, named_target: bool) -> anyhow::Error {
    match e {
        TransportError::NotAllowed(device) if named_target => anyhow!(
            "the device that answered is {}, not the paired device you asked for; \
             nothing was sent to it",
            device.short()
        ),
        TransportError::NotAllowed(device) => anyhow!(
            "the device that answered is {}, which is not paired with this one; \
             nothing was sent to it",
            device.short()
        ),
        TransportError::Io(io) if is_closed(&io) => {
            anyhow!(
                "the other device refused the connection. It is probably not paired with this \
                 one; check `nebula peers` on both"
            )
        }
        other => other.into(),
    }
}
