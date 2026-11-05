# Nebula

Nebula keeps folders identical across your own devices on the same local
network, like OneDrive without the cloud. Nothing syncs until you opt a
folder in. Devices pair once with a short code, and after that every
connection is encrypted and authenticated, and unknown devices are refused.
Files never pass through a third-party server, there are no accounts, and
there is no telemetry. Large files sync efficiently: Nebula splits them into
content-defined chunks and sends only the chunks that changed. When two
devices edit the same file while apart, Nebula keeps both versions rather
than silently losing one.

Senior capstone, Covenant College (COS 492), Fall 2026.

> **Status:** early development. See [docs/brief.md](docs/brief.md) for the design and milestones.

## Building

Requires a recent stable Rust toolchain (`rustup update stable`).

```sh
cargo build --release      # binary at target/release/nebula
cargo test --workspace
```

## Usage

```sh
nebula add ~/Documents/Shared     # opt a folder in (nothing syncs by default)
nebula scan                       # index changes in every synced folder
nebula ls ~/Documents/Shared      # what Nebula knows about each file
nebula folders                    # list synced folders
nebula remove ~/Documents/Shared  # stop syncing; files are left alone
nebula id                         # this device's ID
nebula scan -v                    # explain what was found (-vv: every decision)
```

Pairing and connecting (run each half on a different device):

```sh
nebula pair                       # device A: shows a code like 123-456
nebula pair 123-456               # device B: finds A on the network and pairs
nebula peers                      # list paired devices
nebula run                        # keep synced folders in sync with paired devices
nebula ping                       # from the other device: connect and time round trips
nebula unpair <name>              # revoke a device
```

To sync a folder, add it on both devices under the same name, then leave
`nebula run` running on both:

```sh
nebula add ~/Documents/Homework --as homework     # on each device
nebula run                                        # on each device
```

Changes sync within a few seconds. If both devices edit the same file while
apart, the newer edit keeps the name and the other is saved next to it as
`name (conflict copy <device> <time>).ext`. Nothing is silently lost.

If the network blocks discovery (common on campus Wi-Fi), add
`--addr <ip>[:port]` to `pair` and `ping`, and `--connect <ip>[:port]` to `run`.
`nebula pair` and `nebula run` print the address to use.

State lives in the OS data directory. Pass `--home <dir>` or set
`NEBULA_HOME` to use another one (handy for running two devices on one machine).

## Layout

| Path | Contents |
|---|---|
| `crates/nebula-core` | Chunking, hashing, manifest types, version vectors, conflict logic |
| `crates/nebula-net` | Framing, Noise transport, SPAKE2 pairing, mDNS discovery |
| `crates/nebula-sync` | Scanner, watcher, index exchange, chunk transfer, atomic apply |
| `crates/nebula-cli` | The `nebula` binary |
| `tests/` | Integration tests and the fault-injection TCP proxy |
| `docs/decisions/` | Architecture decision records |
