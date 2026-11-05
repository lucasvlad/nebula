//! `nebula ping` opens an encrypted session to a running daemon and measures
//! the round trip, plus helpers shared with the daemon (`crate::daemon`).

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use nebula_net::discovery;
use nebula_net::identity::Identity;
use nebula_net::transport::{self, Session, TransportError};
use nebula_sync::protocol::{Message, PROTOCOL_VERSION};
use nebula_sync::store::Store;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::pair;

/// Port `nebula run` listens on.
pub const RUN_PORT: u16 = 7878;
const FIND_TIME: Duration = Duration::from_secs(4);

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
    // We do not say Hello: a session that opens with Ping or Note is a ping
    // session to the daemon, answered but kept apart from syncing, so it
    // cannot displace this device's own daemon's sync session.
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

pub(crate) fn hello(name: &str) -> Message {
    Message::Hello {
        name: name.to_string(),
        protocol: PROTOCOL_VERSION,
    }
}

pub(crate) fn check_protocol(theirs: u32) -> Result<()> {
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

pub(crate) fn is_closed(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
    )
}

pub(crate) fn is_hangup(e: &anyhow::Error) -> bool {
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
