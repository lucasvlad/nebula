//! The sync engine: pure decision logic, driven by inputs and producing
//! messages to send. It owns the state database and does file IO inside its
//! synced folders, but knows nothing about sockets, threads or timers, so
//! two engines can be wired together in a test and checked for convergence.
//!
//! The protocol is symmetric. On connect each side sends its whole manifest
//! (`Index`) for every folder; after that it sends only entries that change.
//! For every entry a peer announces, version vectors decide (via
//! [`nebula_core::conflict::resolve`]) whether to keep ours, fetch theirs, or
//! handle a conflict. Fetching requests only chunks we don't already have
//! locally, verifies each against its hash, assembles the file in a temp file
//! next to its destination, verifies the whole-file hash and renames it into
//! place. A temp file left by an interrupted download is reused: its verified
//! prefix is kept, so only the remaining chunks cross the network.
//!
//! Every peer message is treated as hostile: unknown folders are ignored,
//! entries whose chunk list doesn't add up are dropped, paths are already
//! validated by [`RelPath`], paths that would pass through a symlink or
//! collide by case with a local file are skipped, and chunk requests are only
//! answered from the named folder's own manifest.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use nebula_core::chunker::{ChunkRef, RollingChunker};
use nebula_core::conflict::{Resolution, Side, conflict_copy_name, resolve};
use nebula_core::manifest::FileEntry;
use nebula_core::path::RelPath;
use nebula_core::version::Causality;
use nebula_core::{DeviceId, Digest};
use tracing::{debug, info, warn};

use crate::protocol::Message;
use crate::scanner::{self, ScanError, TEMP_PREFIX};
use crate::store::{Folder, Store, StoreError};

/// Chunk requests kept in flight per download. With 8 KiB average chunks
/// this is ~256 KiB in flight, enough to fill a LAN link.
const WINDOW: usize = 32;
/// Index messages are split so each stays well under the transport limit.
const INDEX_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Downloads with an open temp file at once. Each holds a file descriptor,
/// and macOS allows only 256 per process by default; more decisions wait
/// in a queue.
pub const MAX_ACTIVE_DOWNLOADS: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

type Result<T> = std::result::Result<T, EngineError>;

/// Something that happened, fed to [`Engine::handle`].
#[derive(Debug)]
pub enum Input {
    /// An authenticated session with a paired peer is up.
    Connected { peer: DeviceId },
    /// That session ended. Downloads from it pause until it reconnects.
    Disconnected { peer: DeviceId },
    /// A message arrived from a connected peer.
    Received { peer: DeviceId, msg: Message },
    /// Something changed on disk under this folder (from the watcher,
    /// already debounced). The engine rescans it.
    LocalChange { folder: i64 },
    /// Periodic fallback: rescan everything and retry stalled downloads.
    Tick,
}

/// A message the caller must send to `peer`.
#[derive(Debug, PartialEq, Eq)]
pub struct Output {
    pub peer: DeviceId,
    pub msg: Message,
}

type FileKey = (i64, RelPath);

/// The parts of an entry that matter for whether peers must hear about it.
#[derive(Clone, PartialEq, Eq)]
struct Known {
    version: nebula_core::version::VersionVector,
    deleted: bool,
}

impl Known {
    fn of(e: &FileEntry) -> Self {
        Self {
            version: e.version.clone(),
            deleted: e.deleted,
        }
    }
}

/// A file being assembled from local and remote chunks.
struct Download {
    folder: Folder,
    want: FileEntry,
    from: DeviceId,
    /// Our entry when the download was decided; the file on disk must still
    /// match it when we rename, or the user changed it meanwhile.
    base: Option<FileEntry>,
    /// Set for a conflict we lost: our current file moves here first.
    loser_copy: Option<RelPath>,
    temp: PathBuf,
    file: File,
    hasher: blake3::Hasher,
    /// Chunks written to the temp file so far, in order.
    written: usize,
    /// Next chunk index to consider requesting ahead.
    cursor: usize,
    inflight: usize,
    requested: HashSet<usize>,
    received: HashMap<usize, Vec<u8>>,
}

/// A download that has been decided but waits for a free slot. Its temp
/// file is opened only when it starts.
struct Queued {
    folder: Folder,
    from: DeviceId,
    base: Option<FileEntry>,
    want: FileEntry,
    loser_copy: Option<RelPath>,
}

struct PendingRequest {
    key: FileKey,
    index: usize,
}

/// Lowercased spelling -> (local spelling, live files using it) for every
/// live local file path and each of its directory prefixes. A remote path
/// that matches any of these case-insensitively but is spelled differently
/// would land inside the local file or directory on macOS or Windows.
#[derive(Default)]
struct Cases(HashMap<String, (String, usize)>);

impl Cases {
    fn add(&mut self, path: &RelPath) {
        for p in prefixes(path.as_str()) {
            let e = self
                .0
                .entry(p.to_lowercase())
                .or_insert_with(|| (p.to_string(), 0));
            if e.0 == p {
                e.1 += 1;
            }
        }
    }

