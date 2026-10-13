//! mDNS discovery. Advertises only the device ID (brief §4): no folder names,
//! file info, user name or real hostname ever go on the wire.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::SocketAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use nebula_core::DeviceId;
use tokio::time::Instant;

const SYNC_TYPE: &str = "_nebula._tcp.local.";
const PAIR_TYPE: &str = "_nebula-pair._tcp.local.";
const TXT_VERSION: &str = "1";

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mdns: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// Which kind of service to advertise or browse for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    /// A running Nebula daemon (`_nebula._tcp`).
    Sync,
    /// A device currently showing a pairing code (`_nebula-pair._tcp`).
    Pairing,
}

impl ServiceKind {
    fn service_type(self) -> &'static str {
        match self {
            ServiceKind::Sync => SYNC_TYPE,
            ServiceKind::Pairing => PAIR_TYPE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// `None` for pairing services, which do not reveal an ID.
    pub device_id: Option<DeviceId>,
    /// IPv4 addresses first, then IPv6.
    pub addrs: Vec<SocketAddr>,
}

/// A live mDNS registration. Unregisters when dropped (best-effort).
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Advertises this device on the LAN.
///
/// The Sync instance is `nebula-<short id>` and carries `id=<64 hex>` and `v=1`.
/// The Pairing instance is `nebula-pair-<random 8 hex>` and carries only `v=1`:
/// the joiner does not know the ID yet, and a random name avoids leaking any
/// part of the ID (or linking a pairing window to a daemon) to the LAN.
/// The host name is `<instance>.local.`, never the machine's real hostname.
pub fn advertise(
    kind: ServiceKind,
    device: &DeviceId,
    port: u16,
) -> Result<Advertisement, DiscoveryError> {
    let (instance, mut props) = match kind {
        ServiceKind::Sync => (format!("nebula-{}", device.short()), {
            let mut p = HashMap::new();
            p.insert("id".to_string(), device.to_string());
            p
        }),
        ServiceKind::Pairing => (format!("nebula-pair-{}", random_hex8()), HashMap::new()),
    };
    props.insert("v".to_string(), TXT_VERSION.to_string());
    let host = format!("{instance}.local.");
    let info = ServiceInfo::new(kind.service_type(), &instance, &host, "", port, props)?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    let daemon = ServiceDaemon::new()?;
    if let Err(e) = daemon.register(info) {
        let _ = daemon.shutdown();
        return Err(e.into());
    }
    Ok(Advertisement { daemon, fullname })
}

/// Browses for `timeout`, returning every distinct instance resolved in that
/// window (deduped by instance fullname, addresses merged).
pub async fn browse(kind: ServiceKind, timeout: Duration) -> Result<Vec<Found>, DiscoveryError> {
    let daemon = ServiceDaemon::new()?;
    let result = browse_with(&daemon, kind, timeout).await;
    let _ = daemon.stop_browse(kind.service_type());
    let _ = daemon.shutdown();
    result
}

async fn browse_with(
    daemon: &ServiceDaemon,
    kind: ServiceKind,
    timeout: Duration,
) -> Result<Vec<Found>, DiscoveryError> {
    let rx = daemon.browse(kind.service_type())?;
    let deadline = Instant::now() + timeout;
    let mut seen: HashMap<String, Found> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv_async()).await {
        let ServiceEvent::ServiceResolved(svc) = event else {
            continue;
        };
        let device_id = svc.get_property_val_str("id").and_then(parse_device_id);
        if kind == ServiceKind::Sync && device_id.is_none() {
            continue; // malformed or hostile advertisement
        }
        let entry = seen.entry(svc.fullname.clone()).or_insert_with(|| {
            order.push(svc.fullname.clone());
            Found {
                device_id,
                addrs: Vec::new(),
            }
        });
        for ip in &svc.addresses {
            let addr = SocketAddr::new(ip.to_ip_addr(), svc.port);
            if !entry.addrs.contains(&addr) {
                entry.addrs.push(addr);
            }
        }
        sort_addrs(&mut entry.addrs);
    }
    Ok(order.into_iter().filter_map(|n| seen.remove(&n)).collect())
}

