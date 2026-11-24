//! The thread that owns the sync engine. It is fed through a std channel
//! and routes the engine's outputs to the sessions' outbound channels.
//!
//! Bounding the input: only `Received` inputs arrive at a rate a peer
//! controls, so only they are limited, by a semaphore of [`MAX_RECEIVED`]
//! permits. A session reader awaits a permit before queueing a message, and
//! the engine thread drops it once the message is handled. A flooding peer
//! therefore stalls its own reader task (which stops reading its socket, so
//! TCP pushes back on the sender) without blocking a tokio worker or the
//! engine thread. The other inputs (`Connected`, `Disconnected`, ticks,
//! watcher events) stay unbounded and never block: they are few, and some
//! are sent while the session registry is locked, which the engine thread
//! also locks to route outputs, so a blocking send there could deadlock.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;
use nebula_core::DeviceId;
use nebula_sync::engine::{Engine, Event, Input};
use nebula_sync::protocol::Message;
use nebula_sync::store::Store;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, oneshot};
use tracing::{debug, warn};

use super::session::{Queued, Registry};
use crate::events::{EventKind, UiEvent};

/// At most this often per file, so a fast transfer doesn't flood the UI.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Peer messages queued for the engine at once, across all sessions.
const MAX_RECEIVED: usize = 4096;

pub enum Job {
    /// The permit, if any, is released once the input has been handled.
    Input(Input, Option<OwnedSemaphorePermit>),
    Stop,
}

/// Feeds the engine thread. See the module docs for what waits and what
/// does not. After the thread has stopped, inputs are dropped (the daemon is
/// shutting down by then).
#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Job>,
    received: Arc<Semaphore>,
}

impl EngineHandle {
    fn new(tx: mpsc::Sender<Job>) -> Self {
        Self {
            tx,
            received: Arc::new(Semaphore::new(MAX_RECEIVED)),
        }
    }

    /// Queues a local or session event. Never blocks.
    pub fn send(&self, input: Input) {
        let _ = self.tx.send(Job::Input(input, None));
    }

    /// Queues a message from a peer, waiting while [`MAX_RECEIVED`] are
    /// already queued.
    pub async fn received(&self, peer: DeviceId, msg: Message) {
        // The semaphore is never closed, so this cannot fail in practice.
        let Ok(permit) = self.received.clone().acquire_owned().await else {
            return;
        };
        let input = Input::Received { peer, msg };
        let _ = self.tx.send(Job::Input(input, Some(permit)));
    }

    /// Asks the thread to stop after the inputs already queued.
    pub fn stop(&self) {
        let _ = self.tx.send(Job::Stop);
    }

    /// A handle whose jobs arrive on the returned receiver instead of an engine.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::channel();
        (Self::new(tx), rx)
    }
}

/// Why the engine thread ended: `Ok` when asked to stop, otherwise a reason.
pub type Stopped = std::result::Result<(), String>;

pub fn spawn(
    store: Store,
    me: DeviceId,
    name: String,
    sessions: Registry,
    events: broadcast::Sender<UiEvent>,
) -> Result<(EngineHandle, oneshot::Receiver<Stopped>)> {
    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("sync-engine".into())
        .spawn(move || {
            let res = catch_unwind(AssertUnwindSafe(|| {
                drive(store, me, name, &rx, &sessions, &events)
            }))
            .unwrap_or_else(|panic| Err(format!("it crashed: {}", panic_message(&*panic))));
            let _ = done_tx.send(res);
        })?;
    Ok((EngineHandle::new(tx), done_rx))
}

fn drive(
    store: Store,
    me: DeviceId,
    name: String,
    rx: &mpsc::Receiver<Job>,
    sessions: &Registry,
    events: &broadcast::Sender<UiEvent>,
) -> Stopped {
    let mut engine = Engine::new(store, me, name).map_err(|e| format!("could not start: {e}"))?;
    let mut inputs = VecDeque::new();
    let mut last_progress = HashMap::new();
    while let Ok(Job::Input(input, permit)) = rx.recv() {
        inputs.push_back(input);
        while let Some(input) = inputs.pop_front() {
            for out in engine.handle(input) {
                match sessions.send(&out.peer, out.msg) {
                    Queued::Yes => {}
                    // The engine resends a full index on the next Connected.
                    Queued::NotConnected => debug!(
                        "{} is not connected; dropped a message for it",
                        out.peer.short()
                    ),
                    // Waiting for room would stall sync with everyone else,
                    // and buffering more would let the peer exhaust memory.
                    // The registry has already closed the session; the
                    // engine is told here because this thread drains its
                    // own input channel.
                    Queued::Full => {
                        warn!("{} is not reading; closing the session", out.peer.short());
                        inputs.push_back(Input::Disconnected { peer: out.peer });
                    }
                }
            }
            publish(&mut engine, events, &mut last_progress);
        }
        drop(permit);
    }
    Ok(())
}

/// Forwards the engine's events to whoever is listening (the app). Sending
/// never blocks; with no listener the events are simply dropped.
fn publish(
    engine: &mut Engine,
    events: &broadcast::Sender<UiEvent>,
    last_progress: &mut HashMap<(String, String), Instant>,
) {
    let mut changed = false;
    for event in engine.take_events() {
        let kind = match event {
            Event::LocalChange {
                folder,
                added,
                modified,
                deleted,
            } => {
                changed = true;
                EventKind::LocalChange {
                    folder,
                    added,
                    modified,
                    deleted,
                }
            }
            Event::Progress {
                folder,
                path,
                from,
                done,
                total,
            } => {
                let key = (folder.clone(), path.to_string());
                let now = Instant::now();
                if last_progress
                    .get(&key)
                    .is_some_and(|t| now.duration_since(*t) < PROGRESS_EVERY)
                {
                    continue;
                }
                last_progress.insert(key, now);
                EventKind::DownloadProgress {
                    folder,
                    path: path.to_string(),
                    done,
                    total,
                    from: engine.peer_name(&from),
                }
            }
            Event::FileReceived {
                folder,
                path,
                from,
                size,
                transferred,
            } => {
                changed = true;
                last_progress.remove(&(folder.clone(), path.to_string()));
                EventKind::FileReceived {
                    folder,
                    path: path.to_string(),
                    from: engine.peer_name(&from),
                    size,
                    transferred,
                }
            }
            Event::FileDeleted { folder, path, from } => {
                changed = true;
                EventKind::FileDeleted {
                    folder,
                    path: path.to_string(),
                    from: engine.peer_name(&from),
                }
            }
            Event::Conflict { folder, path, copy } => {
                changed = true;
                EventKind::Conflict {
                    folder,
                    path: path.to_string(),
                    copy: copy.to_string(),
                }
            }
        };
        let _ = events.send(UiEvent::now(kind));
    }
    if changed {
        let _ = events.send(UiEvent::now(EventKind::StatusChanged));
    }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn received_waits_for_room() {
        let (engine, jobs) = EngineHandle::for_test();
        let peer = DeviceId([7; 32]);
        for nonce in 0..MAX_RECEIVED as u64 {
            engine.received(peer, Message::Ping { nonce }).await;
        }
        let blocked = engine.received(peer, Message::Ping { nonce: 0 });
        tokio::pin!(blocked);
        let wait = Duration::from_millis(50);
        assert!(tokio::time::timeout(wait, &mut blocked).await.is_err());
        // Handling one (dropping its permit) lets the next one in.
        drop(jobs.recv().unwrap());
        assert!(tokio::time::timeout(wait, &mut blocked).await.is_ok());
        // Local events never wait.
        engine.send(Input::Tick);
    }
}
