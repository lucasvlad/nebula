# Nebula — Codebase Brief (Final Plan v2)

**For:** the Claude Code session building Nebula. This is the locked design. Scaffold the repo and start with Milestone 1. Where a decision below is marked **[paper]**, record it as a short ADR in `docs/decisions/` (context, decision, alternatives, source it comes from) because the 10-page paper has to describe what was actually built.

## 1. What Nebula is

A cross-platform, peer-to-peer file sync tool with no cloud server. A user opts a folder into syncing (nothing syncs by default); paired devices on the same LAN keep that folder identical, like OneDrive without the cloud. Senior CS capstone, Covenant College (COS 492), project track with a 10-page supporting paper. Draft due **Nov 21**, final due **Dec 9, 2026**. Audience for the final presentation includes people outside the CS department, so the live demo should be easy to follow.

## 2. Scope

**MVP (graded on this):**
- Two devices on the same LAN: a MacBook plus the dual-boot PC. **First real two-device test: macOS + EndeavourOS (Linux).** Windows 11 is covered by CI from day one and gets a manual real-device pass in M5 (Windows is the stricter test: reserved filenames, case-insensitive paths, open files cannot be replaced, so atomic rename needs care). The PC dual boots, so only one of its OSes is on the network at a time.
- One-time pairing with a short code; unpaired devices are always rejected
- Bidirectional sync of any number of opted-in folders (CLI to add/remove)
- Content-defined chunking so small edits to big files transfer only the changed chunks
- Conflict handling: version vectors detect concurrent edits; last-write-wins at the original path, loser kept as a conflict copy; never silently lose data

**Stretch (only after the MVP freeze on Nov 20):** offline queue with auto-resync (mostly free given the index-exchange design), local status page for the demo, 3+ devices, Bluetooth transport, CRDT merge.

**Explicitly out of scope:** internet/NAT traversal, cloud relay, mobile apps (an iPhone client would need a separate Swift app and has no background folder access; mention as future work), file-explorer integration, selective/virtual files.

## 3. Repo layout and stack

```
nebula/
  Cargo.toml                 # workspace
  crates/
    nebula-core/             # chunker, hashing, manifest, version vectors, conflict logic (no network/IO policy)
    nebula-net/              # framing, Noise transport, SPAKE2 pairing, mDNS discovery
    nebula-sync/             # scanner, watcher, index exchange, chunk transfer, atomic apply
    nebula-cli/              # binary `nebula`
  tests/                     # integration tests + fault-injection TCP proxy
  docs/decisions/            # ADRs (feeds the paper)
  .github/workflows/ci.yml
```

Likely crates (verify current versions and APIs before using): `tokio`, `serde` + `postcard`, `blake3`, `snow` (Noise), `spake2`, `mdns-sd`, `notify`, `rusqlite` (bundled), `clap`, `tracing`, `thiserror`, `unicode-normalization`, `proptest`, `tempfile`.

## 4. Security design (explicit, tied to COS 311 / COS 417)

- **Identity:** each device generates a static keypair on first run, stored in the OS config dir with owner-only permissions. Device ID = hash of the public key.
- **Pairing [paper]:** `nebula pair` on A shows a short code and listens. `nebula pair <code>` on B finds A, and both run SPAKE2 with the code as the shared secret, then key confirmation. Over the resulting key they exchange static public keys and device names, and each stores the other in a local allowlist. Code expires in about 2 minutes and dies after 3 failed attempts. This adapts Peer Copy's PAKE idea: Peer Copy runs a PAKE per transfer, Nebula pairs once and pins keys. State that deviation in the paper.
- **Sessions [paper]:** every later connection is a Noise handshake with static keys (pattern chosen in an ADR: since keys are pinned after pairing, IK or KK fit; Dowling et al. prove 8 of the 15 patterns, so check which ones before claiming a proof covers ours). The remote static key must be in the allowlist or the connection is dropped before any file metadata is exchanged. All traffic is encrypted and authenticated.
- **Discovery:** mDNS (`_nebula._tcp`) advertises only the device ID, never folder names or file info. Always support an explicit `--addr host:port` fallback, because campus Wi-Fi often blocks multicast and client-to-client traffic; the demo must not depend on mDNS working.
- **Input validation:** length-prefixed frames with a hard max size; reject any path that is absolute, contains `..`, or resolves through a symlink outside the sync root; verify every chunk hash before it is written; treat all peer messages as hostile.
- **Revocable trust:** `nebula unpair <device>` removes a device from the allowlist; `nebula remove <folder>` stops syncing a folder without deleting files. No telemetry, no accounts, no network calls except to paired peers.
- **Threat model to state in the paper:** attacker on the same LAN (open Wi-Fi) who can sniff, replay, tamper, or impersonate a paired device. Not covered: compromised endpoints, a stolen unlocked laptop, traffic-analysis metadata.

