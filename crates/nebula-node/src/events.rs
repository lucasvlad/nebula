//! What the node reports to a user interface. The shapes match
//! `docs/ui-contract.md`; serialized as JSON for the app.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// One activity-feed event. Serialized as `{ "kind": "...", "at": ms, ... }`.
#[derive(Debug, Clone, Serialize)]
pub struct UiEvent {
    /// Unix milliseconds.
    pub at: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    PeerOnline {
        name: String,
        id: String,
        address: String,
        security_code: String,
    },
    PeerOffline {
        name: String,
        id: String,
    },
    Paired {
        name: String,
        id: String,
        security_code: String,
    },
    PairFailed {
        reason: String,
    },
    LocalChange {
        folder: String,
        added: usize,
        modified: usize,
        deleted: usize,
    },
    DownloadProgress {
        folder: String,
        path: String,
        done: u64,
        total: u64,
        from: String,
    },
    FileReceived {
        folder: String,
        path: String,
        from: String,
        size: u64,
        transferred: u64,
    },
    FileDeleted {
        folder: String,
        path: String,
        from: String,
    },
    Conflict {
        folder: String,
        path: String,
        copy: String,
    },
    Refused {
        address: String,
        id: String,
    },
    StatusChanged,
}

impl UiEvent {
    pub fn now(kind: EventKind) -> Self {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self { at, kind }
    }
}

/// A snapshot for the app's panels.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub me: Me,
    pub peers: Vec<PeerStatus>,
    pub folders: Vec<FolderStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Me {
    pub name: String,
    pub id: String,
    pub port: u16,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerStatus {
    pub name: String,
    pub id: String,
    pub online: bool,
    pub address: Option<String>,
    pub security_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderStatus {
    pub label: String,
    pub path: String,
    pub files: usize,
    pub bytes: u64,
}

/// The result of joining a pairing.
#[derive(Debug, Clone, Serialize)]
pub struct Paired {
    pub name: String,
    pub id: String,
    pub security_code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_flat_with_a_kind() {
        let e = UiEvent {
            at: 5,
            kind: EventKind::FileReceived {
                folder: "homework".into(),
                path: "a.txt".into(),
                from: "Laptop".into(),
                size: 10,
                transferred: 2,
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"at":5,"kind":"file_received","folder":"homework","path":"a.txt","from":"Laptop","size":10,"transferred":2}"#
        );
        let json = serde_json::to_string(&UiEvent::now(EventKind::StatusChanged)).unwrap();
        assert!(json.contains(r#""kind":"status_changed""#));
    }
}