    fn remove(&mut self, path: &RelPath) {
        for p in prefixes(path.as_str()) {
            let k = p.to_lowercase();
            if let Some(e) = self.0.get_mut(&k)
                && e.0 == p
            {
                e.1 -= 1;
                if e.1 == 0 {
                    self.0.remove(&k);
                }
            }
        }
    }

    /// The differently spelled local file or directory `path` would
    /// collide with, if any.
    fn clash(&self, path: &RelPath) -> Option<&str> {
        prefixes(path.as_str()).find_map(|p| {
            self.0
                .get(&p.to_lowercase())
                .filter(|(spelling, _)| spelling != p)
                .map(|(spelling, _)| spelling.as_str())
        })
    }
}

/// Each directory prefix of `path`, then the full path.
fn prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(|(i, _)| &path[..i])
        .chain(std::iter::once(path))
}

pub struct Engine {
    store: Store,
    me: DeviceId,
    name: String,
    connected: HashSet<DeviceId>,
    /// What each connected peer has told us, by folder id and path.
    remote: HashMap<DeviceId, HashMap<i64, HashMap<RelPath, FileEntry>>>,
    /// Active downloads, at most [`MAX_ACTIVE_DOWNLOADS`].
    downloads: HashMap<FileKey, Download>,
    /// Decided downloads waiting for a slot, started in `queue` order. A
    /// key in `queue` but not `queued` was cancelled while waiting.
    queued: HashMap<FileKey, Queued>,
    queue: VecDeque<FileKey>,
    requests: HashMap<u64, PendingRequest>,
    next_request: u64,
    /// Where each chunk hash can be read locally, per folder. Rebuilt lazily
    /// after the folder's manifest changes.
    locators: HashMap<i64, HashMap<Digest, (RelPath, u64, u32)>>,
    /// Per folder, the version of each entry our peers have been told about
    /// (or already had). A rescan announces whatever differs from this, so
    /// changes recorded by another process (`nebula scan`) are not missed.
    known: HashMap<i64, HashMap<RelPath, Known>>,
    /// Label and root of each folder as last seen. Folders are added and
    /// removed by another process (`nebula add` / `remove`), and SQLite
    /// reuses a removed folder's id, so every cache above is keyed by an id
    /// that may later name a different folder.
    folders_seen: HashMap<i64, (String, PathBuf)>,
    out: Vec<Output>,
}

impl Engine {
    /// `name` is this device's name, used to label conflict copies.
    pub fn new(store: Store, me: DeviceId, name: String) -> Result<Self> {
        let folders_seen = folder_ids(&store.folders()?);
        Ok(Self {
            store,
            me,
            name,
            connected: HashSet::new(),
            remote: HashMap::new(),
            downloads: HashMap::new(),
            queued: HashMap::new(),
            queue: VecDeque::new(),
            requests: HashMap::new(),
            next_request: 0,
            locators: HashMap::new(),
            known: HashMap::new(),
            folders_seen,
            out: Vec::new(),
        })
    }

    /// Folders being synced, for setting up the watcher.
    pub fn folders(&self) -> Result<Vec<Folder>> {
        Ok(self.store.folders()?)
    }

    /// Downloads not yet finished, for status display and tests.
    pub fn pending_downloads(&self) -> usize {
        self.downloads.len() + self.queued.len()
    }

    /// Downloads with an open temp file; never more than
    /// [`MAX_ACTIVE_DOWNLOADS`].
    pub fn active_downloads(&self) -> usize {
        self.downloads.len()
    }

