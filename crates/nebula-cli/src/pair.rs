//! Pairing and the allowlist in the terminal: `nebula pair`, `peers` and
//! `unpair`. The pairing itself lives in `nebula_node::pairing`, shared with
//! the app.

use std::net::SocketAddr;

use anyhow::Result;
use nebula_core::DeviceId;
use nebula_net::identity::Identity;
use nebula_net::pairing::{CODE_LIFETIME, MAX_ATTEMPTS, PairedPeer};
use nebula_net::sas::SecurityCode;
use nebula_node::net::{device_name, find_peer, local_ips};
use nebula_node::pairing;
use nebula_sync::store::Store;

/// Device A: show a code and wait for the other device.
pub async fn listen(identity: &Identity, store: &Store, port: u16) -> Result<()> {
    let name = device_name(store)?;
    let offer = pairing::offer(identity, port).await?;
    let code = offer.code.clone();

    println!("Pairing code:  {code}");
    println!();
    println!("On your other device, run:");
    println!("    nebula pair {code}");
    if let Some(ip) = local_ips().first() {
        println!("If it cannot find this device automatically, add:");
        println!("    --addr {}", SocketAddr::new(*ip, offer.port));
    }
    println!();
    println!(
        "The code works for {} minutes and stops working after {MAX_ATTEMPTS} wrong tries.",
        CODE_LIFETIME.as_secs() / 60
    );
    println!("Waiting for the other device...");

    let peer = offer.wait(identity, &name).await?;
    pairing::remember(store, &peer)?;
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
    let name = device_name(store)?;
    if addr.is_none() {
        println!("Looking for a device that is waiting to pair...");
    }
    let peer = pairing::join(identity, &name, code, addr).await?;
    pairing::remember(store, &peer)?;
    confirm(&peer, &name, &identity.device_id());
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
        "The other device should say it paired with \"{my_name}\" (device {}).",
        me.short()
    );
    println!();
    print_security_code(&peer.security_code);
    println!(
        "If they differ, someone may be in the middle: run `nebula unpair` on both \
         and pair again."
    );
}

/// The four words, set apart so they are easy to compare across screens.
pub fn print_security_code(code: &SecurityCode) {
    let words = code.words().map(str::to_uppercase);
    println!("Security code:   {}", words.join("   "));
    println!("Check that the other device shows the same four words.");
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
