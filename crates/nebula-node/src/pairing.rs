//! Pairing flows without any printing, so the CLI and the app can each
//! present them their own way.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, bail};
use nebula_net::discovery::{self, Advertisement, ServiceKind};
use nebula_net::identity::Identity;
use nebula_net::pairing::{self, PairedPeer, PairingCode};
use nebula_sync::store::{Peer, Store};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::net::{PAIR_PORT, connect_any, resolve};

const BROWSE_TIME: Duration = Duration::from_secs(4);

/// A pairing code being shown, with the listener waiting for the other device.
pub struct Offer {
    pub code: PairingCode,
    pub port: u16,
    listener: TcpListener,
    _advert: Option<Advertisement>,
}

/// Device A: picks a code, listens on `port` (or any free port if it is
/// taken) and announces itself on the local network.
pub async fn offer(identity: &Identity, port: u16) -> Result<Offer> {
    let code = PairingCode::generate()?;
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("port {port} is unavailable ({e}); using a random port instead");
            TcpListener::bind(("0.0.0.0", 0)).await?
        }
    };
    let port = listener.local_addr()?.port();
    let advert = match discovery::advertise(ServiceKind::Pairing, &identity.device_id(), port) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!(
                "could not announce on the local network ({e}); the other device will need its address"
            );
            None
        }
    };
    Ok(Offer {
        code,
        port,
        listener,
        _advert: advert,
    })
}

impl Offer {
    /// Waits for the other device to enter the code (or for expiry or too
    /// many wrong tries).
    pub async fn wait(self, identity: &Identity, my_name: &str) -> Result<PairedPeer> {
        Ok(pairing::listen(self.listener, identity, my_name, &self.code).await?)
    }
}

/// Device B: finds a device showing `code` (or uses `addr`) and pairs.
pub async fn join(
    identity: &Identity,
    my_name: &str,
    code: &str,
    addr: Option<&str>,
) -> Result<PairedPeer> {
    let code = PairingCode::parse(code)?;
    let candidates = match addr {
        Some(addr) => resolve(addr, PAIR_PORT).await?,
        None => {
            let found = discovery::browse(ServiceKind::Pairing, BROWSE_TIME).await?;
            match found.as_slice() {
                [] => bail!(
                    "no device is waiting to pair. Start pairing on the other device first, \
                     or give its address if this network blocks discovery"
                ),
                [one] => one.addrs.clone(),
                many => bail!(
                    "{} devices are waiting to pair; pick one by address ({})",
                    many.len(),
                    many.iter()
                        .filter_map(|f| f.addrs.first())
                        .map(SocketAddr::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    let stream = connect_any(&candidates).await?;
    info!("connected to {}; checking the code", stream.peer_addr()?);
    Ok(pairing::join(stream, identity, my_name, &code).await?)
}

/// Adds a newly paired device to the allowlist.
pub fn remember(store: &Store, peer: &PairedPeer) -> Result<()> {
    store.add_peer(&Peer {
        device_id: peer.device_id,
        public_key: peer.public_key,
        name: peer.name.clone(),
        last_addr: None,
    })?;
    Ok(())
}
