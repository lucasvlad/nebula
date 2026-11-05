//! `nebula run`: the daemon that keeps paired devices connected and feeds
//! the sync engine.
//!
//! ```text
//!  watcher ──LocalChange──┐
//!  30 s timer ──Tick──────┤
//!  session readers ───────┼──> engine thread ──Output──> registry ──> session writers
//!   (Connected, Received, │     (owns Engine,             (peer -> outbound channel)
//!    Disconnected)        ┘      blocking IO)
//! ```
//!
//! The engine is synchronous and does file IO, so it runs on its own thread
//! and is fed through a std channel. Everything network-facing runs on
//! tokio: the accept loop, the outbound connector, and one task per session.

mod connector;
mod engine;
mod session;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use nebula_core::DeviceId;
use nebula_net::discovery::{self, ServiceKind};
use nebula_net::identity::Identity;
use nebula_sync::engine::Input;
use nebula_sync::store::{Folder, Store};
use nebula_sync::watcher::{self, FolderWatcher};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::pair;
use engine::EngineHandle;
use session::Registry;

/// How often to rescan everything and check for added or removed folders.
const TICK: Duration = Duration::from_secs(30);
/// How often to check for devices unpaired while the daemon runs.
const REVOKE_CHECK: Duration = Duration::from_secs(5);
/// How long to let the engine finish its current step when stopping.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// State shared by the daemon's tasks.
struct Daemon {
    identity: Identity,
    me: DeviceId,
    name: String,
    /// For allowlist checks and peer lookups. The engine has its own connection.
    store: Mutex<Store>,
    engine: EngineHandle,
    sessions: Registry,
}

impl Daemon {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn peer_name(&self, device: &DeviceId) -> String {
        self.store()
            .peer(device)
            .ok()
            .flatten()
            .map_or_else(|| device.short(), |p| p.name)
    }
}

/// Runs until Ctrl+C, or until the sync engine stops (an error).
pub async fn run(
    identity: Identity,
    store: Store,
    db: &Path,
    port: u16,
    connect: Vec<String>,
) -> Result<()> {
    let me = identity.device_id();
    let name = pair::device_name(&store)?;
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("could not listen on port {port}; is Nebula already running?"))?;
    let _advert = match discovery::advertise(ServiceKind::Sync, &me, port) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!("could not announce on the local network ({e}); peers will need --connect");
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

    let engine_store = Store::open(db).with_context(|| format!("opening {}", db.display()))?;
    let sessions = Registry::default();
    let (engine, mut engine_done) =
        engine::spawn(engine_store, me, name.clone(), sessions.clone())?;
    let daemon = Arc::new(Daemon {
        identity,
        me,
        name,
        store: Mutex::new(store),
        engine,
        sessions,
    });
    let connector = tokio::spawn(connector::run(daemon.clone(), connect));

    let mut ticks = tokio::time::interval(TICK);
    let mut revocations = tokio::time::interval(REVOKE_CHECK);
    let mut watched: Option<Vec<(i64, PathBuf)>> = None;
    let mut watching: Option<FolderWatcher> = None;
    let result = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Stopping.");
                break Ok(());
            }
            stopped = &mut engine_done => {
                let why = stopped.unwrap_or_else(|_| Err("its thread exited".into()));
                break Err(anyhow!("sync engine stopped: {}", why.err().unwrap_or_default()));
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, addr)) => {
                    tokio::spawn(session::incoming(daemon.clone(), stream, addr));
                }
                Err(e) => warn!("could not accept a connection: {e}"),
            },
            // The first tick fires at once: the startup scan.
            _ = ticks.tick() => {
                refreshwatching(&daemon, &mut watched, &mut watching);
                daemon.engine.send(Input::Tick);
            }
            _ = revocations.tick() => {
                let paired: Vec<DeviceId> = match daemon.store().peers() {
                    Ok(peers) => peers.into_iter().map(|p| p.device_id).collect(),
                    Err(e) => {
                        warn!("could not read the paired device list: {e}");
                        continue;
                    }
                };
                for peer in daemon.sessions.drop_unpaired(&paired, &daemon.engine) {
                    info!("closed the session with {}: it was unpaired", peer.short());
                }
            }
        }
    };

    connector.abort();
    daemon.sessions.close_all();
    drop(watching);
    if result.is_ok() {
        daemon.engine.stop();
        if tokio::time::timeout(STOP_GRACE, engine_done).await.is_err() {
            warn!("the sync engine did not finish its current step in time; stopping anyway");
        }
    }
    result
}

/// (Re)starts the watcher if the folder list changed since last time
/// (`nebula add` / `nebula remove` while running).
fn refreshwatching(
    daemon: &Arc<Daemon>,
    watched: &mut Option<Vec<(i64, PathBuf)>>,
    current: &mut Option<FolderWatcher>,
) {
    let folders: Vec<Folder> = match daemon.store().folders() {
        Ok(f) => f,
        Err(e) => {
            warn!("could not read the folder list: {e}");
            return;
        }
    };
    let now: Vec<(i64, PathBuf)> = folders.iter().map(|f| (f.id, f.root.clone())).collect();
    if watched.as_ref() == Some(&now) {
        return;
    }
    *current = None; // stop the old watches first
    let engine = daemon.engine.clone();
    match watcher::watch(&folders, move |folder| {
        engine.send(Input::LocalChange { folder });
    }) {
        Ok(w) => {
            info!(
                "watching {} folder{} for changes",
                folders.len(),
                if folders.len() == 1 { "" } else { "s" }
            );
            *current = Some(w);
            *watched = Some(now);
        }
        // Retried next tick; the periodic rescan still catches changes.
        Err(e) => warn!("{e}"),
    }
}