    /// Processes one input and returns the messages to send, in order.
    /// Errors are logged and do not stop the engine.
    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        // Chunk traffic is frequent and never depends on the folder list,
        // so only the other inputs check it for changes.
        let refresh = match &input {
            Input::Received { msg, .. } => {
                matches!(msg, Message::Index { .. } | Message::Resend { .. })
            }
            Input::Disconnected { .. } => false,
            Input::Connected { .. } | Input::LocalChange { .. } | Input::Tick => true,
        };
        if refresh && let Err(e) = self.refresh_folders() {
            warn!("could not read the folder list: {e}");
        }
        let result = match input {
            Input::Connected { peer } => self.on_connected(peer),
            Input::Disconnected { peer } => {
                self.on_disconnected(peer);
                Ok(())
            }
            Input::Received { peer, msg } => self.on_message(peer, msg),
            Input::LocalChange { folder } => self.rescan(folder),
            Input::Tick => self.tick(),
        };
        if let Err(e) = result {
            warn!("sync error: {e}");
        }
        std::mem::take(&mut self.out)
    }

    fn on_connected(&mut self, peer: DeviceId) -> Result<()> {
        self.connected.insert(peer);
        self.remote.insert(peer, HashMap::new());
        for folder in self.store.folders()? {
            let entries = self.store.files(folder.id)?;
            debug!(
                "sending {} entries of \"{}\" to {}",
                entries.len(),
                folder.label,
                peer.short()
            );
            let snapshot = entries
                .iter()
                .map(|e| (e.path.clone(), Known::of(e)))
                .collect();
            self.send_index(peer, &folder.label, entries);
            // The peer now has everything as it stands.
            self.known.insert(folder.id, snapshot);
        }
        Ok(())
    }

    /// Notices folders added, removed or replaced since the last call. A
    /// vanished or reused id drops everything cached under it; a new folder
    /// is offered to connected peers right away, with a request for theirs.
    fn refresh_folders(&mut self) -> Result<()> {
        let folders = self.store.folders()?;
        let current = folder_ids(&folders);
        let stale: Vec<i64> = self
            .folders_seen
            .iter()
            .filter(|(id, seen)| current.get(id) != Some(seen))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.forget_folder(id);
        }
        for folder in &folders {
            if !self.folders_seen.contains_key(&folder.id)
                && let Err(e) = self.folder_added(folder)
            {
                warn!("could not offer new folder \"{}\": {e}", folder.label);
            }
        }
        self.folders_seen = current;
        Ok(())
    }

    fn forget_folder(&mut self, id: i64) {
        if let Some((label, _)) = self.folders_seen.remove(&id) {
            debug!("folder \"{label}\" is no longer synced; dropping its state");
        }
        for folders in self.remote.values_mut() {
            folders.remove(&id);
        }
        self.locators.remove(&id);
        self.known.remove(&id);
        let keys: Vec<FileKey> = self
            .downloads
            .keys()
            .chain(self.queued.keys())
            .filter(|k| k.0 == id)
            .cloned()
            .collect();
        for key in keys {
            self.cancel_download(&key);
        }
    }

    /// Peers discarded our Index of this folder at connect time if it did
    /// not exist yet (and we discarded theirs), so both sides resend.
    fn folder_added(&mut self, folder: &Folder) -> Result<()> {
        let entries = self.store.files(folder.id)?;
        let snapshot = entries
            .iter()
            .map(|e| (e.path.clone(), Known::of(e)))
            .collect();
        let peers: Vec<DeviceId> = self.connected.iter().copied().collect();
        for peer in peers {
            info!(
                "now syncing \"{}\"; asking {} for its files",
                folder.label,
                peer.short()
            );
            self.send_index(peer, &folder.label, entries.clone());
            self.out.push(Output {
                peer,
                msg: Message::Resend {
                    folder: folder.label.clone(),
                },
            });
        }
        self.known.insert(folder.id, snapshot);
        Ok(())
    }

    fn on_disconnected(&mut self, peer: DeviceId) {
        self.connected.remove(&peer);
        self.remote.remove(&peer);
        // Temp files stay on disk; the next download of the same content
        // picks up their verified prefix.
        self.queued.retain(|_, q| q.from != peer);
        let stalled: Vec<FileKey> = self
            .downloads
            .iter()
            .filter(|(_, d)| d.from == peer)
            .map(|(k, _)| k.clone())
            .collect();
        for key in stalled {
            if let Some(d) = self.downloads.remove(&key) {
                info!(
                    "{}: paused at {} of {} chunks; will resume when the device reconnects",
                    d.want.path,
                    d.written,
                    d.want.chunks.len()
                );
            }
        }
        self.requests
            .retain(|_, r| self.downloads.contains_key(&r.key));
    }

    fn on_message(&mut self, peer: DeviceId, msg: Message) -> Result<()> {
        if !self.connected.contains(&peer) {
            return Ok(());
        }
        match msg {
            Message::Index { folder, entries } => self.on_index(peer, &folder, entries),
            Message::Request { id, folder, hash } => {
                let reply = match self.read_local_chunk(&folder, &hash) {
                    Some(data) => Message::Chunk { id, data },
                    None => Message::Missing { id },
                };
                self.out.push(Output { peer, msg: reply });
                Ok(())
            }
            Message::Chunk { id, data } => self.on_chunk(peer, id, Some(data)),
            Message::Missing { id } => self.on_chunk(peer, id, None),
            Message::Resend { folder } => {
                // Only a folder we sync under that label is ever sent.
                if let Some(folder) = self.store.folder_by_label(&folder)? {
                    let entries = self.store.files(folder.id)?;
                    self.send_index(peer, &folder.label, entries);
                }
                Ok(())
            }
            // Session-level messages are handled by the caller.
            Message::Hello { .. }
            | Message::Ping { .. }
            | Message::Pong { .. }
            | Message::Note { .. } => Ok(()),
        }
    }

    fn on_index(&mut self, peer: DeviceId, label: &str, entries: Vec<FileEntry>) -> Result<()> {
        let Some(folder) = self.store.folder_by_label(label)? else {
            debug!(
                "{} offered folder \"{label}\", which is not synced here",
                peer.short()
            );
            return Ok(());
        };
        let known = self
            .remote
            .entry(peer)
            .or_default()
            .entry(folder.id)
            .or_default();
        let mut fresh = Vec::new();
        for entry in entries {
            if let Err(why) = check_entry(&entry) {
                warn!("ignoring {} from {}: {why}", entry.path, peer.short());
                continue;
            }
            known.insert(entry.path.clone(), entry.clone());
            fresh.push((peer, entry));
        }
        let mut cases = self.case_map(folder.id)?;
        self.consider_all(&folder, &mut cases, fresh);
        self.pump_downloads()
    }

    /// Considers each offer, logging failures instead of giving up on the
    /// rest: one unwritable file must not stall every other one forever.
    fn consider_all(
        &mut self,
        folder: &Folder,
        cases: &mut Cases,
        mut offers: Vec<(DeviceId, FileEntry)>,
    ) {
        // Tombstones first. A case-only rename arrives as the old name's
        // tombstone plus the new name, and the new name would otherwise be
        // skipped as a case clash with the file about to be deleted.
        offers.sort_by_key(|(_, e)| !e.deleted);
        for (peer, entry) in offers {
            let path = entry.path.clone();
            if let Err(e) = self.consider(folder, peer, entry, cases) {
                warn!("{path}: {e}");
            }
        }
    }

    /// Every live local file path and directory prefix, by case.
    fn case_map(&self, folder_id: i64) -> Result<Cases> {
        let mut cases = Cases::default();
        for e in self.store.files(folder_id)? {
            if !e.deleted {
                cases.add(&e.path);
            }
        }
        Ok(cases)
    }

    /// Decides what to do about one entry a peer announced.
    fn consider(
        &mut self,
        folder: &Folder,
        peer: DeviceId,
        theirs: FileEntry,
        cases: &mut Cases,
    ) -> Result<()> {
        let key = (folder.id, theirs.path.clone());
        let pending = self
            .downloads
            .get(&key)
            .map(|d| &d.want)
            .or_else(|| self.queued.get(&key).map(|q| &q.want));
        if let Some(want) = pending {
            if want.version.compare(&theirs.version) != Causality::Before {
                return Ok(()); // Already fetching this or something newer.
            }
            self.cancel_download(&key);
        }
        let ours = self.store.file(folder.id, &theirs.path)?;
        let resolution = match &ours {
            Some(ours) => resolve(ours, &theirs),
            None => Resolution::TakeRemote,
        };
        match resolution {
            Resolution::KeepLocal => {
                if let Some(ours) = ours {
                    self.merge_version(folder, ours, &theirs)?;
                }
                Ok(())
            }
            Resolution::Conflict {
                winner: Side::Local,
            } => {
                // The other device lost, so it saves its version as a
                // conflict copy and then announces a version covering both.
                // Merging here first would let that announcement overwrite
                // its edit before the copy exists.
                debug!(
                    "{}: edited on both devices; this device's edit is newer and keeps the name",
                    theirs.path
                );
                Ok(())
            }
            Resolution::TakeRemote => {
                if let Some(existing) = cases.clash(&theirs.path) {
                    warn!(
                        "{}: skipped; this device has \"{existing}\", which differs only in letter case",
                        theirs.path
                    );
                    return Ok(());
                }
                self.take(folder, peer, ours, theirs, false, cases)
            }
            Resolution::Conflict {
                winner: Side::Remote,
            } => {
                info!(
                    "{}: edited on both devices; the other edit is newer, so this device's \
                     version is saved as a conflict copy",
                    theirs.path
                );
                self.take(folder, peer, ours, theirs, true, cases)
            }
        }
    }

    /// Our version stays, but it now also covers everything theirs has seen,
    /// so it dominates and the peer fast-forwards to it.
    fn merge_version(
        &mut self,
        folder: &Folder,
        mut ours: FileEntry,
        theirs: &FileEntry,
    ) -> Result<()> {
        let merged = ours.version.merged(&theirs.version);
        if merged == ours.version {
            return Ok(());
        }
        ours.version = merged;
        self.put(folder.id, &ours)?;
        self.announce(folder, vec![ours]);
        Ok(())
    }

    fn take(
        &mut self,
        folder: &Folder,
        peer: DeviceId,
        ours: Option<FileEntry>,
        theirs: FileEntry,
        conflict: bool,
        cases: &mut Cases,
    ) -> Result<()> {
        // Whether `cases` counts this path now; it must stay accurate for the
        // rest of the batch (see `consider_all`).
        let live = ours.as_ref().is_some_and(|o| !o.deleted);
        let Some(target) = safe_target(&folder.root, &theirs.path) else {
            warn!(
                "{}: skipped; the path passes through a link or a file",
                theirs.path
            );
            return Ok(());
        };
        if theirs.deleted {
            // Never delete something our manifest doesn't account for, such
            // as a file created here since the last scan.
            if !self.unchanged_on_disk(&target, ours.as_ref())? {
                return self.rescan(folder.id);
            }
            if target.exists() {
                fs::remove_file(&target)?;
                info!(
                    "{}: deleted, because it was deleted on the other device",
                    theirs.path
                );
            }
            let mut entry = theirs.clone();
            if let Some(o) = &ours {
                entry.version = o.version.merged(&entry.version);
            }
            self.put(folder.id, &entry)?;
            if live {
                cases.remove(&theirs.path);
            }
            if entry.version != theirs.version {
                self.announce(folder, vec![entry]);
            }
            return Ok(());
        }

        let loser_copy = match (&ours, conflict) {
            (Some(o), true) => Some(self.free_copy_name(folder, o)?),
            _ => None,
        };
        if !live {
            cases.add(&theirs.path);
        }
        let key = (folder.id, theirs.path.clone());
        if !self.queued.contains_key(&key) {
            self.queue.push_back(key.clone());
        }
        self.queued.insert(
            key,
            Queued {
                folder: folder.clone(),
                from: peer,
                base: ours,
                want: theirs,
                loser_copy,
            },
        );
        Ok(())
    }

    /// Starts queued downloads while there are free slots.
    fn start_queued(&mut self) {
        while self.downloads.len() < MAX_ACTIVE_DOWNLOADS {
            let Some(key) = self.queue.pop_front() else {
                return;
            };
            let Some(q) = self.queued.remove(&key) else {
                continue; // Cancelled while waiting.
            };
            let path = q.want.path.clone();
            if let Err(e) = self.start_download(q) {
                warn!("{path}: could not start the download: {e}");
            }
        }
    }

    fn start_download(&mut self, q: Queued) -> Result<()> {
        let Queued {
            folder,
            from,
            base,
            want,
            loser_copy,
        } = q;
        let Some(target) = safe_target(&folder.root, &want.path) else {
            return Ok(());
        };
        let dir = target.parent().unwrap_or(&folder.root).to_path_buf();
        fs::create_dir_all(&dir)?;
        // Named by content so an interrupted download resumes, and by path
        // so identical files in one directory don't share a temp file.
        let temp = dir.join(format!(
            "{TEMP_PREFIX}{}-{}",
            &want.content_hash.to_string()[..16],
            &Digest::of(want.path.as_str().as_bytes()).to_string()[..8]
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&temp)?;

        // Keep whatever prefix of an earlier attempt still verifies.
        let mut hasher = blake3::Hasher::new();
        let mut written = 0;
        let mut offset = 0u64;
        let mut buf = Vec::new();
        for c in &want.chunks {
            buf.resize(c.len as usize, 0);
            if file.read_exact(&mut buf).is_err() || Digest::of(&buf) != c.hash {
                break;
            }
            hasher.update(&buf);
            written += 1;
            offset += u64::from(c.len);
        }
        file.set_len(offset)?;
        file.seek(SeekFrom::Start(offset))?;
        if written > 0 {
            info!(
                "{}: resuming; {} of {} chunks were already here from an earlier attempt",
                want.path,
                written,
                want.chunks.len()
            );
        }

        let key = (folder.id, want.path.clone());
        self.downloads.insert(
            key,
            Download {
                folder,
                want,
                from,
                base,
                loser_copy,
                temp,
                file,
                hasher,
                written,
                cursor: written,
                inflight: 0,
                requested: HashSet::new(),
                received: HashMap::new(),
            },
        );
        Ok(())
    }

    fn on_chunk(&mut self, peer: DeviceId, id: u64, data: Option<Vec<u8>>) -> Result<()> {
        let Some(key) = self.requests.get(&id).map(|r| r.key.clone()) else {
            return Ok(()); // Unknown or from a cancelled download.
        };
        // Only the device we asked may answer. Request ids are sequential,
        // so another peer could otherwise cancel the download by guessing
        // one and replying Missing.
        if self.downloads.get(&key).is_some_and(|d| d.from != peer) {
            debug!("{}: ignored a reply from {}", key.1, peer.short());
            return Ok(());
        }
        let Some(req) = self.requests.remove(&id) else {
            return Ok(());
        };
        let Some(d) = self.downloads.get_mut(&req.key) else {
            return Ok(());
        };
        d.inflight = d.inflight.saturating_sub(1);
        let expected = d.want.chunks[req.index];
        match data {
            Some(data)
                if data.len() == expected.len as usize && Digest::of(&data) == expected.hash =>
            {
                d.received.insert(req.index, data);
            }
            Some(_) => {
                warn!(
                    "{}: received a chunk that does not match its hash; dropping this download",
                    d.want.path
                );
                self.cancel_download(&req.key);
            }
            None => {
                info!(
                    "{}: the other device no longer has this version; waiting for its update",
                    d.want.path
                );
                self.cancel_download(&req.key);
            }
        }
        // Also after a cancel, so a queued download takes the free slot.
        self.pump_downloads()
    }

    /// Starts queued downloads as slots free up, writes whatever chunks are
    /// ready, tops up requests and finishes files.
    fn pump_downloads(&mut self) -> Result<()> {
        loop {
            self.start_queued();
            let keys: Vec<FileKey> = self.downloads.keys().cloned().collect();
            for key in keys {
                if let Err(e) = self.pump(&key) {
                    warn!("{}: download failed: {e}", key.1);
                    self.cancel_download(&key);
                }
            }
            // Downloads that completed from local chunks free their slot
            // without any message arriving, so refill until none do.
            if self.queued.is_empty() || self.downloads.len() >= MAX_ACTIVE_DOWNLOADS {
                return Ok(());
            }
        }
    }

    fn pump(&mut self, key: &FileKey) -> Result<()> {
        let folder_id = key.0;
        // Write chunks in order as they become available.
        loop {
            let Some(d) = self.downloads.get_mut(key) else {
                return Ok(());
            };
            if d.written == d.want.chunks.len() {
                return self.finish(key);
            }
            let index = d.written;
            let chunk = d.want.chunks[index];
            let data = match d.received.remove(&index) {
                Some(data) => Some(data),
                None if d.requested.contains(&index) => None, // On its way.
                None => self.copy_local(folder_id, &chunk),
            };
            let Some(data) = data else {
                let Some(d) = self.downloads.get(key) else {
                    return Ok(());
                };
                if !d.requested.contains(&index) {
                    self.request(key, index);
                }
                break;
            };
            let Some(d) = self.downloads.get_mut(key) else {
                return Ok(());
            };
            d.file.write_all(&data)?;
            d.hasher.update(&data);
            d.written += 1;
            d.cursor = d.cursor.max(d.written);
        }

        // Keep a window of requests in flight, skipping chunks we can copy
        // from files already here.
        loop {
            let Some(d) = self.downloads.get_mut(key) else {
                return Ok(());
            };
            if d.inflight >= WINDOW || d.cursor >= d.want.chunks.len() {
                return Ok(());
            }
            let index = d.cursor;
            d.cursor += 1;
            if d.requested.contains(&index) {
                continue;
            }
            let hash = d.want.chunks[index].hash;
            if !self.has_local(folder_id, &hash) {
                self.request(key, index);
            }
        }
    }

    fn request(&mut self, key: &FileKey, index: usize) {
        let Some(d) = self.downloads.get_mut(key) else {
            return;
        };
        d.requested.insert(index);
        d.inflight += 1;
        let id = self.next_request;
        self.next_request += 1;
        self.out.push(Output {
            peer: d.from,
            msg: Message::Request {
                id,
                folder: d.folder.label.clone(),
                hash: d.want.chunks[index].hash,
            },
        });
        self.requests.insert(
            id,
            PendingRequest {
                key: key.clone(),
                index,
            },
        );
    }

    /// Verifies the assembled file and moves it into place.
    fn finish(&mut self, key: &FileKey) -> Result<()> {
        let Some(mut d) = self.downloads.remove(key) else {
            return Ok(());
        };
        self.requests.retain(|_, r| &r.key != key);
        let path = d.want.path.clone();
        if Digest::from(d.hasher.finalize()) != d.want.content_hash {
            warn!("{path}: assembled file does not match its hash; discarding it");
            drop(d.file);
            let _ = fs::remove_file(&d.temp);
            return Ok(());
        }
        // Chunk messages don't refresh the folder list, so the folder may
        // have been removed (or its id reused) since the last check.
        let still_synced = self
            .store
            .folders()?
            .iter()
            .any(|f| f.id == d.folder.id && f.root == d.folder.root);
        if !still_synced {
            info!("{path}: its folder is no longer synced; discarding the download");
            drop(d.file);
            let _ = fs::remove_file(&d.temp);
            return Ok(());
        }
        let Some(target) = safe_target(&d.folder.root, &path) else {
            return Ok(());
        };
        if !self.unchanged_on_disk(&target, d.base.as_ref())? {
            info!("{path}: changed on this device during the download; rescanning before deciding");
            return self.rescan(d.folder.id);
        }

        d.file.flush()?;
        d.file
            .set_modified(UNIX_EPOCH + duration_ns(d.want.mtime_ns))?;
        d.file.sync_all()?;
        drop(d.file);

        if let Some(copy) = &d.loser_copy
            && let Some(copy_path) = safe_target(&d.folder.root, copy)
            && target.exists()
        {
            // The name was free when the download was decided; never let
            // the rename replace a file that has appeared there since.
            if copy_path.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("conflict copy \"{copy}\" already exists"),
                )
                .into());
            }
            fs::rename(&target, &copy_path)?;
            info!("{path}: this device's version was kept as \"{copy}\"");
        }
        fs::rename(&d.temp, &target)?;

        // Record what is now on disk, with the stat the scanner will see.
        let meta = fs::metadata(&target)?;
        let mut entry = d.want.clone();
        entry.mtime_ns = mtime_ns(&meta);
        if let Some(base) = &d.base {
            entry.version = base.version.merged(&entry.version);
        }
        self.put(d.folder.id, &entry)?;
        if d.base
            .as_ref()
            .is_some_and(|b| !b.deleted && b.content_hash == entry.content_hash)
        {
            debug!("{path}: already identical; recorded the other device's version history");
        } else {
            info!("{path}: updated from the other device");
        }
        if d.loser_copy.is_some() {
            // Index the conflict copy so it syncs too.
            self.rescan(d.folder.id)?;
        }
        // If we merged in history the sender lacks (our losing edit, or a
        // delete its edit beat), tell it, so both sides end up identical.
        if entry.version != d.want.version {
            self.announce(&d.folder, vec![entry]);
        }
        Ok(())
    }

    fn cancel_download(&mut self, key: &FileKey) {
        self.downloads.remove(key);
        self.queued.remove(key);
        self.requests.retain(|_, r| &r.key != key);
    }

    /// Scans a folder and announces what changed.
    fn rescan(&mut self, folder_id: i64) -> Result<()> {
        let Some(folder) = self
            .store
            .folders()?
            .into_iter()
            .find(|f| f.id == folder_id)
        else {
            return Ok(());
        };
        if !self.known.contains_key(&folder.id) {
            // First scan of this folder while running: what is stored now
            // was sent in full when each peer connected.
            let seed = self.snapshot(folder.id)?;
            self.known.insert(folder.id, seed);
        }
        let report = scanner::scan(&mut self.store, &folder, self.me)?;
        let known = self.known.get(&folder.id);
        let changed: Vec<FileEntry> = self
            .store
            .files(folder.id)?
            .into_iter()
            .filter(|e| known.and_then(|k| k.get(&e.path)) != Some(&Known::of(e)))
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        self.locators.remove(&folder.id);
        if report.has_changes() {
            info!(
                "\"{}\": {} added, {} modified, {} deleted on this device",
                folder.label,
                report.added.len(),
                report.modified.len(),
                report.deleted.len()
            );
        } else {
            info!(
                "\"{}\": {} change(s) recorded by `nebula scan`; telling the other devices",
                folder.label,
                changed.len()
            );
        }
        self.announce(&folder, changed);
        Ok(())
    }

    fn snapshot(&self, folder_id: i64) -> Result<HashMap<RelPath, Known>> {
        Ok(self
            .store
            .files(folder_id)?
            .into_iter()
            .map(|e| (e.path.clone(), Known::of(&e)))
            .collect())
    }

    /// Stores an entry learned from or settled with a peer. Peers already
    /// have (or are told about) this version, so it is recorded as known.
    fn put(&mut self, folder_id: i64, entry: &FileEntry) -> Result<()> {
        self.store
            .put_files(folder_id, std::slice::from_ref(entry))?;
        self.locators.remove(&folder_id);
        if let Some(k) = self.known.get_mut(&folder_id) {
            k.insert(entry.path.clone(), Known::of(entry));
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        for folder in self.store.folders()? {
            if let Err(e) = self.rescan(folder.id) {
                warn!("could not scan \"{}\": {e}", folder.label);
            }
            // Retry anything a peer offered that we haven't applied, e.g.
            // after a download was abandoned because the file was busy.
            let offers: Vec<(DeviceId, FileEntry)> = self
                .remote
                .iter()
                .flat_map(|(peer, folders)| {
                    folders
                        .get(&folder.id)
                        .into_iter()
                        .flat_map(|m| m.values())
                        .map(|e| (*peer, e.clone()))
                })
                .collect();
            match self.case_map(folder.id) {
                Ok(mut cases) => self.consider_all(&folder, &mut cases, offers),
                Err(e) => warn!("could not read \"{}\": {e}", folder.label),
            }
        }
        self.pump_downloads()
    }

    fn announce(&mut self, folder: &Folder, entries: Vec<FileEntry>) {
        if entries.is_empty() {
            return;
        }
        let known = self.known.entry(folder.id).or_default();
        for e in &entries {
            known.insert(e.path.clone(), Known::of(e));
        }
        let peers: Vec<DeviceId> = self.connected.iter().copied().collect();
        for peer in peers {
            self.send_index(peer, &folder.label, entries.clone());
        }
    }

    /// Sends entries in batches that stay under the message size limit.
    fn send_index(&mut self, peer: DeviceId, label: &str, entries: Vec<FileEntry>) {
        let mut batch = Vec::new();
        let mut size = 0;
        for e in entries {
            // Measured rather than estimated: version vectors grow with
            // every device that ever touched a file, and an oversized
            // message is dropped by the transport.
            let len = postcard::to_stdvec(&e).map_or(INDEX_BATCH_BYTES, |v| v.len());
            if !batch.is_empty() && size + len > INDEX_BATCH_BYTES {
                self.out.push(Output {
                    peer,
                    msg: Message::Index {
                        folder: label.to_string(),
                        entries: std::mem::take(&mut batch),
                    },
                });
                size = 0;
            }
            size += len;
            batch.push(e);
        }
        self.out.push(Output {
            peer,
            msg: Message::Index {
                folder: label.to_string(),
                entries: batch,
            },
        });
    }

    /// Whether the file on disk is still what our manifest says it is, so
    /// replacing or deleting it cannot destroy an edit we haven't seen.
    fn unchanged_on_disk(&self, target: &Path, base: Option<&FileEntry>) -> Result<bool> {
        let live = base.filter(|b| !b.deleted);
        match (fs::symlink_metadata(target), live) {
            (Err(e), None) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            (Err(e), Some(_)) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            (Err(e), _) => Err(e.into()),
            (Ok(_), None) => Ok(false),
            (Ok(m), Some(b)) => Ok(m.is_file() && m.len() == b.size && mtime_ns(&m) == b.mtime_ns),
        }
    }

    /// A conflict copy name for our losing version that is not already taken.
    fn free_copy_name(&self, folder: &Folder, ours: &FileEntry) -> Result<RelPath> {
        let label = if ours.modified_by == self.me {
            self.name.clone()
        } else {
            self.store
                .peer(&ours.modified_by)?
                .map_or_else(|| ours.modified_by.short(), |p| p.name)
        };
        for n in 1..100 {
            let tag = if n == 1 {
                label.clone()
            } else {
                format!("{label} {n}")
            };
            let name = conflict_copy_name(&ours.path, &tag, ours.mtime_ns)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            if safe_target(&folder.root, &name).is_some_and(|p| !p.exists()) {
                return Ok(name);
            }
        }
        // Renaming onto a taken name would destroy that file.
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{}: no free conflict copy name", ours.path),
        )
        .into())
    }

    fn locator(&mut self, folder_id: i64) -> &HashMap<Digest, (RelPath, u64, u32)> {
        if !self.locators.contains_key(&folder_id) {
            let mut map = HashMap::new();
            for e in self.store.files(folder_id).unwrap_or_default() {
                if e.deleted {
                    continue;
                }
                let mut offset = 0u64;
                for c in &e.chunks {
                    map.entry(c.hash).or_insert((e.path.clone(), offset, c.len));
                    offset += u64::from(c.len);
                }
            }
            self.locators.insert(folder_id, map);
        }
        self.locators.entry(folder_id).or_default()
    }

    fn has_local(&mut self, folder_id: i64, hash: &Digest) -> bool {
        self.locator(folder_id).contains_key(hash)
    }

    /// Reads a chunk we already have, checking it is still intact.
    fn copy_local(&mut self, folder_id: i64, chunk: &ChunkRef) -> Option<Vec<u8>> {
        let (path, offset, len) = self.locator(folder_id).get(&chunk.hash)?.clone();
        let root = self
            .store
            .folders()
            .ok()?
            .into_iter()
            .find(|f| f.id == folder_id)?
            .root;
        let data = read_at(&safe_target(&root, &path)?, offset, len).ok()?;
        if Digest::of(&data) == chunk.hash {
            Some(data)
        } else {
            // The file changed since we indexed it.
            self.locators.remove(&folder_id);
            None
        }
    }

    /// Serves a peer's chunk request, only from the folder it names.
    fn read_local_chunk(&mut self, label: &str, hash: &Digest) -> Option<Vec<u8>> {
        let folder = self.store.folder_by_label(label).ok()??;
        // The length comes from our own manifest, not the request.
        self.copy_local(
            folder.id,
            &ChunkRef {
                hash: *hash,
                len: 0,
            },
        )
    }
}

