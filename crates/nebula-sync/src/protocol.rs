//! Messages exchanged over an encrypted session. Everything a peer sends is
//! decoded with bounds (postcard rejects trailing or malformed input) and
//! must still be validated by the receiver.

use nebula_core::Digest;
use nebula_core::manifest::FileEntry;
use serde::{Deserialize, Serialize};

/// Bumped whenever a message changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message from a daemon on every session. One-off clients such as
    /// `nebula ping` wait for the daemon's Hello and do not send their own,
    /// so they are never mistaken for a sync session.
    Hello {
        name: String,
        protocol: u32,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// A line of text for the other device to display. Used by
    /// `nebula ping --message` to show encryption working in a demo.
    Note {
        text: String,
    },
    /// Manifest entries for the folder labelled `folder`: everything on
    /// connect, then whatever changed. Large folders span several messages.
    Index {
        folder: String,
        entries: Vec<FileEntry>,
    },
    /// Asks for one chunk, by hash, from the folder labelled `folder`.
    Request {
        id: u64,
        folder: String,
        hash: Digest,
    },
    /// The bytes of a requested chunk. The requester verifies the hash.
    Chunk {
        id: u64,
        data: Vec<u8>,
    },
    /// The requested chunk is no longer available (the file changed).
    Missing {
        id: u64,
    },
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        // Serializing these types to a Vec cannot fail.
        postcard::to_stdvec(self).unwrap_or_default()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for m in [
            Message::Hello {
                name: "laptop".into(),
                protocol: PROTOCOL_VERSION,
            },
            Message::Ping { nonce: 7 },
            Message::Pong { nonce: u64::MAX },
            Message::Note {
                text: "hello".into(),
            },
        ] {
            assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(Message::decode(&[0xff, 0xff, 0xff]).is_err());
        assert!(Message::decode(&[]).is_err());
    }
}
