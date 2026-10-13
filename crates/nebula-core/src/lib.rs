//! Chunking, hashing, manifest types, version vectors and conflict logic.
//! No network or IO policy lives here, so everything is pure and testable.

pub mod chunker;
pub mod conflict;
pub mod manifest;
pub mod path;
pub mod version;

use std::fmt;

use serde::{Deserialize, Serialize};

/// A BLAKE3 digest. Identifies chunks and whole-file contents.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// First 8 hex characters, for human-facing output.
    pub fn short(&self) -> String {
        hex(&self.0[..4])
    }
}

impl From<blake3::Hash> for Digest {
    fn from(h: blake3::Hash) -> Self {
        Self(*h.as_bytes())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}

/// A device's identity: the BLAKE3 hash of its static public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 32]);

impl DeviceId {
    pub fn from_public_key(key: &[u8]) -> Self {
        Self(*blake3::hash(key).as_bytes())
    }

    /// First 8 hex characters, for human-facing output.
    pub fn short(&self) -> String {
        hex(&self.0[..4])
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.short())
    }
}

/// Lowercase hex encoding.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn device_id_is_key_hash() {
        let id = DeviceId::from_public_key(b"key");
        assert_eq!(id.0, *blake3::hash(b"key").as_bytes());
        assert_eq!(id.short().len(), 8);
    }
}
