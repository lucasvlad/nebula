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

use std::collections::{HashMap, HashSet};
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

struct PendingRequest {
    key: FileKey,
    index: usize,
}

pub struct Engine {
    store: Store,
    me: DeviceId,
    name: String,
    connected: HashSet<DeviceId>,
    /// What each connected peer has told us, by folder id and path.
    remote: HashMap<DeviceId, HashMap<i64, HashMap<RelPath, FileEntry>>>,
    downloads: HashMap<FileKey, Download>,
    requests: HashMap<u64, PendingRequest>,
    next_request: u64,
    /// Where each chunk hash can be read locally, per folder. Rebuilt lazily
    /// after the folder's manifest changes.
    locators: HashMap<i64, HashMap<Digest, (RelPath, u64, u32)>>,
    out: Vec<Output>,
}

impl Engine {
    /// `name` is this device's name, used to label conflict copies.
    pub fn new(store: Store, me: DeviceId, name: String) -> Result<Self> {
        Ok(Self {
            store,
            me,
            name,
            connected: HashSet::new(),
            remote: HashMap::new(),
            downloads: HashMap::new(),
            requests: HashMap::new(),
            next_request: 0,
            locators: HashMap::new(),
            out: Vec::new(),
        })
    }

    /// Folders being synced, for setting up the watcher.
    pub fn folders(&self) -> Result<Vec<Folder>> {
        Ok(self.store.folders()?)
    }

    /// Downloads not yet finished, for status display and tests.
    pub fn pending_downloads(&self) -> usize {
        self.downloads.len()
    }

    /// Processes one input and returns the messages to send, in order.
    /// Errors are logged and do not stop the engine.
    pub fn handle(&mut self, input: Input) -> Vec<Output> {
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
            self.send_index(peer, &folder.label, entries);
        }
        Ok(())
    }

    fn on_disconnected(&mut self, peer: DeviceId) {
        self.connected.remove(&peer);
        self.remote.remove(&peer);
        // Temp files stay on disk; the next download of the same content
        // picks up their verified prefix.
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
            Message::Chunk { id, data } => self.on_chunk(id, Some(data)),
            Message::Missing { id } => self.on_chunk(id, None),
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
            fresh.push(entry);
        }
        let mut cases = self.case_map(folder.id)?;
        for entry in fresh {
            self.consider(&folder, peer, entry, &mut cases)?;
        }
        self.pump_downloads()
    }

    /// Lowercased path -> actual path, for every live local file.
    fn case_map(&self, folder_id: i64) -> Result<HashMap<String, RelPath>> {
        Ok(self
            .store
            .files(folder_id)?
            .into_iter()
            .filter(|e| !e.deleted)
            .map(|e| (e.path.case_key(), e.path))
            .collect())
    }

    /// Decides what to do about one entry a peer announced.
    fn consider(
        &mut self,
        folder: &Folder,
        peer: DeviceId,
        theirs: FileEntry,
        cases: &mut HashMap<String, RelPath>,
    ) -> Result<()> {
        let key = (folder.id, theirs.path.clone());
        if let Some(d) = self.downloads.get(&key) {
            if d.want.version.compare(&theirs.version) != Causality::Before {
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
                if let Some(existing) = cases.get(&theirs.path.case_key())
                    && existing != &theirs.path
                {
                    warn!(
                        "{}: skipped; this device has \"{existing}\", which differs only in letter case",
                        theirs.path
                    );
                    return Ok(());
                }
                self.take(folder, peer, ours, theirs, false)
            }
            Resolution::Conflict {
                winner: Side::Remote,
            } => {
                info!(
                    "{}: edited on both devices; the other edit is newer, so this device's \
                     version is saved as a conflict copy",
                    theirs.path
                );
                self.take(folder, peer, ours, theirs, true)
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
        self.store
            .put_files(folder.id, std::slice::from_ref(&ours))?;
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
    ) -> Result<()> {
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
            self.store
                .put_files(folder.id, std::slice::from_ref(&entry))?;
            self.locators.remove(&folder.id);
            if entry.version != theirs.version {
                self.announce(folder, vec![entry]);
            }
            return Ok(());
        }

        let loser_copy = match (&ours, conflict) {
            (Some(o), true) => Some(self.free_copy_name(folder, o)?),
            _ => None,
        };
        self.start_download(folder.clone(), peer, ours, theirs, loser_copy)
    }

    fn start_download(
        &mut self,
        folder: Folder,
        from: DeviceId,
        base: Option<FileEntry>,
        want: FileEntry,
        loser_copy: Option<RelPath>,
    ) -> Result<()> {
        let Some(target) = safe_target(&folder.root, &want.path) else {
            return Ok(());
        };
        let dir = target.parent().unwrap_or(&folder.root).to_path_buf();
        fs::create_dir_all(&dir)?;
        let temp = dir.join(format!(
            "{TEMP_PREFIX}{}",
            &want.content_hash.to_string()[..16]
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

    fn on_chunk(&mut self, id: u64, data: Option<Vec<u8>>) -> Result<()> {
        let Some(req) = self.requests.remove(&id) else {
            return Ok(()); // Unknown or from a cancelled download.
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
                return Ok(());
            }
            None => {
                info!(
                    "{}: the other device no longer has this version; waiting for its update",
                    d.want.path
                );
                self.cancel_download(&req.key);
                return Ok(());
            }
        }
        self.pump_downloads()
    }

    /// Writes whatever chunks are ready, tops up requests, finishes files.
    fn pump_downloads(&mut self) -> Result<()> {
        let keys: Vec<FileKey> = self.downloads.keys().cloned().collect();
        for key in keys {
            if let Err(e) = self.pump(&key) {
                warn!("{}: download failed: {e}", key.1);
                self.cancel_download(&key);
            }
        }
        Ok(())
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
        self.store
            .put_files(d.folder.id, std::slice::from_ref(&entry))?;
        self.locators.remove(&d.folder.id);
        info!("{path}: updated from the other device");
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
        let before: HashMap<RelPath, FileEntry> = self
            .store
            .files(folder.id)?
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        let report = scanner::scan(&mut self.store, &folder, self.me)?;
        if !report.has_changes() {
            return Ok(());
        }
        self.locators.remove(&folder.id);
        let changed: Vec<FileEntry> = self
            .store
            .files(folder.id)?
            .into_iter()
            .filter(|e| {
                before
                    .get(&e.path)
                    .is_none_or(|b| b.version != e.version || b.deleted != e.deleted)
            })
            .collect();
        info!(
            "\"{}\": {} added, {} modified, {} deleted on this device",
            folder.label,
            report.added.len(),
            report.modified.len(),
            report.deleted.len()
        );
        self.announce(&folder, changed);
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
            let mut cases = self.case_map(folder.id)?;
            for (peer, entry) in offers {
                self.consider(&folder, peer, entry, &mut cases)?;
            }
        }
        self.pump_downloads()
    }

    fn announce(&mut self, folder: &Folder, entries: Vec<FileEntry>) {
        if entries.is_empty() {
            return;
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
            let len = 64 + e.path.as_str().len() + e.chunks.len() * 40;
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
        let mut name = conflict_copy_name(&ours.path, &label, ours.mtime_ns)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        for n in 2..100 {
            let taken = safe_target(&folder.root, &name).is_none_or(|p| p.exists());
            if !taken {
                break;
            }
            name = conflict_copy_name(&ours.path, &format!("{label} {n}"), ours.mtime_ns)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
        Ok(name)
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
