# ADR 0002: LBFS-style content-defined chunking with BLAKE3 chunk IDs

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone / PR:** M1 / #2

## Context

Peers sync by exchanging per-file chunk lists and requesting only the chunks
they lack. With fixed-size blocks, inserting one byte near the start of a file
shifts every block after it, so every block hash changes and the whole file is
sent again. Chunking has to stream 1 GB+ files with flat memory.

## Decision

Implement our own content-defined chunker (`RollingChunker`, behind the
`Chunker` trait):

- A Rabin–Karp polynomial rolling hash mod 2^64 over a 48-byte window. Each byte is first mapped through a fixed 256-entry table of splitmix64 values, so runs of identical bytes still mix.
- A boundary falls after a byte when the high 32 bits of the window hash are below `2^32 / (avg - min)` and the chunk is at least `min` bytes. Chunks are forced at `max`. Expected size is `min + (avg - min) = avg`.
- Parameters: min 2 KiB, average 8 KiB, max 64 KiB.
- Chunks and whole files are identified by BLAKE3, not SHA-1 as in LBFS.
- `ChunkReader` holds one buffer of 2 × max (128 KiB) per stream, whatever the file size.

## Alternatives considered

- **Fixed-size blocks**: simplest, but one insertion invalidates every later block.
- **True Rabin fingerprints over GF(2)**, as LBFS used: same rolling property, but needs polynomial arithmetic tables and is harder to explain and verify. The polynomial hash is a few lines and has a direct test against a from-scratch hash.
- **FastCDC (gear hash) via the `fastcdc` crate**: faster, but the brief asks us to implement the chunker ourselves. It may be benchmarked against ours for the paper.

## Source

Muthitacharoen, Chen and Mazières (2001), "A Low-Bandwidth Network File
System", SOSP '01 (brief source 3). We keep its 48-byte window, 2 KiB / 8 KiB
/ 64 KiB bounds and boundary-on-hash-match idea. We deviate in the hash
(polynomial instead of Rabin fingerprint) and the chunk ID (BLAKE3 instead of
SHA-1, which is faster and is not broken for collisions).

## Consequences

Edits only change nearby chunks. On 4 MiB of random data, flipping one byte
changed 1 of 492 chunks, and inserting one byte at the start kept 491 of 492.
The measured average on 8 MiB of random data is 8380 bytes. All-zero input
has no boundaries and is cut into 64 KiB pieces. The chunk parameters, hash
table and seed are part of the protocol: both peers must agree, and changing
any of them changes every manifest.