fn folder_ids(folders: &[Folder]) -> HashMap<i64, (String, PathBuf)> {
    folders
        .iter()
        .map(|f| (f.id, (f.label.clone(), f.root.clone())))
        .collect()
}

/// Rejects entries that are internally inconsistent.
fn check_entry(e: &FileEntry) -> std::result::Result<(), &'static str> {
    if e.deleted {
        return if e.chunks.is_empty() {
            Ok(())
        } else {
            Err("a deleted file cannot have chunks")
        };
    }
    let max = RollingChunker::DEFAULT_MAX as u64;
    let mut total = 0u64;
    for c in &e.chunks {
        if c.len == 0 || u64::from(c.len) > max {
            return Err("chunk size out of range");
        }
        total += u64::from(c.len);
    }
    if total != e.size {
        return Err("chunk sizes do not add up to the file size");
    }
    Ok(())
}

/// The on-disk path for `rel`, or `None` if any existing component is a
/// symlink or a non-directory where a directory is needed.
fn safe_target(root: &Path, rel: &RelPath) -> Option<PathBuf> {
    let parts: Vec<&str> = rel.components().collect();
    let mut p = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        p.push(part);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return None,
            Ok(m) if i + 1 < parts.len() && !m.is_dir() => return None,
            _ => {}
        }
    }
    Some(p)
}

fn read_at(path: &Path, offset: u64, len: u32) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn duration_ns(ns: i64) -> Duration {
    Duration::from_nanos(u64::try_from(ns).unwrap_or(0))
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}
