//! Content-defined chunking (LBFS-style, see ADR 0002).
//!
//! A rolling hash over the last [`WINDOW`] bytes picks chunk boundaries from
//! the content itself, so an insertion only changes the chunks around it
//! instead of shifting every block after it.

use std::io::{self, ErrorKind, Read};

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

/// Rolling-hash window in bytes (LBFS used 48).
const WINDOW: usize = 48;

/// Multiplier of the polynomial hash. Any odd constant works; this one is the
/// PCG multiplier.
const BASE: u64 = 0x5851_F42D_4C95_7F2D;

/// `BASE^WINDOW` (wrapping), the weight of the byte leaving the window.
const BASE_POW_W: u64 = {
    let mut p: u64 = 1;
    let mut i = 0;
    while i < WINDOW {
        p = p.wrapping_mul(BASE);
        i += 1;
    }
    p
};

/// Maps each byte value to a pseudo-random 64-bit word (splitmix64 from a
/// fixed seed), so runs of similar bytes still produce well-mixed hashes.
const TABLE: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x6E65_6275_6C61_0001; // "nebula" + 1
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
};

/// LBFS-style chunker using a Rabin–Karp polynomial rolling hash.
///
/// A boundary falls after byte `i` when the high 32 bits of the hash of the
/// window ending at `i` are below `u32::MAX / (avg - min)`, and the chunk is
/// at least `min` bytes. Chunks are cut at `max` if no boundary appears, so
/// the expected size is about `min + (avg - min) = avg`.
#[derive(Debug, Clone)]
pub struct RollingChunker {
    min: usize,
    max: usize,
    threshold: u32,
}

impl RollingChunker {
    pub const DEFAULT_MIN: usize = 2 * 1024;
    pub const DEFAULT_AVG: usize = 8 * 1024;
    pub const DEFAULT_MAX: usize = 64 * 1024;

    /// Precondition: `0 < min < avg < max <= u32::MAX`, checked in debug
    /// builds. In release builds invalid input is clamped deterministically
    /// (`max` into `1..=u32::MAX`, `min` to at most `max`, `avg - min` to at
    /// least 1) so the chunker still terminates and respects `max`.
    ///
    /// These parameters are part of the protocol: peers must agree on them.
    pub fn new(min: usize, avg: usize, max: usize) -> Self {
        debug_assert!(
            0 < min && min < avg && avg < max,
            "need 0 < min < avg < max"
        );
        debug_assert!(max <= u32::MAX as usize, "max must fit in u32");
        let max = max.clamp(1, u32::MAX as usize);
        let min = min.min(max);
        let span = avg.saturating_sub(min).max(1) as u64;
        let threshold = (u64::from(u32::MAX) / span) as u32;
        Self {
            min,
            max,
            threshold,
        }
    }
}

impl Default for RollingChunker {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MIN, Self::DEFAULT_AVG, Self::DEFAULT_MAX)
    }
}

impl Chunker for RollingChunker {
    fn max_size(&self) -> usize {
        self.max
    }

    fn cut(&self, buf: &[u8]) -> usize {
        let end = buf.len().min(self.max);
        if end <= self.min {
            return end;
        }
        // Bytes before this cannot be in the window of any legal boundary.
        let start = self.min.saturating_sub(WINDOW);
        let mut h: u64 = 0;
        for i in start..end {
            h = h
                .wrapping_mul(BASE)
                .wrapping_add(TABLE[usize::from(buf[i])]);
            if i >= start + WINDOW {
                let out = TABLE[usize::from(buf[i - WINDOW])];
                h = h.wrapping_sub(out.wrapping_mul(BASE_POW_W));
            }
            if i + 1 >= self.min && ((h >> 32) as u32) < self.threshold {
                return i + 1;
            }
        }
        end
    }
}

/// Streams chunks out of a reader with memory bounded by the chunker's max size.
///
/// Holds one buffer of `2 * max_size()` bytes.
pub struct ChunkReader<R, C> {
    reader: R,
    chunker: C,
    buf: Vec<u8>,
    /// Unconsumed bytes are `buf[start..end]`.
    start: usize,
    end: usize,
    eof: bool,
    hasher: blake3::Hasher,
    size: u64,
}

impl<R: Read, C: Chunker> ChunkReader<R, C> {
    pub fn new(reader: R, chunker: C) -> Self {
        let cap = chunker.max_size().max(1).saturating_mul(2);
        Self {
            reader,
            chunker,
            buf: vec![0; cap],
            start: 0,
            end: 0,
            eof: false,
            hasher: blake3::Hasher::new(),
            size: 0,
        }
    }

