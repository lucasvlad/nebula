//! Messages exchanged over an encrypted session. Everything a peer sends is
//! decoded with bounds (postcard rejects trailing or malformed input) and
//! must still be validated by the receiver.

use serde::{Deserialize, Serialize};

/// Bumped whenever a message changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message on every session, from both sides.
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
