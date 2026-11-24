# Nebula app: frontend ↔ backend contract

The Tauri app (`crates/nebula-app`) runs the sync daemon in-process. The frontend (`crates/nebula-app/ui/`,
plain HTML/CSS/JS, no build step) talks to it only through the commands and events below, using Tauri's
global API (`withGlobalTauri: true`):

```js
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { open } = window.__TAURI__.dialog;   // native folder picker
```

All commands are async. On failure they reject with a **plain-English string** meant to be shown to the user.
IDs are 8-hex-character short device IDs unless noted. Sizes are bytes. Times are Unix milliseconds.

## Commands

| Command | Arguments | Returns |
|---|---|---|
| `status` | none | `Status` (below) |
| `pair_start` | none | `{ code: "123-456", expires_in_secs: 120 }`. The result arrives later as a `paired` or `pair_failed` event |
| `pair_cancel` | none | `null` |
| `pair_join` | `{ code: string, addr: string \| null }` | `Paired` (below); rejects with e.g. "Wrong code" or "No device is waiting to pair" |
| `unpair` | `{ id: string }` | `null` |
| `add_folder` | `{ path: string, label: string }` | `null` |
| `remove_folder` | `{ label: string }` | `null` (files on disk are untouched) |
| `rename_device` | `{ name: string }` | `null` |
| `connect_addr` | `{ addr: string }` | `null`. Adds an explicit `host:port` to try (for networks without discovery) |

```ts
type Status = {
  me: { name: string, id: string, port: number, addresses: string[] }, // addresses: this machine's LAN IPs
  peers: Peer[],
  folders: Folder[],
};
type Peer = {
  name: string, id: string,
  online: boolean,
  address: string | null,        // "192.168.1.20:7878" when online (direct LAN connection)
  security_code: string | null,  // "MUFFIN THIMBLE GLOBE VELVET" for the live session, null when offline
};
type Folder = { label: string, path: string, files: number, bytes: number };
type Paired = { name: string, id: string, security_code: string };
```

## Events

All events arrive on one channel, `nebula://event`, as `{ kind: string, at: number, ...fields }`:

| `kind` | Fields | Meaning |
|---|---|---|
| `peer_online` | `name, id, address, security_code` | Encrypted session established |
| `peer_offline` | `name, id` | Session ended |
| `paired` | `name, id, security_code` | Pairing (started with `pair_start`) succeeded |
| `pair_failed` | `reason` | Wrong codes, expired, or cancelled |
| `local_change` | `folder, added, modified, deleted` (counts) | This device's scan found changes; they are being announced |
| `download_progress` | `folder, path, done, total, from` | Throttled (≤ 4 per second per file); only for files over 1 MiB |
| `file_received` | `folder, path, from, size, transferred` | A file arrived. `transferred` = bytes that crossed the network (smaller than `size` when chunks were reused) |
| `file_deleted` | `folder, path, from` | Deleted because the other device deleted it |
| `conflict` | `folder, path, copy` | Edited on both devices; this device's version was saved as `copy` |
| `refused` | `address, id` | An unpaired device tried to connect and was refused |
| `status_changed` | none | Something in `Status` changed; the frontend should call `status` again |

`from` is the peer's **name**.

## Mock mode

When `window.__TAURI__` is missing (the UI opened directly in a browser for development), the frontend uses
`ui/mock.js`, which implements the same commands and emits a scripted stream of events so every screen can
be exercised.
