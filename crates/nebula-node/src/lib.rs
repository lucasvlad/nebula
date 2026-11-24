//! A running Nebula device: the sync daemon, pairing and status, behind one
//! [`Node`] handle. The `nebula` CLI and the desktop app both use it, so they
//! run exactly the same code; the app adds a window, the CLI a terminal.

mod daemon;
pub mod events;
pub mod net;
pub mod pairing;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result, bail};
use nebula_net::identity::Identity;
use nebula_net::pairing::CODE_LIFETIME;
use nebula_sync::engine::Input;
use nebula_sync::store::{Folder, Store};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::warn;

use daemon::{Daemon, Running};
use events::{EventKind, FolderStatus, Me, Paired, PeerStatus, Status, UiEvent};

/// Events kept for slow listeners before the oldest are dropped.
const EVENT_BUFFER: usize = 1024;

/// The default state directory for this OS user.
pub fn default_home() -> Result<PathBuf> {
    directories::ProjectDirs::from("", "", "nebula")
        .map(|d| d.data_dir().to_path_buf())
        .context("could not find a home directory; pass --home")
}

/// This device's key pair, created on first use.
pub fn identity(home: &Path) -> Result<Identity> {
    let path = home.join("identity.key");
    Identity::load_or_create(&path).with_context(|| format!("loading {}", path.display()))
}

pub fn open_store(home: &Path) -> Result<Store> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let path = home.join("state.db");
    Store::open(&path).with_context(|| format!("opening {}", path.display()))
}

/// Opts a folder in, refusing folders that would sync Nebula's own state or
/// overlap another synced folder. `label` defaults to the folder's name.
pub fn add_folder(
    store: &Store,
    home: &Path,
    folder: &Path,
    label: Option<&str>,
) -> Result<Folder> {
    let root = std::fs::canonicalize(folder)
        .with_context(|| format!("{} does not exist", folder.display()))?;
    if !root.is_dir() {
        bail!("{} is not a folder", root.display());
    }
    // Nebula's own state must not sync, or every write would trigger a sync.
    let home = std::fs::canonicalize(home)?;
    if home.starts_with(&root) {
        bail!(
            "{} contains Nebula's own data folder ({})",
            root.display(),
            home.display()
        );
    }
    for other in store.folders()? {
        if root.starts_with(&other.root) || other.root.starts_with(&root) {
            bail!(
                "{} overlaps the synced folder {}",
                root.display(),
                other.root.display()
            );
        }
    }
    let label = match label {
        Some(l) => l.trim().to_string(),
        None => root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".to_string()),
    };
    if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
        bail!("folder names must be 1 to 64 characters");
    }
    Ok(store.add_folder(&root, &label)?)
}

pub struct NodeConfig {
    pub home: PathBuf,
    pub port: u16,
    /// Addresses to dial even if discovery doesn't find the device.
    pub connect: Vec<String>,
}

/// A running device. Dropping it without [`Node::shutdown`] stops the
/// network tasks when the tokio runtime ends.
pub struct Node {
    home: PathBuf,
    running: Running,
    pairing: Mutex<Option<JoinHandle<()>>>,
}

impl Node {
    pub async fn start(config: NodeConfig) -> Result<Self> {
        let identity = identity(&config.home)?;
        let store = open_store(&config.home)?;
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let running = daemon::start(
            identity,
            store,
            &config.home.join("state.db"),
            config.port,
            config.connect,
            events,
        )
        .await?;
        Ok(Self {
            home: config.home,
            running,
            pairing: Mutex::new(None),
        })
    }

    fn d(&self) -> &Arc<Daemon> {
        &self.running.daemon
    }

