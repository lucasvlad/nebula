//! The daemon that keeps paired devices connected and feeds the sync
//! engine. Started by [`crate::Node`], for `nebula run` and for the app.
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
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::events::{EventKind, UiEvent};
use crate::net;
use engine::EngineHandle;
pub(crate) use session::Registry;

/// How often to rescan everything and check for added or removed folders.
const TICK: Duration = Duration::from_secs(30);
/// How often to check for devices unpaired while the daemon runs.
const REVOKE_CHECK: Duration = Duration::from_secs(5);
/// How long to let the engine finish its current step when stopping.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Incoming connections still in the secure handshake, i.e. not yet known to
/// be from a paired device. Each holds a socket for up to the handshake
/// timeout, so without a cap anyone on the network could use up the
/// daemon's file descriptors.
const MAX_HANDSHAKES: usize = 32;
/// Pause after a failed accept: the usual cause (out of file descriptors)
/// does not clear at once, and retrying immediately would spin the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// State shared by the daemon's tasks.
pub(crate) struct Daemon {
    pub(crate) identity: Identity,
    pub(crate) me: DeviceId,
    pub(crate) name: String,
    pub(crate) port: u16,
    /// For allowlist checks and peer lookups. The engine has its own connection.
    pub(crate) store: Mutex<Store>,
    pub(crate) engine: EngineHandle,
    pub(crate) sessions: Registry,
    pub(crate) events: broadcast::Sender<UiEvent>,
    /// Addresses to dial even without discovery (`--connect`, or the app).
    explicit: Mutex<Vec<String>>,
    /// Re-check folders and watches now rather than on the next tick.
    refresh: Notify,
}

impl Daemon {
    pub(crate) fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn peer_name(&self, device: &DeviceId) -> String {
        self.store()
            .peer(device)
            .ok()
            .flatten()
            .map_or_else(|| device.short(), |p| p.name)
    }

    /// Tells listeners (the app) about something. Never blocks.
    pub(crate) fn emit(&self, kind: EventKind) {
        let _ = self.events.send(UiEvent::now(kind));
    }

    pub(crate) fn explicit_addrs(&self) -> Vec<String> {
        self.explicit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn add_explicit(&self, addr: String) {
        let mut list = self.explicit.lock().unwrap_or_else(PoisonError::into_inner);
        if !list.contains(&addr) {
            list.push(addr);
        }
    }

    pub(crate) fn refresh_now(&self) {
        self.refresh.notify_one();
    }

    pub(crate) fn nudge(&self, input: Input) {
        self.engine.send(input);
    }
}

/// The running daemon: shared state, its main task, and a stop signal.
pub(crate) struct Running {
    pub(crate) daemon: Arc<Daemon>,
    stop: Arc<Notify>,
    main: JoinHandle<Result<()>>,
}

impl Running {
    /// Asks the daemon to stop and waits for it (and the engine) to finish.
    pub(crate) async fn shutdown(self) -> Result<()> {
        self.stop.notify_one();
        self.main
            .await
            .map_err(|e| anyhow!("daemon task failed: {e}"))?
    }

    /// Asks the daemon to stop without waiting for it.
    pub(crate) fn request_stop(&self) {
        self.stop.notify_one();
    }

