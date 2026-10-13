//! Pairing and the allowlist: `nebula pair`, `peers` and `unpair`.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nebula_core::DeviceId;
use nebula_net::discovery::{self, ServiceKind};
use nebula_net::identity::Identity;
use nebula_net::pairing::{self, CODE_LIFETIME, MAX_ATTEMPTS, PairedPeer, PairingCode};
use nebula_sync::store::{Peer, Store};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

/// Port `nebula pair` listens on, so `--addr` only needs the IP.
pub const PAIR_PORT: u16 = 7879;
const BROWSE_TIME: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// This device's name as shown to peers. Chosen once from the hostname
/// and remembered, because peers label conflict copies with it.
pub fn device_name(store: &Store) -> Result<String> {
    if let Some(name) = store.setting("name")? {
        return Ok(name);
    }
    let name = clean_name(&gethostname::gethostname().to_string_lossy());
    store.set_setting("name", &name)?;
    Ok(name)
}

pub fn set_device_name(store: &Store, name: &str) -> Result<String> {
    let name = clean_name(name);
    store.set_setting("name", &name)?;
    Ok(name)
}

fn clean_name(raw: &str) -> String {
    let raw = raw.trim().trim_end_matches(".local");
    let mut name = String::new();
    for c in raw.chars().filter(|c| !c.is_control()) {
        if name.len() + c.len_utf8() > pairing::MAX_NAME_LEN {
            break;
        }
        name.push(c);
    }
    let name = name.trim();
    if name.is_empty() {
        "Nebula device".to_string()
    } else {
        name.to_string()
    }
}

/// Device A: show a code and wait for the other device.
pub async fn listen(identity: &Identity, store: &Store, port: u16) -> Result<()> {
    let name = device_name(store)?;
    let code = PairingCode::generate()?;
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("port {port} is unavailable ({e}); using a random port instead");
            TcpListener::bind(("0.0.0.0", 0)).await?
        }
    };
    let port = listener.local_addr()?.port();
    let _advert = match discovery::advertise(ServiceKind::Pairing, &identity.device_id(), port) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!(
                "could not announce on the local network ({e}); the other device will need --addr"
            );
            None
        }
    };

    println!("Pairing code:  {code}");
    println!();
    println!("On your other device, run:");
    println!("    nebula pair {code}");
    let ips = local_ips();
    if let Some(ip) = ips.first() {
        println!("If it cannot find this device automatically, add:");
        println!("    --addr {}", SocketAddr::new(*ip, port));
    }
    println!();
    println!(
        "The code works for {} minutes and stops working after {MAX_ATTEMPTS} wrong tries.",
        CODE_LIFETIME.as_secs() / 60
    );
    println!("Waiting for the other device...");

    let peer = pairing::listen(listener, identity, &name, &code).await?;
    remember(store, &peer)?;
    confirm(&peer, &name, &identity.device_id());
    Ok(())
}

/// Device B: join a device that is showing `code`.
pub async fn join(
    identity: &Identity,
    store: &Store,
    code: &str,
    addr: Option<&str>,
) -> Result<()> {
    let code = PairingCode::parse(code)?;
    let name = device_name(store)?;
    let candidates = match addr {
        Some(addr) => resolve(addr, PAIR_PORT).await?,
        None => {
            println!("Looking for a device that is waiting to pair...");
            let found = discovery::browse(ServiceKind::Pairing, BROWSE_TIME).await?;
            match found.as_slice() {
                [] => bail!(
                    "no device is waiting to pair. Run `nebula pair` on the other device first, \
                     or pass --addr if this network blocks discovery"
                ),
                [one] => one.addrs.clone(),
                many => bail!(
                    "{} devices are waiting to pair; pick one with --addr ({})",
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
    let peer = pairing::join(stream, identity, &name, &code).await?;
    remember(store, &peer)?;
    confirm(&peer, &name, &identity.device_id());
    Ok(())
}

fn remember(store: &Store, peer: &PairedPeer) -> Result<()> {
    store.add_peer(&Peer {
        device_id: peer.device_id,
        public_key: peer.public_key,
        name: peer.name.clone(),
        last_addr: None,
    })?;
    Ok(())
}

fn confirm(peer: &PairedPeer, my_name: &str, me: &DeviceId) {
    println!();
    println!(
        "Paired with \"{}\" (device {}).",
        peer.name,
        peer.device_id.short()
    );
    println!(
        "The other device should say it paired with \"{my_name}\" (device {}). \
         If the names or IDs don't match, run `nebula unpair` on both.",
        me.short()
    );
}

pub fn list(store: &Store) -> Result<()> {
    let peers = store.peers()?;
    if peers.is_empty() {
        println!("No paired devices. Pair one with `nebula pair`.");
        return Ok(());
    }
    println!("{:<24}  {:<8}  LAST SEEN AT", "NAME", "DEVICE");
    for p in peers {
        println!(
            "{:<24}  {:<8}  {}",
            p.name,
            p.device_id.short(),
            p.last_addr.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// Removes a peer, matched by name (any case) or by the start of its ID.
pub fn unpair(store: &Store, who: &str) -> Result<()> {
    let peer = find_peer(store, who)?;
    store.remove_peer(&peer.device_id)?;
    println!(
        "Unpaired \"{}\" (device {}). It can no longer connect to this device.",
        peer.name,
        peer.device_id.short()
    );
    println!("Run `nebula unpair` on that device too to remove this one from its list.");
    Ok(())
}

pub fn find_peer(store: &Store, who: &str) -> Result<Peer> {
    let who_lower = who.to_lowercase();
    let mut matches: Vec<Peer> = store
        .peers()?
        .into_iter()
        .filter(|p| {
            p.name.to_lowercase() == who_lower
                || (who.len() >= 4 && p.device_id.to_string().starts_with(&who_lower))
        })
        .collect();
    match matches.len() {
        0 => bail!("no paired device called \"{who}\"; see `nebula peers`"),
        1 => Ok(matches.remove(0)),
        n => bail!("\"{who}\" matches {n} devices; use more of the device ID"),
    }
}

/// Accepts `host:port`, or just `host` to use `default_port`.
pub async fn resolve(addr: &str, default_port: u16) -> Result<Vec<SocketAddr>> {
    let with_port = if addr.parse::<SocketAddr>().is_ok()
        || addr
            .rsplit_once(':')
            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        addr.to_string()
    } else {
        format!("{addr}:{default_port}")
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("could not resolve {addr}"))?
        .collect();
    if addrs.is_empty() {
        bail!("could not resolve {addr}");
    }
    Ok(addrs)
}

pub async fn connect_any(addrs: &[SocketAddr]) -> Result<TcpStream> {
    let mut last_err = None;
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_err = Some(format!("{addr}: {e}")),
            Err(_) => last_err = Some(format!("{addr}: timed out")),
        }
    }
    bail!(
        "could not connect ({}). Check both devices are on the same network and \
         the other device's firewall allows Nebula",
        last_err.unwrap_or_else(|| "no addresses".into())
    )
}

/// Non-loopback addresses of this machine, IPv4 first, for `--addr` hints.
pub fn local_ips() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .map(|i| i.ip())
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !v4.is_link_local(),
            IpAddr::V6(_) => false,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("Lucass-MacBook.local"), "Lucass-MacBook");
        assert_eq!(clean_name("  pc\u{1b}[31m  "), "pc[31m");
        assert_eq!(clean_name(""), "Nebula device");
        assert_eq!(clean_name(&"é".repeat(40)).len(), 64);
    }
}
