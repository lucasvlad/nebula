//! The Nebula desktop app. It runs the same node as `nebula run` inside the
//! app and shows it in a window. The page in `ui/` is bundled into the app
//! and talks to the node only through the commands below and the
//! `nebula://event` stream (see `docs/ui-contract.md`). There is no web
//! server: the only network traffic is encrypted sync with paired devices.
//!
//! `NEBULA_HOME`, `NEBULA_PORT` and `NEBULA_CONNECT` (comma-separated
//! `host:port`s) work as in the CLI, e.g. to run two copies on one machine.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nebula_node::events::{EventKind, Paired, Status, UiEvent};
use nebula_node::net::RUN_PORT;
use nebula_node::{Node, NodeConfig};
use serde::Serialize;
use tauri::{Emitter, Manager, RunEvent, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tokio::sync::broadcast::error::RecvError;

type Shared = Arc<Node>;

/// Errors are shown to the user as they are, so start them with a capital.
fn user_error(e: anyhow::Error) -> String {
    let msg = e.to_string();
    let mut chars = msg.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Something went wrong".to_string(),
    }
}

#[derive(Serialize)]
struct PairStart {
    code: String,
    expires_in_secs: u64,
}

#[tauri::command]
fn status(node: State<'_, Shared>) -> Result<Status, String> {
    node.status().map_err(user_error)
}

#[tauri::command]
async fn pair_start(node: State<'_, Shared>) -> Result<PairStart, String> {
    let (code, expires_in_secs) = node.pair_start().await.map_err(user_error)?;
    Ok(PairStart {
        code,
        expires_in_secs,
    })
}

#[tauri::command]
fn pair_cancel(node: State<'_, Shared>) {
    node.pair_cancel();
}

#[tauri::command]
async fn pair_join(
    node: State<'_, Shared>,
    code: String,
    addr: Option<String>,
) -> Result<Paired, String> {
    let addr = addr.filter(|a| !a.trim().is_empty());
    node.pair_join(&code, addr.as_deref())
        .await
        .map_err(user_error)
}

#[tauri::command]
fn unpair(node: State<'_, Shared>, id: String) -> Result<(), String> {
    node.unpair(&id).map_err(user_error)
}

#[tauri::command]
fn add_folder(node: State<'_, Shared>, path: String, label: String) -> Result<(), String> {
    node.add_folder(Path::new(&path), Some(&label))
        .map(|_| ())
        .map_err(user_error)
}

#[tauri::command]
fn remove_folder(node: State<'_, Shared>, label: String) -> Result<(), String> {
    node.remove_folder(&label).map_err(user_error)
}

#[tauri::command]
fn rename_device(node: State<'_, Shared>, name: String) -> Result<(), String> {
    node.rename(&name).map(|_| ()).map_err(user_error)
}

#[tauri::command]
fn connect_addr(node: State<'_, Shared>, addr: String) {
    node.connect_addr(&addr);
}

fn config() -> anyhow::Result<NodeConfig> {
    let home = match std::env::var_os("NEBULA_HOME") {
        Some(h) => PathBuf::from(h),
        None => nebula_node::default_home()?,
    };
    let port = std::env::var("NEBULA_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(RUN_PORT);
    let connect = std::env::var("NEBULA_CONNECT")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Ok(NodeConfig {
        home,
        port,
        connect,
    })
}

/// Forwards node events to the window. If the window fell behind and
/// events were dropped, it is told to refresh its status instead.
fn forward_events(node: &Node, app: tauri::AppHandle) {
    let mut events = node.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => UiEvent::now(EventKind::StatusChanged),
                Err(RecvError::Closed) => break,
            };
            let _ = app.emit("nebula://event", &event);
        }
    });
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let started = config().and_then(|c| tauri::async_runtime::block_on(Node::start(c)));
            let node = match started {
                Ok(node) => Arc::new(node),
                Err(e) => {
                    app.dialog()
                        .message(format!(
                            "{}\n\nIf `nebula run` is open in a terminal, stop it first.",
                            user_error(e)
                        ))
                        .title("Nebula could not start")
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                    std::process::exit(1);
                }
            };
            forward_events(&node, app.handle().clone());
            app.manage(node);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            status,
            pair_start,
            pair_cancel,
            pair_join,
            unpair,
            add_folder,
            remove_folder,
            rename_device,
            connect_addr
        ])
        .build(tauri::generate_context!());

    let app = match app {
        Ok(app) => app,
        Err(e) => {
            eprintln!("Nebula could not start: {e}");
            std::process::exit(1);
        }
    };
    app.run(|handle, event| {
        if let RunEvent::Exit = event
            && let Some(node) = handle.try_state::<Shared>()
        {
            // Close connections and let the engine finish its current step.
            node.request_stop();
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}
