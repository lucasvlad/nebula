//! Dials paired devices that have no live session, every few seconds.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use nebula_core::DeviceId;
use nebula_net::discovery;
use nebula_net::transport;
use tracing::{debug, info};

use super::{Daemon, session};
use crate::pair;
use crate::run::RUN_PORT;

const EVERY: Duration = Duration::from_secs(10);
/// Kept short: this runs for every disconnected peer on every round.
const FIND_TIME: Duration = Duration::from_secs(2);

/// `explicit` are `--connect` addresses. They may answer as any paired
/// device; once one has, it is skipped while that device is connected.
pub async fn run(d: Arc<Daemon>, explicit: Vec<String>) {
    let mut known: HashMap<String, DeviceId> = HashMap::new();
    let mut ticks = tokio::time::interval(EVERY);
    loop {
        ticks.tick().await;
        let peers = match d.store().peers() {
            Ok(p) => p,
            Err(e) => {
                debug!("could not read paired devices: {e}");
                continue;
            }
        };
        let mut dialled: HashSet<DeviceId> = HashSet::new();

        for addr in &explicit {
            let wanted = match known.get(addr) {
                Some(dev) => peers.iter().any(|p| p.device_id == *dev) && !d.sessions.is_live(dev),
                None => peers.iter().any(|p| !d.sessions.is_live(&p.device_id)),
            };
            if !wanted {
                continue;
            }
            let keys: Vec<[u8; 32]> = peers.iter().map(|p| p.public_key).collect();
            match pair::resolve(addr, RUN_PORT).await {
                Ok(addrs) => match dial(&d, &addrs, &keys).await {
                    Ok(dev) => {
                        known.insert(addr.clone(), dev);
                        dialled.insert(dev);
                    }
                    Err(e) => debug!("could not connect to {addr}: {e:#}"),
                },
                Err(e) => debug!("{e:#}"),
            }
        }

        for peer in &peers {
            if dialled.contains(&peer.device_id) || d.sessions.is_live(&peer.device_id) {
                continue;
            }
            let mut addrs = match discovery::find_device(&peer.device_id, FIND_TIME).await {
                Ok(Some(found)) => found.addrs,
                Ok(None) => Vec::new(),
                Err(e) => {
                    debug!("local network discovery failed: {e}");
                    Vec::new()
                }
            };
            if let Some(last) = &peer.last_addr
                && let Ok(last) = pair::resolve(last, RUN_PORT).await
            {
                for a in last {
                    if !addrs.contains(&a) {
                        addrs.push(a);
                    }
                }
            }
            if addrs.is_empty() {
                debug!("\"{}\" was not found on the local network", peer.name);
                continue;
            }
            if let Err(e) = dial(&d, &addrs, &[peer.public_key]).await {
                debug!("could not connect to \"{}\": {e:#}", peer.name);
            }
        }
    }
}

/// Connects to the first address that answers, as the initiator, accepting
/// only the given keys, and starts the session.
async fn dial(d: &Arc<Daemon>, addrs: &[SocketAddr], keys: &[[u8; 32]]) -> Result<DeviceId> {
    let stream = pair::connect_any(addrs).await?;
    let remote = stream.peer_addr()?;
    let session = transport::connect(stream, &d.identity, |k| keys.contains(k)).await?;
    let device = session.remote_device();
    if let Err(e) = d.store().set_peer_addr(&device, &remote.to_string()) {
        info!("could not remember the address of {}: {e}", device.short());
    }
    tokio::spawn(session::run(d.clone(), session, remote, d.me));
    Ok(device)
}