    /// Ensures at least `max_size()` unconsumed bytes are buffered, or EOF.
    fn fill(&mut self) -> io::Result<()> {
        let want = self.chunker.max_size().max(1);
        if self.eof || self.end - self.start >= want {
            return Ok(());
        }
        self.buf.copy_within(self.start..self.end, 0);
        self.end -= self.start;
        self.start = 0;
        while self.end < want {
            match self.reader.read(&mut self.buf[self.end..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => self.end += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The next chunk and its bytes, or `None` at end of input.
    pub fn next_chunk(&mut self) -> io::Result<Option<(ChunkRef, &[u8])>> {
        self.fill()?;
        let avail = &self.buf[self.start..self.end];
        if avail.is_empty() {
            return Ok(None);
        }
        let n = self.chunker.cut(avail);
        let len = match u32::try_from(n) {
            Ok(len) if (1..=avail.len()).contains(&n) => len,
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "chunker returned invalid length {n} for {} bytes",
                        avail.len()
                    ),
                ));
            }
        };
        let chunk = &self.buf[self.start..self.start + n];
        self.start += n;
        self.hasher.update(chunk);
        self.size += u64::from(len);
        let r = ChunkRef {
            hash: Digest::of(chunk),
            len,
        };
        Ok(Some((r, chunk)))
    }

    /// Total bytes read and the BLAKE3 hash of the whole stream.
    pub fn finish(self) -> (u64, Digest) {
        (self.size, self.hasher.finalize().into())
    }
}

/// Every chunk of a stream, plus its total size and whole-content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunked {
    pub chunks: Vec<ChunkRef>,
    pub size: u64,
    pub content_hash: Digest,
}

