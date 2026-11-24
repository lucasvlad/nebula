//! Helpers shared by the CLI and the app: ports, device naming, addresses,
//! and the session greeting.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nebula_net::pairing;
use nebula_net::transport::TransportError;
use nebula_sync::protocol::{Message, PROTOCOL_VERSION};
use nebula_sync::store::{Peer, Store};
use tokio::net::TcpStream;

/// Port `nebula run` (and the app) listens on.
pub const RUN_PORT: u16 = 7878;
/// Port pairing listens on, so `--addr` only needs the IP.
pub const PAIR_PORT: u16 = 7879;
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

/// A paired device, matched by name (any case) or by the start of its ID.
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
        0 => bail!("no paired device called \"{who}\""),
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

/// Non-loopback IPv4 addresses of this machine, for "reach me at" hints.
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

/// The first message a daemon sends on every session.
pub fn hello(name: &str) -> Message {
    Message::Hello {
        name: name.to_string(),
        protocol: PROTOCOL_VERSION,
    }
}

pub fn check_protocol(theirs: u32) -> Result<()> {
    if theirs != PROTOCOL_VERSION {
        bail!(
            "the other device speaks protocol version {theirs}, this one {PROTOCOL_VERSION}; \
             update Nebula on both"
        );
    }
    Ok(())
}

pub fn is_closed(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
    )
}

pub fn is_hangup(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<TransportError>(), Some(TransportError::Io(io)) if is_closed(io))
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
