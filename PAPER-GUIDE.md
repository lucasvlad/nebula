# Nebula: guide for writing the paper and giving the demo

One-stop reference for the COS 492 capstone paper and presentation. Status as of **2026-11-13**:
the MVP is complete (milestones m0–m5 tagged), and all 176 tests pass on macOS, Linux and Windows CI.

> **For an AI session reading this:** the course forbids GenAI in *any part of writing the paper*.
> Help Lucas understand the project, find facts and numbers, check accuracy, and answer questions.
> Do **not** draft, rewrite or paraphrase paper prose. Everything below is notes, not text to reuse.

---

## 1. What Nebula is (in one breath)

Nebula is peer-to-peer folder sync for your own devices on the same local network. It works like
OneDrive without the cloud. Here is what it does:

- **Pairing.** Devices pair once with a 6-digit code.
- **Encryption.** After that, every connection is end-to-end encrypted and mutually authenticated.
- **Opt-in folders.** Only folders you add are synced.
- **Delta transfer.** Changes travel as content-defined chunks, so a small edit to a big file sends only kilobytes.
- **Conflicts.** Concurrent edits are detected with version vectors, and both versions are kept.
- **No third parties.** There is no server, no account and no telemetry.

It is written in Rust: about 9,600 lines in 4 crates plus a test package. Repo: `github.com/lucasvlad/nebula` (private).

## 2. Where everything is

| What | Where |
|---|---|
| Design brief (locked design, kept up to date) | `docs/brief.md` (the copy in the Capstone folder is older) |
| Design decisions with sources, one per topic | `docs/decisions/0001`–`0006` (ADRs, see §5) |
| Evaluation results (tables) | `docs/evaluation/` (start at `README.md`) |
| Regenerate all results (~80 s) | `eval/run-all.sh` |
| Presentation attack demo | `demo/README.md`, `demo/mitm-demo.sh`, `cargo run -p nebula-tests --bin mitm-demo` |
| User-facing usage | `README.md` |
| Notes for coding sessions | `CLAUDE.md` |
| UI colours (slides, any visuals) | `docs/ui-palette.md`: Deep Space Blue `#1A2749`, Cosmic Purple `#5D3FD3`, Alabaster `#E6E6E6`, Nebula Teal `#2DE1C2`, Alert `#FF6B35` |
| Snapshot per milestone | git tags `m0`–`m5` (e.g. `git checkout m2`) |

## 3. Timeline (what was built when)

| Tag / PR | Date | What |
|---|---|---|
| `m0` / #1 | Oct 13 | Workspace, CI (fmt, clippy, tests on 3 OSes, `cargo audit`), ADR log |
| `m1` / #2–#4 | Oct 13 | Core logic (paths, chunker, version vectors, conflicts), scanner, SQLite store, CLI |
| `m2` / #5–#7 | Oct 13 – Nov 4 | Pairing, encrypted sessions, discovery, fault proxy, security codes, attack demo |
| `m3`+`m4` / #8 | Nov 4 | Two-way sync engine, file watcher, background daemon (M3 and M4 merged into one) |
| `m5` / #9 | Nov 13 | Evaluation harness and results; SQLite CVE fix |

Schedule notes: M1 and M2 finished well ahead of plan. M3 slipped about 4 days, recovered by
building M4 at the same time. Remaining: M6 freeze (bug fixes only) Nov 16–20, draft paper Nov 21,
final Dec 9.

## 4. How the code is organised

```
crates/nebula-core   pure logic, no network or files: easy to test exhaustively
crates/nebula-net    everything security- and network-related
crates/nebula-sync   state database, scanning, watching, the sync engine
crates/nebula-cli    the `nebula` program (commands + background daemon)
tests/               cross-crate tests, fault-injection proxy, evaluation, attack demo
```