pub fn chunk_all<C: Chunker>(reader: impl Read, chunker: C) -> io::Result<Chunked> {
    let mut cr = ChunkReader::new(reader, chunker);
    let mut chunks = Vec::new();
    while let Some((r, _)) = cr.next_chunk()? {
        chunks.push(r);
    }
    let (size, content_hash) = cr.finish();
    Ok(Chunked {
        chunks,
        size,
        content_hash,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Cursor;

    use proptest::prelude::*;

    use super::*;

    /// Deterministic xorshift64 bytes.
    fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn window_hash(window: &[u8]) -> u64 {
        window.iter().fold(0u64, |h, &b| {
            h.wrapping_mul(BASE).wrapping_add(TABLE[usize::from(b)])
        })
    }

    /// Chunks via `ChunkReader`, returning refs and the bytes of each chunk.
    fn chunk_bytes(reader: impl Read, chunker: RollingChunker) -> (Vec<ChunkRef>, Vec<Vec<u8>>) {
        let mut cr = ChunkReader::new(reader, chunker);
        let (mut refs, mut bytes) = (Vec::new(), Vec::new());
        while let Some((r, b)) = cr.next_chunk().unwrap() {
            refs.push(r);
            bytes.push(b.to_vec());
        }
        (refs, bytes)
    }

    fn hashes(data: &[u8]) -> Vec<Digest> {
        chunk_all(data, RollingChunker::default())
            .unwrap()
            .chunks
            .iter()
            .map(|c| c.hash)
            .collect()
    }

    fn assert_bounds(chunks: &[ChunkRef], min: usize, max: usize) {
        for (i, c) in chunks.iter().enumerate() {
            let len = c.len as usize;
            assert!(len <= max, "chunk {i} has len {len} > max");
            if i + 1 < chunks.len() {
                assert!(len >= min, "chunk {i} has len {len} < min");
            }
        }
    }

    /// Returns at most `sizes[k]` bytes per read (cycling), and reports
    /// `Interrupted` before every read whose size is 0.
    struct Trickle<'a> {
        data: &'a [u8],
        sizes: Vec<usize>,
        k: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let want = self.sizes[self.k % self.sizes.len()];
            self.k += 1;
            if want == 0 {
                return Err(io::Error::from(ErrorKind::Interrupted));
            }
            let n = want.min(out.len()).min(self.data.len());
            out[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn rolling_hash_matches_window_hash() {
        let data = random_bytes(4096, 7);
        let mut h: u64 = 0;
        for i in 0..data.len() {
            h = h
                .wrapping_mul(BASE)
                .wrapping_add(TABLE[usize::from(data[i])]);
            if i >= WINDOW {
                h = h.wrapping_sub(TABLE[usize::from(data[i - WINDOW])].wrapping_mul(BASE_POW_W));
            }
            let lo = (i + 1).saturating_sub(WINDOW);
            assert_eq!(h, window_hash(&data[lo..=i]), "at {i}");
        }
    }

    #[test]
    fn empty_input() {
        let c = chunk_all(&[][..], RollingChunker::default()).unwrap();
        assert!(c.chunks.is_empty());
        assert_eq!(c.size, 0);
        assert_eq!(c.content_hash, Digest::of(&[]));
    }

    #[test]
    fn deterministic() {
        let data = random_bytes(1 << 20, 1);
        assert_eq!(hashes(&data), hashes(&data));
    }

    #[test]
    fn reassembles_and_respects_bounds() {
        let data = random_bytes(1 << 20, 2);
        let (refs, bytes) = chunk_bytes(&data[..], RollingChunker::default());
        assert_eq!(bytes.concat(), data);
        for (r, b) in refs.iter().zip(&bytes) {
            assert_eq!(r.len as usize, b.len());
            assert_eq!(r.hash, Digest::of(b));
        }
        assert_bounds(
            &refs,
            RollingChunker::DEFAULT_MIN,
            RollingChunker::DEFAULT_MAX,
        );

        let c = chunk_all(&data[..], RollingChunker::default()).unwrap();
        assert_eq!(c.chunks, refs);
        assert_eq!(c.size, data.len() as u64);
        assert_eq!(
            c.chunks.iter().map(|r| u64::from(r.len)).sum::<u64>(),
            c.size
        );
        assert_eq!(c.content_hash, Digest::of(&data));
    }

    #[test]
    fn low_entropy_input_respects_bounds() {
        let data = vec![0u8; 1 << 20];
        let c = chunk_all(&data[..], RollingChunker::default()).unwrap();
        eprintln!("zeros: {} chunks for 1 MiB", c.chunks.len());
        assert_bounds(
            &c.chunks,
            RollingChunker::DEFAULT_MIN,
            RollingChunker::DEFAULT_MAX,
        );
        assert_eq!(c.size, data.len() as u64);
    }

    #[test]
    fn average_size_near_target() {
        let data = random_bytes(8 << 20, 3);
        let c = chunk_all(&data[..], RollingChunker::default()).unwrap();
        let avg = data.len() / c.chunks.len();
        eprintln!(
            "average chunk size: {avg} bytes over {} chunks",
            c.chunks.len()
        );
        assert!((6 * 1024..=11 * 1024).contains(&avg), "avg {avg}");
    }

    #[test]
    fn one_byte_edit_changes_few_chunks() {
        let data = random_bytes(4 << 20, 4);
        let mut edited = data.clone();
        edited[data.len() / 2] ^= 0xff;
        let old: HashSet<_> = hashes(&data).into_iter().collect();
        let new = hashes(&edited);
        let changed = new.iter().filter(|h| !old.contains(h)).count();
        eprintln!("flip: {changed} of {} chunks changed", new.len());
        assert!(changed <= 3, "{changed} chunks changed");
    }

    #[test]
    fn insertion_at_start_keeps_most_chunks() {
        let data = random_bytes(4 << 20, 5);
        let mut shifted = vec![0x42];
        shifted.extend_from_slice(&data);
        let old = hashes(&data);
        let new: HashSet<_> = hashes(&shifted).into_iter().collect();
        let kept = old.iter().filter(|h| new.contains(h)).count();
        eprintln!("insert: {kept} of {} original chunks kept", old.len());
        assert!(
            kept * 100 > old.len() * 95,
            "only {kept} of {} kept",
            old.len()
        );
    }

    #[test]
    fn rejects_invalid_cut() {
        struct Zero;
        impl Chunker for Zero {
            fn max_size(&self) -> usize {
                16
            }
            fn cut(&self, _: &[u8]) -> usize {
                0
            }
        }
        let err = chunk_all(&b"abc"[..], Zero).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn streaming_matches_whole_read(
            data in proptest::collection::vec(any::<u8>(), 0..8192),
            sizes in proptest::collection::vec(0usize..300, 1..16),
        ) {
            prop_assume!(sizes.iter().any(|&s| s > 0));
            let chunker = RollingChunker::new(64, 256, 1024);
            let (whole, _) = chunk_bytes(Cursor::new(&data), chunker.clone());
            let (trickled, bytes) =
                chunk_bytes(Trickle { data: &data, sizes, k: 0 }, chunker);
            prop_assert_eq!(&trickled, &whole);
            prop_assert_eq!(bytes.concat(), data);
            assert_bounds(&whole, 64, 1024);
        }

        #[test]
        fn cut_is_in_range(data in proptest::collection::vec(any::<u8>(), 1..4096)) {
            let chunker = RollingChunker::new(64, 256, 1024);
            let n = chunker.cut(&data);
            prop_assert!(n >= 1 && n <= data.len().min(1024));
            prop_assert!(n >= 64 || n == data.len());
        }
    }
}
