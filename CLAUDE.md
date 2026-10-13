# Nebula: notes for Claude Code

Peer-to-peer LAN folder sync in Rust. Senior capstone with a 10-page paper.
**`docs/brief.md` is the locked design. Read it before starting a milestone.**
If a decision in it changes, update the brief in the same PR as the code.

Key dates: MVP freeze Nov 20, draft paper Nov 21, final Dec 9, 2026.

## Commands

```sh
cargo build
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo audit
cargo run -p nebula-cli -- <args>     # runs the `nebula` binary
```

Run fmt, clippy and the tests before every commit. CI runs the same checks
on ubuntu, macos and windows.

## Git workflow (self-enforced; GitHub does not block it)

- Never commit directly to `main`. Branch as `feat/<subsystem>`, `fix/<thing>` or `chore/<thing>`.
- One PR per subsystem, small enough to review. Merge only when CI is green on all three OSes. Squash-merge.
- When a PR carries a **[paper]** decision from the brief, add an ADR in `docs/decisions/` (copy `0000-template.md`) and link it in the PR.
- Tag milestone completions on `main`: `m1` … `m6`.
- The user is the sole developer and tests at each checkpoint. Stop at milestone checkpoints with a progress report and concrete things to test by hand.

## Code style

- Minimal and tightly scoped. No speculative abstraction. One trait only where the brief calls for one (e.g. `Chunker`).
- No `unwrap` outside tests (clippy denies it). Avoid `expect` too. Library errors use `thiserror`. The binary may use `anyhow`.
- No regex.
- `unsafe` is forbidden workspace-wide.
- Treat every peer message as hostile: bounded frame sizes, path validation, hash-verify chunks before writing them.
- Keep `nebula-core` free of network and filesystem policy, so it stays pure and property-testable.

## Cross-platform rules (enforced from day one)

- Store paths as normalised UTF-8, `/`-separated and NFC (macOS can return NFD).
- Reject absolute paths, `..`, Windows-reserved names, and symlinks that escape the sync root.
- Detect case-only collisions (macOS and Windows are case-insensitive).
- Never trust mtime alone for change detection. Hash when in doubt.
- Writes go to a temp file, then an atomic rename. Windows cannot replace open files.

## Testing

- Unit tests next to the code. Property tests (`proptest`) for version vectors, the chunker and convergence.
- Cross-crate integration tests and the fault-injection proxy go in `tests/` (the `nebula-tests` package).
- Evaluation targets with pass criteria are in brief §6. They become the paper's results section.

## UI

There is no UI in the MVP. For the stretch status page or the demo, use the palette in `docs/ui-palette.md`.
