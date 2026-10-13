# ADR 0003: Per-file version vectors for change causality

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone / PR:** M1 / #2

## Context

When two devices disagree about a file, Nebula must tell "B has already seen
A's edit" (B can simply take A's version) from "A and B edited independently"
(a conflict). Wall clocks on different devices cannot be trusted to make that
call: they skew, drift, and are set by users.

## Decision

Each file carries a version vector: one counter per device ID, with a missing
device meaning 0. A device increments its own counter when it edits the file.
Comparing two vectors pointwise gives one of four results:

- **Equal:** nothing to do.
- **After / Before:** one side dominates, so the older side fast-forwards.
- **Concurrent:** neither dominates, so the edits were independent and it is a conflict.

Conflicts are resolved as in brief §5. The newest mtime keeps the original
path, with the device ID breaking ties, and the loser is saved as a conflict
copy. A concurrent delete and edit keeps the edit. mtime is used only as a
tie-breaker after concurrency has been detected, never to detect it.

Equality is semantic: an explicit zero entry equals an absent one, because a
hostile peer could send zeros. `merge` and `increment` never insert zeros.

## Alternatives considered

- **Last-write-wins by timestamp alone**: simplest, but under clock skew it silently discards one side's edit.
- **Single global Lamport counter**: orders events but cannot detect concurrency, so every conflict would look like a fast-forward.
- **CRDT merge**: would merge contents automatically, but is out of scope for whole-file sync (Shapiro et al. 2011, brief source #2).

## Source

Parker, Popek, Rudisin et al., "Detection of Mutual Inconsistency in
Distributed Systems," IEEE Trans. Software Engineering 9(3), 1983, pp.
240-247, doi 10.1109/TSE.1983.236733 (brief source #1). The dominance
relation is a partial order, as in COS 210/230. We use the paper's vectors
unchanged and add only the conflict policy above.

## Consequences

A vector grows with the number of devices that have ever edited the file.
That is fine for 2-3 devices and unbounded in principle. Antisymmetry,
transitivity and merge laws are property-tested, and the convergence tests
(brief §6) rely on them. The paper detects conflicts but does not resolve
them, so resolution is tested separately.