## 5. Sync engine rules

- **State:** SQLite (WAL) holds the manifest per folder: relative path, size, chunk hash list, version vector, mtime, tombstone flag. Chosen for crash safety.
- **Chunking [paper]:** implement our own content-defined chunker with a rolling hash (LBFS-style, target about 8 KiB average with min/max bounds) behind a trait, with unit tests. BLAKE3 identifies chunks (LBFS used SHA-1; note the change). Optionally benchmark against the `fastcdc` crate for the paper.
- **Transfer [paper]:** peers exchange manifests, diff chunk lists, and request only missing chunks. Chunks stream to a temp store; after the whole file verifies, atomic rename into place. This makes dropped connections resumable by construction and keeps memory flat for 1 GB+ files.
- **Version vectors [paper]:** per file, one counter per device. If A's vector dominates B's, fast-forward. If neither dominates, it is a conflict. Resolution: newest mtime (device ID breaks ties) keeps the original path, the other is saved as `name (conflict copy <device> <timestamp>).ext`. Concurrent delete vs. edit keeps the edit.
- **Watcher pitfalls:** debounce events, ignore writes Nebula made itself (otherwise sync loops), re-scan on startup to catch changes made while the daemon was off, and skip temp files.
- **Cross-platform pitfalls to handle from day one:** store paths as normalized UTF-8 with `/` separators; NFC-normalize names (macOS can hand back NFD); detect case-only collisions (macOS/Windows are case-insensitive, Linux is not); reject Windows-reserved names; coarse mtime resolution on some filesystems, so never rely on mtime alone for change detection (hash when in doubt).
- **Process model (MVP):** `nebula run` runs the daemon in the foreground; other subcommands edit state in SQLite; no IPC needed. A tray app or service install is stretch.

## 6. Testing and evaluation (measurable; build the harness early)

Write a fault-injection TCP proxy in `tests/` that can drop, delay, truncate, and corrupt traffic between two in-process peers.

| Evaluation | Pass criteria |
|---|---|
| Sync correctness | Randomized operation sequences (proptest) on two peers always converge to identical directory hashes |
| Delta efficiency | Edit 1 byte in a 100 MB file and insert at the start; report bytes sent vs. file size (the LBFS-style result for the paper) |
| Conflict handling | Disconnect, edit same file on both, reconnect: conflict copy exists, zero bytes lost |
| Large files | 1 MB, 100 MB, 1+ GB: hash-verified, throughput logged, memory stays bounded |
| Dropped connections | Kill mid-transfer: resumes, transfers only missing chunks, no corruption |
| Security | Unpaired key rejected; wrong code rejected; lockout after 3 bad codes; flipped ciphertext bits rejected; captured bytes never contain a known plaintext marker; path traversal attempts rejected |

## 7. Dev practices

- GitHub flow: `main` protected behind passing CI; branches `feat/<subsystem>`; PRs even solo, each linking its ADR if it carries a **[paper]** decision.
- CI on ubuntu, macos, and windows runners: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, plus `cargo audit`.
- Tag each milestone (`m1`…`m6`) so the paper can reference clean snapshots.
- Code style: minimal, tightly scoped, no regex, no `unwrap` in library code, no speculative abstraction.

## 8. Milestones (today is Oct 12; backward from Nov 21 and Dec 9)

| Dates | Milestone |
|---|---|
| Oct 12–18 | **M1** Scaffold, CI, ADR log. `nebula-core`: chunker, scanner, manifest, version vectors, conflict logic with unit and property tests. No networking yet. |
| Oct 19–25 | **M2** `nebula-net`: framing, Noise transport, SPAKE2 pairing, allowlist, mDNS plus `--addr`. Localhost integration tests and the fault proxy. |
| Oct 26–Nov 1 | **M3** One-way sync between two real processes; watcher; atomic apply. **First real two-OS test by Nov 1.** |
| Nov 2–8 | **M4** Bidirectional sync, conflicts, deletes, `unpair`/`remove`, CLI polish. |
| Nov 9–15 | **M5** Full evaluation harness run; manual Windows 11 real-device pass; collect numbers and screenshots; draft results and security sections of the paper. |
| Nov 16–20 | **M6 MVP freeze.** Bug fixes only. Draft paper complete. **Nov 21 draft due.** |
| Nov 22–Dec 4 | **M7** Advisor feedback, stretch goals if stable, final paper, demo rehearsal for a non-CS audience. **Dec 9 final due.** |