### nebula-core
| File | What it does |
|---|---|
| `path.rs` | `RelPath`: a validated path inside a synced folder. It is NFC-normalised (macOS gives NFD), uses `/` separators and must be legal on all 3 OSes. It rejects absolute paths, `..`, Windows-reserved names (`CON`, `COM1`…) and bad characters. It also detects case-only name clashes (macOS and Windows are case-insensitive). |
| `chunker.rs` | Content-defined chunking: a Rabin–Karp rolling hash over a 48-byte window decides chunk boundaries by content, with a min/avg/max of 2/8/64 KiB. A streaming reader keeps memory at about 2× the max chunk. Chunks are identified by their BLAKE3 hash. |
| `version.rs` | Version vectors: a per-file counter for each device. Comparing two vectors gives equal, newer, older or *concurrent* (a conflict). |
| `conflict.rs` | Decides keep, take or conflict for a pair of versions, and builds the conflict-copy name (`name (conflict copy <device> <time>).ext`). It is symmetric, so both devices pick the same winner independently. |
| `manifest.rs` | `FileEntry` records size, mtime, content hash, chunk list, version vector, and a deleted flag (tombstones let deletions sync). |
| `lib.rs` | Shared types: `Digest` (BLAKE3) and `DeviceId` (BLAKE3 of the device's public key). |

### nebula-net
| File | What it does |
|---|---|
| `identity.rs` | Each device's long-term X25519 keypair, created on first run and stored with owner-only permissions (0600). It never leaves the device. |
| `frame.rs` | Length-prefixed messages with a hard size limit, so a peer can't make us allocate huge buffers. |
| `transport.rs` | Encrypted sessions using the Noise XX pattern. The allowlist is checked as soon as the peer's key is known, and an unknown device is dropped before any file information is exchanged. Messages are split across Noise frames. Any tampering ends the session. Sessions split into send/receive halves for the daemon. |
| `pairing.rs` | One-time pairing: a 6-digit code drives SPAKE2 (a password-authenticated key exchange), whose output keys a Noise XXpsk2 handshake that exchanges and pins both devices' keys and names. The code expires in 2 min and is locked out after 3 wrong tries. |
| `sas.rs` | The "security code": 4 words both devices derive from the handshake hash, compared by eye to rule out a man-in-the-middle (like Signal or ZRTP). |
| `discovery.rs` | mDNS on the local network. It advertises only the device ID, never folder or file names or the real hostname. `--addr` / `--connect` work without it (campus Wi-Fi often blocks mDNS). |

### nebula-sync
| File | What it does |
|---|---|
| `store.rs` | SQLite in WAL mode (crash-safe). Tables: folders (with a label), file manifests, the paired-device allowlist, and settings. Schema migrations run automatically. |
| `scanner.rs` | Walks a folder and updates its manifest. It only re-reads files whose size or mtime changed, with a 2 s "racy" window so coarse-mtime edits aren't missed. It never mass-deletes if a folder is missing, never tombstones files under an unreadable directory, and skips symlinks, illegal names and case clashes. |
| `watcher.rs` | OS file-change notifications (debounced 500 ms) trigger a rescan of that folder. |
| `protocol.rs` | Wire messages: Hello, Ping/Pong, Note (demo), Index (file list), Request/Chunk/Missing (chunk transfer). |
| `engine.rs` | **The sync engine.** A pure state machine: inputs (connected, message, local change, tick) go in and messages to send come out. It exchanges indexes and decides per file using version vectors. It requests only chunks it doesn't already have, verifies every chunk, assembles in a temp file, verifies the whole file and renames it into place. Interrupted downloads resume. It never overwrites an edit it hasn't seen. Conflicts keep both versions. |

### nebula-cli (`nebula` program)
| File | What it does |
|---|---|
| `main.rs` | Commands: `id add remove folders scan ls pair name peers unpair run ping`. `--home` / `NEBULA_HOME` runs a second "device" on one machine. `-v` / `-vv` explain what's happening. |
| `pair.rs` | `pair` (show or enter code), `peers`, `unpair`, device naming, prints the security code. |
| `run.rs` | `ping`: connect, show the security code, time round trips, optionally send an encrypted note. |
| `daemon/` | `nebula run`: the engine on its own thread, the file watcher, accepting and making connections (mDNS, last address, `--connect`), resolving duplicate connections, 30 s keepalive, and cutting a session within 5 s of `unpair`. |

### tests/ (not shipped in the program)
| File | What it does |
|---|---|
| `src/proxy.rs` | Fault-injection TCP proxy: drop, delay, truncate or corrupt traffic, and capture raw bytes. |
| `tests/security.rs` | Security evaluation over real TCP through the proxy. |
| `src/bin/eval.rs` | Delta-efficiency and resume measurements. |
| `src/bin/mitm_demo.rs`, `src/demo.rs` | The eavesdropping and man-in-the-middle presentation demo. |

## 5. Sources and where each one shows up

These are the six paper sources from brief §9, each verified against the publication.

| # | Source | Used for (in code) | ADR |
|---|---|---|---|
| 1 | Parker et al. (1983), "Detection of Mutual Inconsistency in Distributed Systems," *IEEE TSE* 9(3):240–247, doi 10.1109/TSE.1983.236733 | Version vectors and the dominate/concurrent rule (`version.rs`, `conflict.rs`, `engine.rs`) | 0003, 0006 |
| 2 | Shapiro et al. (2011), "Conflict-Free Replicated Data Types," arXiv:1805.06358 | Convergence as the correctness criterion (the property test); why we keep conflict copies instead of merging | 0003, 0006 |
| 3 | Muthitacharoen, Chen & Mazières (2001), "A Low-Bandwidth Network File System," SOSP '01 | Content-defined chunking, chunk-level transfer, the bytes-sent metric (`chunker.rs`, `engine.rs`, `eval.rs`) | 0002, 0006 |
| 4 | Trautwein, Raulf & Hilt (2023), "Introducing Peer Copy," arXiv:2305.02049 | mDNS discovery and PAKE pairing (`discovery.rs`, `pairing.rs`) | 0005 |
| 5 | Dowling, Rösler & Schwenk (2020), "Flexible Authenticated and Confidential Channel Establishment (fACCE): Analyzing the Noise Protocol Framework," PKC 2020, LNCS 12110, pp. 341–373; IACR ePrint 2019/436 | Security grounding for Noise XX sessions (`transport.rs`) | 0004 |
| 6 | Mehdi & Ragab-Hassen (2016), "File Synchronization Systems Survey," arXiv:1611.05346 | Related work, and the motivation that P2P sync tools have authentication gaps. Treat it as lighter weight | (none) |

Reference only, not counted as sources: the Noise Protocol specification (Perrin), docs for the `spake2` / `snow` / `blake3` crates, and RFC 9382 (SPAKE2).
Worldview sources: Dyer, *From the Garden to the City*; ACM Code of Ethics (privacy). They are tracked separately.

**Exact facts from the sources that the ADRs rely on:**
- **Dowling et al.** prove exactly 8 Noise patterns: N, NN, NX, NK, X, XN, **XX**, XK (§6 p. 27, Table 1 p. 29, Appendix E). XX is Theorem 8 (§E.7): the responder is authenticated from stage 2 and the initiator from stage 3, with forward secrecy, KCI resistance and replay resistance. KK and IK are *not* proven. The paper does not cover identity hiding (future work, §7), key distribution (§3, so our pairing isn't covered), or the concrete libraries.
- **LBFS** used Rabin fingerprints over 48 bytes, SHA-1 chunk IDs, and about 8 KiB average chunks. Nebula keeps the 48-byte window and 8 KiB average but uses a Rabin–Karp polynomial hash and BLAKE3, which is faster, and SHA-1 is broken for collisions.
- **Peer Copy** runs a PAKE for every transfer. Nebula runs one only at pairing and then pins keys.

## 6. Decisions that differ from the original plan (say so in the paper)

1. **Noise XX, not KK or IK** (ADR 0004). The brief said "IK or KK fit". Checking the paper showed neither is proven. XX is proven and works with `--addr` when you don't know in advance which device will answer. Trade-off: anyone can learn a device's public key by connecting, but mDNS already broadcasts its hash.
2. **Pairing with XXpsk2, not XXpsk3** (ADR 0005). The first design mixed the code-derived key in last. A subagent showed that a fake listener could then get its key pinned before the code was checked. A test demonstrates the flaw (`xxpsk3_leaves_responder_static_unauthenticated`). This is a good "found it in design review" story.
3. **The conflict data-loss bug caught before release** (ADR 0006). The winning side originally merged version histories immediately. If that reached the loser before its conflict copy existed, the loser's edit would be overwritten. Fix: only the loser merges, after saving the copy.
4. **M3 and M4 merged** into one bidirectional engine, to recover schedule.
5. **Added beyond the brief:** security codes (four words) and the attack demo, from Checkpoint 2 feedback; folder **labels** match folders across devices; a **16 MiB message cap**, which limits files to about 3.5 GB in the MVP.

## 7. Evaluation results (brief §6), summarised

Full tables are in `docs/evaluation/`. Machine: Apple M3 MacBook, 16 GiB, macOS 26, loopback.

| Evaluation | Result |
|---|---|
| Correctness | **1000/1000** random edit/delete sequences on two devices converged to identical folders |
| Delta, 100 MiB file | Insert 1 byte at start: **7.1 KiB** sent (fixed 8 KiB blocks: 100 MiB). Insert 1 KiB mid-file: 55.7 KiB (fixed: 50 MiB). Delete 1000 B mid-file: 53.8 KiB (fixed: 50 MiB). Append 1 MiB: 1.0 MiB. **Overwrite 1 byte: 54.7 KiB (fixed: 8 KiB, so fixed blocks win here)** |
| Conflicts | Both versions kept; zero bytes lost (tested in-process and live) |
| Large files | 1 MiB in 0.14 s; 100 MiB in 0.71 s (141 MiB/s); **1 GiB in 7.8 s (132 MiB/s)**. Receiver peak memory 28 MiB; sender 93 MiB at 1 GiB |
| Dropped connection | `kill -9` at 52 % of 1 GiB, then restart: kept 68,229 of 131,009 chunks, done in 3.0 s, byte-identical |
| Security | All pass: no plaintext or keys on the wire, a flipped bit is rejected (handshake and data), an unpaired device is refused before any data, 3 wrong codes lock out, the code never appears on the wire |

Interpreting the numbers:
- Content-defined chunking shines on **insertions and deletions** (shifted data), not on in-place overwrites.
- The 1 MiB throughput is low only because connection setup dominates at that size.
- Sender memory grows with file size, from the chunk locator and the index entry for 131k chunks.
- These are loopback numbers. A Mac ↔ PC run over Wi-Fi with `eval/large-files.sh` would be more realistic.

## 8. Security model (for the security section)

- **Attacker:** someone on the same LAN or open Wi-Fi who can sniff, replay, tamper with or inject traffic, or impersonate a paired device.
- **Not covered:** a compromised device, a stolen unlocked laptop, and traffic-analysis metadata (timing, sizes, device ID in mDNS).
- **Mechanisms:**
  - Pinned keys after PAKE pairing.
  - Noise XX with mutual authentication.
  - An allowlist checked before any metadata is exchanged.
  - Hash-verified chunks.
  - Path validation (no `..`, no symlink escape).
  - Bounded message sizes.
  - Revocation via `unpair`, effective within 5 s.
  - Security codes to rule out a man-in-the-middle by eye.
- **Pairing math:** 3 tries against 10⁶ codes gives a 3×10⁻⁶ chance of an online guess succeeding. SPAKE2 prevents offline guessing.
- **Known residual risks** (in the ADRs):
  - A LAN attacker can burn the 3 pairing attempts (denial of service, not a compromise).
  - XX reveals the responder's public key to any active connector.
  - The identity-hiding properties are unproven.

## 9. Course tie-ins (brief §10) and worldview features (brief §11)

**Course tie-ins:**
- **COS 327 Networking:** framing, P2P protocol design, mDNS.
- **COS 326 OS:** the watcher and races, atomic rename, crash safety (WAL, temp files), the "racy mtime" problem.
- **COS 311 Security:** authentication, confidentiality, integrity, the allowlist.
- **COS 417 Software Security:** input validation, path traversal, fault injection, the XXpsk3 flaw.
- **COS 440 Paradigms:** Rust ownership, errors as values, no `unsafe`, `unwrap` banned.
- **COS 230/210:** rolling hash and partial orders (version vectors).
- **COS 260 Ethics:** stewardship and privacy.

**Stewardship features that really exist:**
- Folders are opt-in.
- No cloud, accounts or telemetry.
- mDNS advertises only the device ID, and pairing advertises a random name.
- Keys never leave the device (0600 file).
- Revocable trust: `unpair` cuts live sessions, and `remove` stops syncing without touching files.

## 10. Known limitations and future work

**Limitations:**
- Empty folders aren't synced.
- A rename is a delete plus an add (cheap thanks to chunk reuse, but the version history doesn't follow the file).
- Symlinks are skipped.
- Files are capped at about 3.5 GB.
- Sender memory grows with file size.
- A file whose size and mtime are both unchanged and older than the last scan is trusted without re-reading (documented speed trade-off).
- Only local-network sync; tested with 2 devices.
- A manual Windows 11 real-device pass is still to do (Windows CI passes).
- **Known bug, fix planned in M6:** running `nebula scan` by hand while `nebula run` is active records the change, so the daemon never announces it. It only syncs after the next reconnect.

**Future work** (from brief §2):
- Internet/NAT traversal.
- 3+ devices.
- Mobile clients (iOS would need a separate Swift app and has no background folder access).
- CRDT merging of file contents.
- A local status page.
- Bluetooth transport.

## 11. Suggestions for the paper

- **Structure (~10 pages):**
  1. Introduction (problem, stewardship motivation).
  2. Related work (survey, LBFS, Peer Copy).
  3. Design: architecture, sync model, chunking, version vectors and conflicts.
  4. Security design and threat model.
  5. Implementation (Rust, crates, testing approach).
  6. Evaluation (§7 tables).
  7. Discussion and limitations.
  8. Worldview.
  9. Conclusion and future work.
- **Figures worth making:**
  - The crate architecture.
  - A sequence diagram of pairing (SPAKE2, then XXpsk2, then pinned keys) and of a normal session (XX, allowlist check, index exchange, chunk requests).
  - The delta-efficiency table, or a bar chart against fixed blocks.
- **Be honest about trade-offs:** overwrite vs. insert performance, the sender memory, loopback-only throughput, and what the Dowling proof does *not* cover. Reviewers trust a paper that states its limits.
- **The two caught design flaws** (XXpsk3 and the conflict merge order) show security and correctness engineering as a process. They're good material for the discussion section.
- **Consider stating in the paper** that GenAI assisted with development (allowed by the course) while the paper is your own writing, so the boundary is clear.
- Cite crate docs in code comments, not as paper sources (brief §9).

## 12. Suggested demo order (about 10 minutes, non-CS audience)

Prep: two machines (or the Mac plus the VM) on a hotspot or home network. Run `nebula add ~/Demo --as demo` on both. Have the commands in a script or notes.

1. **The problem (slide, 30 s):** "Your files go through someone else's server. What if they didn't have to?"
2. **Pairing (1 min):**
   - Run `nebula pair` on A, then enter the code on B.
   - Point at the **security words matching** on both screens ("like Signal's safety numbers").
3. **Live sync (2 min):**
   - `nebula run` on both.
   - Drop a photo into A's folder; it appears on B. Edit a document on B; it updates on A.
4. **Efficiency (1 min):**
   - Do this *before* starting `run`: edit one line of a big file and run `nebula scan`. The output says e.g. "1 of 12,203 chunks changed: only 2.3 KiB of 95.4 MiB needs sending".
   - Back it up with the §7 delta table on a slide.
   - Don't run `nebula scan` while `run` is active (see the §10 known bug).
5. **Conflict (1.5 min):**
   - Stop B. Edit the same file on both. Restart B.
   - Show both versions and the conflict copy: "nothing is silently lost".
6. **Security, part 1 (2 min):** `cargo run -p nebula-tests --bin mitm-demo` (`simulate`). Walk through the 4 acts:
   1. An unencrypted app leaks the message.
   2. Nebula shows gibberish to the eavesdropper.
   3. A middleman against an app that trusts anyone, where the **words differ**.
   4. Real Nebula refuses the middleman.
7. **Security, part 2 (30 s):** `nebula unpair` on A, and B is cut off within seconds.
8. **Results and stewardship (slides, 1.5 min):** the §7 table and the stewardship feature list.

**Backup plan:** if mDNS fails on the venue Wi-Fi, use `--addr` / `--connect` (both programs print the address). If the network fails entirely, use two `--home` directories on one laptop. Run `demo/mitm-demo.sh` beforehand to make sure everything still works.

## 13. Handy commands

```sh
cargo build --release                 # binary: target/release/nebula
cargo test --workspace                # 176 tests
eval/run-all.sh                       # regenerate docs/evaluation/
cargo run -p nebula-tests --bin mitm-demo            # attack demo (simulate)
NEBULA_HOME=~/nebula-test/b nebula ...               # act as a second device
```
