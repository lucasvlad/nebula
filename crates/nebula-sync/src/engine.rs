//! The sync engine: pure decision logic, driven by inputs and producing
//! messages to send. It owns the state database and does file IO inside its
//! synced folders, but knows nothing about sockets, threads or timers, so
//! two engines can be wired together in a test and checked for convergence.
//!
//! STUB: API only. Implementation in progress.

use nebula_core::DeviceId;

use crate::protocol::Message;
use crate::store::{Folder, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Something that happened, fed to [`Engine::handle`].
#[derive(Debug)]
pub enum Input {
    /// An authenticated session with a paired peer is up.
    Connected { peer: DeviceId },
    /// That session ended. Downloads from it pause until it reconnects.
    Disconnected { peer: DeviceId },
    /// A message arrived from a connected peer.
    Received { peer: DeviceId, msg: Message },
    /// Something changed on disk under this folder (from the watcher,
    /// already debounced). The engine rescans it.
    LocalChange { folder: i64 },
    /// Periodic fallback: rescan everything and retry stalled downloads.
    Tick,
}

/// A message the caller must send to `peer`.
#[derive(Debug, PartialEq, Eq)]
pub struct Output {
    pub peer: DeviceId,
    pub msg: Message,
}

pub struct Engine {
    _store: Store,
}

impl Engine {
    /// `name` is this device's name, used to label conflict copies.
    pub fn new(store: Store, me: DeviceId, name: String) -> Result<Self, EngineError> {
        let _ = (me, name);
        Ok(Self { _store: store })
    }

    /// Folders being synced, for setting up the watcher.
    pub fn folders(&self) -> Result<Vec<Folder>, EngineError> {
        todo!()
    }

    /// Processes one input and returns the messages to send, in order.
    /// Errors are logged and do not stop the engine.
    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        let _ = input;
        todo!()
    }
}