Cut line: if M3 slips past Nov 4, drop mDNS to `--addr` only and defer everything in the stretch list.

## 9. Sources to code (narrowed; each earns its place)

Principle: reuse sources across the paper, and add one only if it holds information no other source does and the code needs it. All six below were verified against the real publications.

| # | Source | Used for | Why it is not redundant |
|---|---|---|---|
| 1 | Parker et al. (1983), "Detection of Mutual Inconsistency in Distributed Systems," IEEE TSE 9(3), 240–247, doi 10.1109/TSE.1983.236733 | Version vectors and the dominate/concurrent rules for conflict detection | Primary source for the exact file-level detection algorithm we implement |
| 2 | Shapiro et al. (2011), "Conflict-Free Replicated Data Types," arXiv:1805.06358 | Convergence as the correctness criterion for the sync tests; scoping statement on why we do not implement CRDT merge | Defines convergence and the merge approach we deliberately do not adopt |
| 3 | Muthitacharoen, Chen, & Mazières (2001), "A Low-Bandwidth Network File System," SOSP '01 | Content-defined chunking, chunk-level transfer, and the bytes-sent evaluation metric | Reused for design and evaluation |
| 4 | Trautwein, Raulf, & Hilt (2023), "Introducing Peer Copy," arXiv:2305.02049 | mDNS discovery, PAKE pairing, and comparison-against-established-tools evaluation approach | Reused for design and evaluation |
| 5 | Dowling, Rösler, & Schwenk (2020), "Flexible Authenticated and Confidential Channel Establishment (fACCE): Analyzing the Noise Protocol Framework," PKC 2020 (LNCS), 341–373; eprint 2019/436 | Security grounding for Noise-based sessions | Peer Copy runs a PAKE per transfer; none of the others cover Noise static-key sessions |
| 6 | Mehdi & Ragab-Hassen (2016), "File Synchronization Systems Survey," arXiv:1611.05346 | Related work (cloud vs. P2P sync) and its note that P2P systems have authentication and authorization gaps, which motivates our security model | Only source surveying existing sync tools. It is an arXiv preprint, so treat it as lighter weight and do not lean on its claims about BitTorrent superiority |

Dropped on purpose: FastCDC (we implement our own LBFS-style chunker; add it only if we keep the optional benchmark), the original SPAKE2 paper (Peer Copy covers PAKE pairing; cite the crate docs in code), and a separate evaluation-methodology paper (LBFS and Peer Copy already give evaluation precedent). Reference-only, not counted toward the six: the Noise specification (Perrin).

Worldview sources (Dyer, *From the Garden to the City*; the ACM Code of Ethics, principle on respecting privacy; plus whatever the research session returns) are tracked separately and do not change the code.

If a decision above changes, update this brief before coding that part.

## 10. Coursework tie-ins (for ADRs and the paper)

| Course | Where it shows up |
|---|---|
| COS 327 Network Programming | sockets, framing, P2P architecture, mDNS/DNS, protocol design |
| COS 326 Operating Systems | concurrency and races (watcher, sync loops), file systems, atomic rename, crash recovery, fault tolerance, performance evaluation |
| COS 311 Computer and Information Security | authentication, confidentiality, integrity, threat model, access control via allowlist |
| COS 417 Software Security Engineering | input validation, path traversal, race conditions, network software security, security testing |
| COS 440 Programming Paradigms | why Rust: ownership and type-state for protocol states, error handling as values |
| COS 230 / COS 210 | rolling hash and big-O of chunking and diffing; partial orders behind version vectors |
| COS 260 Ethical and Professional Issues | technology in scripture, cultural mandate, man as creator, IEEE/ACM code (privacy) for the stewardship section |

## 11. Worldview link (the paper's section must describe real features)

Stewardship: a person's files are entrusted to them, so Nebula is built to keep them out of third-party hands. The concrete features: opt-in per folder, no cloud or telemetry, minimal discovery metadata, keys that never leave the device, and revocable trust (`unpair`, `remove`). Log these in ADRs as they are built.

## 12. First tasks for Claude Code

1. Create the workspace, CI workflow, `docs/decisions/` with an ADR template, and a README with the one-paragraph pitch.
2. Implement `nebula-core` chunker, manifest, and version vectors with tests (Milestone 1).
3. Open a PR per subsystem; keep each PR small enough to review.
