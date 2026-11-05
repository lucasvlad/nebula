//! The thread that owns the sync engine. It is fed through a std channel
//! and routes the engine's outputs to the sessions' outbound channels.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;

use anyhow::Result;
use nebula_core::DeviceId;
use nebula_sync::engine::{Engine, Input};
use nebula_sync::store::Store;
use tokio::sync::oneshot;
use tracing::debug;

use super::session::Registry;

pub enum Job {
    Input(Input),
    Stop,
}

/// Feeds the engine thread. Sending never blocks; after the thread has
/// stopped, inputs are dropped (the daemon is shutting down by then).
#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Job>,
}

impl EngineHandle {
    pub fn send(&self, input: Input) {
        let _ = self.tx.send(Job::Input(input));
    }

    /// Asks the thread to stop after the inputs already queued.
    pub fn stop(&self) {
        let _ = self.tx.send(Job::Stop);
    }

    /// A handle whose jobs arrive on the returned receiver instead of an engine.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, rx)
    }
}

/// Why the engine thread ended: `Ok` when asked to stop, otherwise a reason.
pub type Stopped = std::result::Result<(), String>;

pub fn spawn(
    store: Store,
    me: DeviceId,
    name: String,
    sessions: Registry,
) -> Result<(EngineHandle, oneshot::Receiver<Stopped>)> {
    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("sync-engine".into())
        .spawn(move || {
            let res = catch_unwind(AssertUnwindSafe(|| drive(store, me, name, &rx, &sessions)))
                .unwrap_or_else(|panic| Err(format!("it crashed: {}", panic_message(&*panic))));
            let _ = done_tx.send(res);
        })?;
    Ok((EngineHandle { tx }, done_rx))
}

fn drive(
    store: Store,
    me: DeviceId,
    name: String,
    rx: &mpsc::Receiver<Job>,
    sessions: &Registry,
) -> Stopped {
    let mut engine = Engine::new(store, me, name).map_err(|e| format!("could not start: {e}"))?;
    while let Ok(Job::Input(input)) = rx.recv() {
        for out in engine.handle(input) {
            if !sessions.send(&out.peer, out.msg) {
                // The engine resends a full index on the next Connected.
                debug!(
                    "{} is not connected; dropped a message for it",
                    out.peer.short()
                );
            }
        }
    }
    Ok(())
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