/// First resolved Sync instance whose device ID matches.
pub async fn find_device(
    device: &DeviceId,
    timeout: Duration,
) -> Result<Option<Found>, DiscoveryError> {
    let all = browse(ServiceKind::Sync, timeout).await?;
    Ok(all
        .into_iter()
        .find(|f| f.device_id.as_ref() == Some(device)))
}

fn sort_addrs(addrs: &mut [SocketAddr]) {
    addrs.sort_by_key(|a| (a.is_ipv6(), *a));
}

/// Randomness without a new dependency: `RandomState` is seeded by the OS.
fn random_hex8() -> String {
    let n = RandomState::new().build_hasher().finish() as u32;
    format!("{n:08x}")
}

/// Strict: exactly 64 hex characters (either case).
fn parse_device_id(s: &str) -> Option<DeviceId> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = (nibble(b[2 * i])? << 4) | nibble(b[2 * i + 1])?;
    }
    Some(DeviceId(out))
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: [u8; 32] = [0xab; 32];

    #[test]
    fn parses_valid_id_both_cases() {
        let lower = "ab".repeat(32);
        assert_eq!(parse_device_id(&lower), Some(DeviceId(ID)));
        assert_eq!(parse_device_id(&lower.to_uppercase()), Some(DeviceId(ID)));
    }

    #[test]
    fn rejects_bad_ids() {
        assert_eq!(parse_device_id(""), None);
        assert_eq!(parse_device_id(&"ab".repeat(31)), None);
        assert_eq!(parse_device_id(&"ab".repeat(33)), None);
        assert_eq!(parse_device_id(&format!("{}zz", "ab".repeat(31))), None);
        // 64 bytes long but multi-byte chars must not panic or parse
        assert_eq!(parse_device_id(&"é".repeat(32)), None);
        assert_eq!(parse_device_id(&format!("+{}", "a".repeat(63))), None);
    }

    #[test]
    fn service_types() {
        assert_eq!(ServiceKind::Sync.service_type(), "_nebula._tcp.local.");
        assert_eq!(
            ServiceKind::Pairing.service_type(),
            "_nebula-pair._tcp.local."
        );
    }

    #[test]
    fn ipv4_sorts_first() {
        let mut v: Vec<SocketAddr> = vec![
            "[fe80::1]:4242".parse().unwrap(),
            "192.168.1.9:4242".parse().unwrap(),
            "10.0.0.2:4242".parse().unwrap(),
        ];
        sort_addrs(&mut v);
        let want: Vec<SocketAddr> = vec![
            "10.0.0.2:4242".parse().unwrap(),
            "192.168.1.9:4242".parse().unwrap(),
            "[fe80::1]:4242".parse().unwrap(),
        ];
        assert_eq!(v, want);
    }

    #[test]
    fn pairing_names_are_random_hex() {
        let a = random_hex8();
        assert_eq!(a.len(), 8);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    #[ignore = "needs multicast; run with --ignored"]
    async fn sync_advertisement_is_found() {
        let id = DeviceId([0x42; 32]);
        let _ad = advertise(ServiceKind::Sync, &id, 4242).unwrap();
        let found = find_device(&id, Duration::from_secs(3)).await.unwrap();
        let found = found.unwrap();
        assert!(!found.addrs.is_empty());
        assert!(found.addrs.iter().all(|a| a.port() == 4242));
    }

    #[tokio::test]
    #[ignore = "needs multicast; run with --ignored"]
    async fn pairing_advertisement_is_found() {
        let id = DeviceId([0x43; 32]);
        let _ad = advertise(ServiceKind::Pairing, &id, 4343).unwrap();
        let all = browse(ServiceKind::Pairing, Duration::from_secs(3))
            .await
            .unwrap();
        assert!(
            all.iter()
                .any(|f| f.device_id.is_none() && f.addrs.iter().any(|a| a.port() == 4343))
        );
    }

    #[tokio::test]
    #[ignore = "needs multicast; run with --ignored"]
    async fn drop_unregisters() {
        let id = DeviceId([0x44; 32]);
        let ad = advertise(ServiceKind::Sync, &id, 4444).unwrap();
        assert!(
            find_device(&id, Duration::from_secs(3))
                .await
                .unwrap()
                .is_some()
        );
        drop(ad);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            find_device(&id, Duration::from_secs(2))
                .await
                .unwrap()
                .is_none()
        );
    }
}