    /// Waits until the daemon stops on its own (the sync engine failed).
    pub(crate) async fn stopped(&mut self) -> Result<()> {
        (&mut self.main)
            .await
            .map_err(|e| anyhow!("daemon task failed: {e}"))?
    }
}

/// Binds the port, announces this device, and starts the daemon in the
/// background. Fails if the port is taken (another Nebula running).
pub(crate) async fn start(
    identity: Identity,
    store: Store,
    db: &Path,
    port: u16,
    connect: Vec<String>,
    events: broadcast::Sender<UiEvent>,
) -> Result<Running> {
    let me = identity.device_id();
    let name = net::device_name(&store)?;
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("could not listen on port {port}; is Nebula already running?"))?;
    let port = listener.local_addr()?.port();
    let advert = match discovery::advertise(ServiceKind::Sync, &me, port) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!("could not announce on the local network ({e}); peers will need --connect");
            None
        }
    };

    let engine_store = Store::open(db).with_context(|| format!("opening {}", db.display()))?;
    let sessions = Registry::default();
    let (engine, engine_done) = engine::spawn(
        engine_store,
        me,
        name.clone(),
        sessions.clone(),
        events.clone(),
    )?;
    let daemon = Arc::new(Daemon {
        identity,
        me,
        name,
        port,
        store: Mutex::new(store),
        engine,
        sessions,
        events,
        explicit: Mutex::new(connect),
        refresh: Notify::new(),
    });
    let stop = Arc::new(Notify::new());
    let main = tokio::spawn(main_loop(
        daemon.clone(),
        listener,
        engine_done,
        stop.clone(),
        advert,
    ));
    Ok(Running { daemon, stop, main })
}

async fn main_loop(
    daemon: Arc<Daemon>,
    listener: TcpListener,
    mut engine_done: tokio::sync::oneshot::Receiver<engine::Stopped>,
    stop: Arc<Notify>,
    _advert: Option<discovery::Advertisement>,
) -> Result<()> {
    let connector = tokio::spawn(connector::run(daemon.clone()));
    let mut ticks = tokio::time::interval(TICK);
    let mut revocations = tokio::time::interval(REVOKE_CHECK);
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let mut watched: Option<Vec<Watched>> = None;
    let mut watching: Option<FolderWatcher> = None;
    let result = loop {
        tokio::select! {
            () = stop.notified() => break Ok(()),
            stopped = &mut engine_done => {
                let why = stopped.unwrap_or_else(|_| Err("its thread exited".into()));
                break Err(anyhow!("sync engine stopped: {}", why.err().unwrap_or_default()));
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, addr)) => match admit(&handshakes) {
                    Some(permit) => {
                        tokio::spawn(session::incoming(daemon.clone(), stream, addr, permit));
                    }
                    // Dropping the stream closes it.
                    None => debug!(
                        "closed a connection from {addr}: {MAX_HANDSHAKES} handshakes already in progress"
                    ),
                },
                Err(e) => {
                    warn!("could not accept a connection: {e}");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
            // The first tick fires at once: the startup scan.
            _ = ticks.tick() => {
                refreshwatching(&daemon, &mut watched, &mut watching);
                daemon.engine.send(Input::Tick);
            }
            () = daemon.refresh.notified() => {
                refreshwatching(&daemon, &mut watched, &mut watching);
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
                    daemon.emit(EventKind::StatusChanged);
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

/// A slot for one more pre-authentication handshake, if there is room.
fn admit(handshakes: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    handshakes.clone().try_acquire_owned().ok()
}

/// A folder as last watched: ID, root, and whether the root existed then.
type Watched = (i64, PathBuf, bool);

/// (Re)starts the watcher if the folder list changed since last time
/// (`nebula add` / `nebula remove` while running), or a root appeared or
/// disappeared (a USB drive plugged back in). The watcher skips roots it
/// cannot watch, so a missing root is only watched once it exists again.
fn refreshwatching(
    daemon: &Arc<Daemon>,
    watched: &mut Option<Vec<Watched>>,
    current: &mut Option<FolderWatcher>,
) {
    let folders: Vec<Folder> = match daemon.store().folders() {
        Ok(f) => f,
        Err(e) => {
            warn!("could not read the folder list: {e}");
            return;
        }
    };
    let now: Vec<Watched> = folders
        .iter()
        .map(|f| (f.id, f.root.clone(), f.root.is_dir()))
        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshakes_are_capped() {
        let sem = Arc::new(Semaphore::new(MAX_HANDSHAKES));
        let held: Vec<_> = (0..MAX_HANDSHAKES).filter_map(|_| admit(&sem)).collect();
        assert_eq!(held.len(), MAX_HANDSHAKES);
        assert!(admit(&sem).is_none());
        // A finished handshake frees its slot.
        drop(held);
        assert!(admit(&sem).is_some());
    }
}