    /// A live stream of activity for the UI.
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.d().events.subscribe()
    }

    pub fn status(&self) -> Result<Status> {
        let d = self.d();
        let store = d.store();
        let peers = store
            .peers()?
            .into_iter()
            .map(|p| {
                let live = d.sessions.info(&p.device_id);
                PeerStatus {
                    name: p.name,
                    id: p.device_id.short(),
                    online: live.is_some(),
                    address: live.as_ref().map(|(a, _)| a.to_string()),
                    security_code: live.map(|(_, c)| c),
                }
            })
            .collect();
        let mut folders = Vec::new();
        for f in store.folders()? {
            let live: Vec<_> = store
                .files(f.id)?
                .into_iter()
                .filter(|e| !e.deleted)
                .collect();
            folders.push(FolderStatus {
                label: f.label,
                path: f.root.display().to_string(),
                files: live.len(),
                bytes: live.iter().map(|e| e.size).sum(),
            });
        }
        Ok(Status {
            me: Me {
                name: net::device_name(&store)?,
                id: d.me.short(),
                port: d.port,
                addresses: net::local_ips().iter().map(ToString::to_string).collect(),
            },
            peers,
            folders,
        })
    }

    pub fn add_folder(&self, path: &Path, label: Option<&str>) -> Result<Folder> {
        let folder = add_folder(&self.d().store(), &self.home, path, label)?;
        // Picked up by the engine (index, Resend to peers) and the watcher.
        self.refresh();
        Ok(folder)
    }

    /// Stops syncing a folder; its files are left alone.
    pub fn remove_folder(&self, label: &str) -> Result<()> {
        let store = self.d().store();
        let Some(folder) = store.folder_by_label(label)? else {
            bail!("no synced folder called \"{label}\"");
        };
        store.remove_folder(&folder.root)?;
        drop(store);
        self.refresh();
        Ok(())
    }

    /// Removes a device from the allowlist; its live session is closed
    /// within a few seconds.
    pub fn unpair(&self, who: &str) -> Result<()> {
        let store = self.d().store();
        let peer = net::find_peer(&store, who)?;
        store.remove_peer(&peer.device_id)?;
        drop(store);
        self.d().emit(EventKind::StatusChanged);
        Ok(())
    }

    /// Renames this device. Devices pair with the new name from now on.
    pub fn rename(&self, name: &str) -> Result<String> {
        let name = net::set_device_name(&self.d().store(), name)?;
        self.d().emit(EventKind::StatusChanged);
        Ok(name)
    }

    /// Adds an address to dial, for networks where discovery is blocked.
    pub fn connect_addr(&self, addr: &str) {
        self.d().add_explicit(addr.trim().to_string());
    }

    /// Shows a pairing code. The outcome arrives as a `Paired` or
    /// `PairFailed` event. Starting again replaces the previous code.
    pub async fn pair_start(&self) -> Result<(String, u64)> {
        self.pair_cancel_quietly();
        let d = self.d().clone();
        let offer = pairing::offer(&d.identity, net::PAIR_PORT).await?;
        let code = offer.code.to_string();
        let task = tokio::spawn(async move {
            let name = net::device_name(&d.store()).unwrap_or_else(|_| d.name.clone());
            match offer.wait(&d.identity, &name).await {
                Ok(peer) => {
                    if let Err(e) = pairing::remember(&d.store(), &peer) {
                        warn!("could not save the paired device: {e}");
                    }
                    d.emit(EventKind::Paired {
                        name: peer.name.clone(),
                        id: peer.device_id.short(),
                        security_code: peer.security_code.to_string().to_uppercase(),
                    });
                    d.emit(EventKind::StatusChanged);
                }
                Err(e) => d.emit(EventKind::PairFailed {
                    reason: e.to_string(),
                }),
            }
        });
        *self.pairing.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
        Ok((code, CODE_LIFETIME.as_secs()))
    }

    pub fn pair_cancel(&self) {
        if self.pair_cancel_quietly() {
            self.d().emit(EventKind::PairFailed {
                reason: "pairing cancelled".into(),
            });
        }
    }

    fn pair_cancel_quietly(&self) -> bool {
        let task = self
            .pairing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match task {
            Some(t) if !t.is_finished() => {
                t.abort();
                true
            }
            _ => false,
        }
    }

    /// Enters a code shown on another device (found by discovery, or at
    /// `addr`).
    pub async fn pair_join(&self, code: &str, addr: Option<&str>) -> Result<Paired> {
        let d = self.d().clone();
        let name = net::device_name(&d.store())?;
        let peer = pairing::join(&d.identity, &name, code, addr).await?;
        pairing::remember(&d.store(), &peer)?;
        d.emit(EventKind::StatusChanged);
        Ok(Paired {
            name: peer.name,
            id: peer.device_id.short(),
            security_code: peer.security_code.to_string().to_uppercase(),
        })
    }

    /// The port this device listens on.
    pub fn port(&self) -> u16 {
        self.d().port
    }

    /// Waits until the node stops on its own (the sync engine failed).
    pub async fn stopped(&mut self) -> Result<()> {
        self.running.stopped().await
    }

    /// Asks the node to stop without waiting, for an app that is exiting
    /// and cannot give up its handle. Connections close and the engine
    /// finishes its current step.
    pub fn request_stop(&self) {
        self.pair_cancel_quietly();
        self.running.request_stop();
    }

    pub async fn shutdown(self) -> Result<()> {
        self.pair_cancel_quietly();
        self.running.shutdown().await
    }

    /// Makes the daemon notice folder changes now instead of on its next tick.
    fn refresh(&self) {
        self.d().refresh_now();
        self.d().nudge(Input::Tick);
        self.d().emit(EventKind::StatusChanged);
    }
}
