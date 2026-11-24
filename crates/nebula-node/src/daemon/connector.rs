//! Dials paired devices that have no live session, every few seconds.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use nebula_core::DeviceId;
use nebula_net::discovery;
use nebula_net::transport;
use tracing::{debug, info};

use super::{Daemon, session};
use crate::net::{self, RUN_PORT};

const EVERY: Duration = Duration::from_secs(10);
/// Kept short: this runs for every disconnected peer on every round.
const FIND_TIME: Duration = Duration::from_secs(2);
/// If both devices dial each other at once, two sessions form and one is
/// dropped; the UI would briefly show two sets of security words. So only
/// the device with the lower ID dials straight away. The other waits this
/// long first, in case the network only lets connections through one way.
const HIGHER_ID_WAIT: Duration = Duration::from_secs(9);

/// Whether to dial `peer` now (see [`HIGHER_ID_WAIT`]).
fn may_dial(me: &DeviceId, peer: &DeviceId, offline_since: &HashMap<DeviceId, Instant>) -> bool {
    me < peer
        || offline_since
            .get(peer)
            .is_some_and(|t| t.elapsed() >= HIGHER_ID_WAIT)
}

/// Explicit addresses (`--connect`, or added from the app) may answer as
/// any paired device; once one has, it is skipped while that device is
/// connected.
pub async fn run(d: Arc<Daemon>) {
    let mut known: HashMap<String, DeviceId> = HashMap::new();
    let mut offline_since: HashMap<DeviceId, Instant> = HashMap::new();
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
        let explicit = d.explicit_addrs();
        for p in &peers {
            if d.sessions.is_live(&p.device_id) {
                offline_since.remove(&p.device_id);
            } else {
                offline_since
                    .entry(p.device_id)
                    .or_insert_with(Instant::now);
            }
        }
        let due = |dev: &DeviceId| !d.sessions.is_live(dev) && may_dial(&d.me, dev, &offline_since);

        for addr in &explicit {
            let wanted = match known.get(addr) {
                Some(dev) => peers.iter().any(|p| p.device_id == *dev) && due(dev),
                None => peers.iter().any(|p| due(&p.device_id)),
            };
            if !wanted {
                continue;
            }
            let keys: Vec<[u8; 32]> = peers.iter().map(|p| p.public_key).collect();
            match net::resolve(addr, RUN_PORT).await {
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
            if dialled.contains(&peer.device_id) || !due(&peer.device_id) {
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
                && let Ok(last) = net::resolve(last, RUN_PORT).await
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
    let stream = net::connect_any(addrs).await?;
    let remote = stream.peer_addr()?;
    let session = transport::connect(stream, &d.identity, |k| keys.contains(k)).await?;
    let device = session.remote_device();
    if let Err(e) = d.store().set_peer_addr(&device, &remote.to_string()) {
        info!("could not remember the address of {}: {e}", device.short());
    }
    tokio::spawn(session::run(d.clone(), session, remote, d.me));
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_id_dials_at_once_higher_id_waits() {
        let low = DeviceId([1; 32]);
        let high = DeviceId([2; 32]);
        let mut since = HashMap::new();
        assert!(may_dial(&low, &high, &since));
        assert!(!may_dial(&high, &low, &since));
        since.insert(low, Instant::now());
        assert!(!may_dial(&high, &low, &since));
        since.insert(low, Instant::now() - HIGHER_ID_WAIT);
        assert!(may_dial(&high, &low, &since));
    }
}
