# ADR 0006: Index exchange, chunk transfer and conflict application

- **Status:** Accepted
- **Date:** 2026-11-04
- **Milestone / PR:** M3–M4 / #8

## Context

Two paired devices must keep a folder identical in both directions, transfer only changed data,
survive dropped connections, and never silently lose an edit (brief §5). Peers are hostile in the
sense that any message may be malformed or malicious, even from a paired device.

## Decision

**Engine as a pure state machine.** The sync engine takes inputs (connected, disconnected, message
received, local change, tick) and returns messages to send. It does file IO in synced folders but has
no sockets, threads or timers, so two engines can be connected in memory and tested for convergence.

**Index exchange.** On connect each device sends its whole manifest per folder (`Index`), batched under
8 MiB per message. After that it sends only entries whose version or deleted flag changed. Folders are
matched by a user-chosen label, not by path.

**Decision per entry** uses version vectors (ADR 0003): dominated means keep ours; dominating means
fetch theirs; concurrent means a conflict, resolved by `nebula_core::conflict::resolve`.

**Transfer.** A download requests only chunks not already present in the folder (any file), keeping 32
requests in flight. Each chunk is verified against its BLAKE3 hash before it is written. The file is
assembled in `.nebula-tmp-<content hash>` next to its destination, verified against the whole-file
hash, given the sender's mtime, fsynced, and renamed into place. Because the temp name depends on the
content, an interrupted download resumes: the verified prefix of the temp file is kept, and only the
remaining chunks are requested.

**Never overwrite unseen edits.** Immediately before renaming or deleting, the engine checks that the
file on disk still matches its manifest entry (exists or not, size, mtime). If not, the user changed it
during the transfer, so the engine rescans and decides again instead.

**Conflicts: only the loser merges.** The device whose version lost renames its file to the conflict
copy name, installs the winner, then records a version vector that merges both histories and announces
it. The winning device does nothing until that announcement arrives. An earlier draft had the winner
merge immediately, but that lost data: the merged (dominating) vector could reach the loser before its
copy existed, and the loser would then fast-forward over its own edit. Merging only after the copy is
saved makes "dominates" true in substance as well as on paper.

**Own writes.** The engine updates the manifest with the stat of each file it installs. The watcher's
rescan then finds nothing new, so no special suppression is needed and sync loops cannot start.

**Validation.** Entries whose chunk lengths don't sum to the size, or exceed the chunker maximum, are
dropped. Paths that would pass through a symlink or a file, or that differ only by case from a local
file, are skipped. Chunk requests are answered only from the named folder's manifest, and the data is
re-hashed before it is sent.

## Alternatives considered

- **Request every chunk of a changed file.** Simpler, but loses LBFS's benefit across files and on renames.
- **Both sides create the conflict copy.** Deterministic names make this possible, but the content then
  has to be duplicated in two places and both sides race. Loser-only is simpler.
- **rsync-style rolling checksums per transfer.** Needs a round trip per file and more CPU on the sender;
  content-defined chunk lists are already in the manifest.

## Source

Parker et al. (1983) for the dominance and concurrency rules; Muthitacharoen, Chen & Mazières (2001,
LBFS) for chunk-level transfer; Shapiro et al. (2011) for convergence as the correctness criterion.

## Consequences

Convergence is tested by a property test of random writes and deletes on both sides with random sync
points (`crates/nebula-sync/tests/sync.rs`; 300 cases pass). Empty directories are not synced or
removed. The message limit (16 MiB) caps files at about 3.5 GB in the MVP.
