//! Content-defined chunking. STUB: API only.

use std::io::{self, Read};

use serde::{Deserialize, Serialize};

use crate::Digest;

/// One chunk of a file: its BLAKE3 hash and length in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Digest,
    pub len: u32,
}

/// Splits a byte stream into chunks.
pub trait Chunker {
    /// Largest chunk this chunker ever produces.
    fn max_size(&self) -> usize;

    /// `buf` starts at a chunk boundary and holds at least `max_size()` bytes
    /// unless it is the end of the input. Returns the length of the first
    /// chunk: `1..=buf.len()` for non-empty `buf`.
    fn cut(&self, buf: &[u8]) -> usize;
}

/// LBFS-style chunker using a rolling hash.
#[derive(Debug, Clone)]
pub struct RollingChunker {
    _priv: (),
}

impl RollingChunker {
    pub const DEFAULT_MIN: usize = 2 * 1024;
    pub const DEFAULT_AVG: usize = 8 * 1024;
    pub const DEFAULT_MAX: usize = 64 * 1024;

    pub fn new(_min: usize, _avg: usize, _max: usize) -> Self {
        todo!()
    }
}

impl Default for RollingChunker {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MIN, Self::DEFAULT_AVG, Self::DEFAULT_MAX)
    }
}

impl Chunker for RollingChunker {
    fn max_size(&self) -> usize {
        todo!()
    }
    fn cut(&self, _buf: &[u8]) -> usize {
        todo!()
    }
}

/// Streams chunks out of a reader with memory bounded by the chunker's max size.
pub struct ChunkReader<R, C> {
    _r: R,
    _c: C,
}

impl<R: Read, C: Chunker> ChunkReader<R, C> {
    pub fn new(_reader: R, _chunker: C) -> Self {
        todo!()
    }

    /// The next chunk and its bytes, or `None` at end of input.
    pub fn next_chunk(&mut self) -> io::Result<Option<(ChunkRef, &[u8])>> {
        todo!()
    }

    /// Total bytes read and the BLAKE3 hash of the whole stream.
    pub fn finish(self) -> (u64, Digest) {
        todo!()
    }
}

/// Every chunk of a stream, plus its total size and whole-content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunked {
    pub chunks: Vec<ChunkRef>,
    pub size: u64,
    pub content_hash: Digest,
}

pub fn chunk_all<C: Chunker>(_reader: impl Read, _chunker: C) -> io::Result<Chunked> {
    todo!()
}
