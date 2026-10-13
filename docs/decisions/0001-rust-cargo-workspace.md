# ADR 0001: Rust, split into a four-crate Cargo workspace

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone / PR:** Scaffold / #1

## Context

Nebula parses hostile network input, writes to the user's files, and runs
concurrent tasks (watcher, network, apply) against the same state. It has to
build on macOS, Linux and Windows from one codebase.

## Decision

Write Nebula in Rust (edition 2024) as a Cargo workspace with four crates:

- `nebula-core`: pure logic (chunking, hashing, version vectors, conflicts). No network, no IO policy, so it is easy to unit and property test.
- `nebula-net`: framing, Noise sessions, SPAKE2 pairing, discovery.
- `nebula-sync`: scanning, watching, index exchange, transfer, apply.
- `nebula-cli`: the `nebula` binary.

Integration tests and the fault-injection proxy live in a fifth package, `tests/` (`nebula-tests`).

`unsafe` is forbidden workspace-wide, and `unwrap` is a clippy error outside tests.

## Alternatives considered

- **Go**: simple concurrency and cross-compilation, but no compile-time guarantee against data races, and nil and error handling is by convention.
- **C++**: fast, but memory safety in the network parser would rest entirely on discipline.
- **One crate**: less boilerplate, but the core logic would sit next to IO and be harder to test in isolation.

## Source

COS 440 Programming Paradigms: ownership, type-state for protocol states, and errors as values.

## Consequences

The borrow checker rules out data races between the watcher and apply tasks
at compile time. Crate boundaries enforce the core/IO split. Builds are
slower than Go's.
