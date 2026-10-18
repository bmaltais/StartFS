//! Content-addressed chunk store on local disk.
//!
//! A chunk is addressed by the BLAKE3 hash of its plaintext. On disk it is a
//! small self-describing envelope followed by the payload:
//!
//! ```text
//! byte 0     envelope format version (1)
//! byte 1     cipher id (0 = none; reserved for at-rest encryption)
//! byte 2     codec id  (0 = raw, 1 = zstd)
//! byte 3     reserved (0)
//! bytes 4..8 plaintext length, u32 little endian
//! bytes 8..  payload (codec applied first, then cipher once it exists)
//! ```
//!
//! The header can serve as AEAD associated data later, and the cipher byte
//! lets encrypted and plain chunks coexist without a migration.

use crate::ops::{FsError, FsResult, StoreStats};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub type ChunkId = [u8; 32];

pub const HEADER_LEN: usize = 8;
const FORMAT_VERSION: u8 = 1;
const CIPHER_NONE: u8 = 0;
const CODEC_RAW: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const ZSTD_LEVEL: i32 = 3;

/// The plaintext length from an envelope's header (at least [`HEADER_LEN`] bytes).
fn plain_len(envelope: &[u8]) -> u32 {
    u32::from_le_bytes(envelope[4..8].try_into().unwrap())
}

fn io(e: std::io::Error) -> FsError {
    // A full disk is the one failure the user can act on: report it as ENOSPC.
    if e.raw_os_error() == Some(libc::ENOSPC) {
        return FsError::NoSpace;
    }
    FsError::Io(e.to_string())
}

/// A stored chunk as its envelope describes it.
#[derive(Debug, Clone, Copy)]
pub struct ChunkInfo {
    pub disk_bytes: u64,
    pub plain_bytes: u64,
    /// Compressed with zstd; otherwise stored as is.
    pub zstd: bool,
}

pub struct ChunkStore {
    root: PathBuf,
    /// Chunk count and bytes on disk, kept up to date by `put` and `remove`
    /// so that `stats` never walks the store.
    chunks: AtomicU64,
    bytes: AtomicU64,
    /// Held while a finished temporary file is moved into place, so a chunk
    /// stored twice at once is counted once.
    publish: Mutex<()>,
    /// Makes each temporary file name unique.
    tmp: AtomicU64,
    /// Chunks added since the last [`Self::take_added`], when recording.
    added: Option<Mutex<Vec<ChunkId>>>,
    /// Chunks stored with [`Self::put_lazy`] that may not be on disk yet.
    unsynced: Mutex<HashSet<ChunkId>>,
}

/// Chunk files written within this long before the dirty marker may not have
/// reached disk; coarse file timestamps make the margin necessary.
const DIRTY_SLACK: std::time::Duration = std::time::Duration::from_secs(5);

impl ChunkStore {
    pub fn open(root: &Path) -> FsResult<Self> {
        fs::create_dir_all(root).map_err(io)?;
        let mut store = Self {
            root: root.to_path_buf(),
            chunks: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            publish: Mutex::new(()),
            tmp: AtomicU64::new(0),
            added: None,
            unsynced: Mutex::new(HashSet::new()),
        };
        store.recover()?;
        let found = store.scan()?;
        *store.chunks.get_mut() = found.chunks;
        *store.bytes.get_mut() = found.stored_bytes;
        Ok(store)
    }

    /// Remember each chunk added from now on, for [`Self::take_added`].
    pub fn recording_additions(mut self) -> Self {
        self.added = Some(Mutex::new(Vec::new()));
        self
    }

    /// The chunks added since the last call, or since the store was opened.
    /// Always empty unless the store records additions.
    pub fn take_added(&self) -> Vec<ChunkId> {
        self.added
            .as_ref()
            .map(|a| std::mem::take(&mut *a.lock().unwrap()))
            .unwrap_or_default()
    }

    fn path(&self, id: &ChunkId) -> PathBuf {
        let hex = blake3::Hash::from_bytes(*id).to_hex();
        self.root.join(&hex.as_str()[..2]).join(hex.as_str())
    }

    /// Store a chunk, returning its id. Identical content is stored once.
    pub fn put(&self, data: &[u8]) -> FsResult<ChunkId> {
        self.store(data, true)
    }

    /// Like [`Self::put`], but the chunk may reach disk later: [`Self::sync`]
    /// makes it durable. A crash before then can lose the chunk, never keep
    /// a damaged one (see [`Self::open`]).
    pub fn put_lazy(&self, data: &[u8]) -> FsResult<ChunkId> {
        self.store(data, false)
    }

    fn store(&self, data: &[u8], durable: bool) -> FsResult<ChunkId> {
        let id = *blake3::hash(data).as_bytes();
        let path = self.path(&id);
        if path.exists() {
            if durable {
                self.sync_chunks(&[id])?;
            }
            return Ok(id);
        }
        if !durable {
            self.expect_unsynced(id)?;
        }
        let mut envelope = vec![FORMAT_VERSION, CIPHER_NONE, CODEC_RAW, 0];
        envelope.extend_from_slice(&(data.len() as u32).to_le_bytes());
        // Compress, but keep the raw bytes when that does not actually help.
        match zstd::bulk::compress(data, ZSTD_LEVEL) {
            Ok(z) if z.len() < data.len() => {
                envelope[2] = CODEC_ZSTD;
                envelope.extend_from_slice(&z);
            }
            _ => envelope.extend_from_slice(data),
        }
        let dir = path.parent().expect("chunk path has a parent");
        fs::create_dir_all(dir).map_err(io)?;
        let n = self.tmp.fetch_add(1, Ordering::SeqCst);
        let tmp = dir.join(format!(".tmp-{}-{n}", std::process::id()));
        let mut f = fs::File::create(&tmp).map_err(io)?;
        f.write_all(&envelope).map_err(io)?;
        if durable {
            f.sync_all().map_err(io)?;
        }
        let _publish = self.publish.lock().unwrap();
        if path.exists() {
            // Stored by someone else since the check above.
            #[allow(
                clippy::let_underscore_must_use,
                reason = "the temporary file is not needed any more"
            )]
            let _ = fs::remove_file(&tmp);
            if durable {
                self.sync_chunks(&[id])?;
            }
            return Ok(id);
        }
        fs::rename(&tmp, &path).map_err(io)?;
        self.chunks.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .fetch_add(envelope.len() as u64, Ordering::SeqCst);
        if let Some(added) = &self.added {
            added.lock().unwrap().push(id);
        }
        Ok(id)
    }

    /// Whether lazily stored chunks wait for [`Self::sync`].
    pub fn has_unsynced(&self) -> bool {
        !self.unsynced.lock().unwrap().is_empty()
    }

    /// Make every lazily stored chunk durable.
    pub fn sync(&self) -> FsResult<()> {
        let ids: Vec<ChunkId> = self.unsynced.lock().unwrap().iter().copied().collect();
        if ids.is_empty() {
            return Ok(());
        }
        self.sync_chunks(&ids)
    }

    fn sync_chunks(&self, ids: &[ChunkId]) -> FsResult<()> {
        for id in ids {
            if !self.unsynced.lock().unwrap().contains(id) {
                continue;
            }
            match fs::File::open(self.path(id)).and_then(|f| f.sync_all()) {
                Ok(()) => {}
                // Removed since: nothing left to make durable.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(e)),
            }
            let mut unsynced = self.unsynced.lock().unwrap();
            unsynced.remove(id);
            if unsynced.is_empty() {
                // Nothing is at risk now: a crash needs no recovery pass.
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "a marker left behind only costs a recovery pass"
                )]
                let _ = fs::remove_file(self.marker());
            }
        }
        Ok(())
    }

    /// Where the dirty marker lives: next to the store, so scans of the store never see it.
    fn marker(&self) -> PathBuf {
        let mut name = self.root.file_name().unwrap_or_default().to_os_string();
        name.push(".dirty");
        self.root.with_file_name(name)
    }

    /// Note that `id` is about to be written without a sync. While anything
    /// is unsynced a marker file says so on disk, made durable first, so that
    /// after a crash [`Self::open`] knows which chunk files to check.
    fn expect_unsynced(&self, id: ChunkId) -> FsResult<()> {
        let mut unsynced = self.unsynced.lock().unwrap();
        if unsynced.is_empty() {
            let marker = self.marker();
            fs::File::create(&marker)
                .and_then(|f| f.sync_all())
                .map_err(io)?;
            if let Some(parent) = marker.parent() {
                fs::File::open(parent)
                    .and_then(|d| d.sync_all())
                    .map_err(io)?;
            }
        }
        unsynced.insert(id);
        Ok(())
    }

    /// After a crash with unsynced chunks, drop the chunk files written since
    /// the marker that do not hash to their name: they were cut short.
    fn recover(&self) -> FsResult<()> {
        let marker = self.marker();
        let Ok(since) = fs::metadata(&marker).and_then(|m| m.modified()) else {
            return Ok(());
        };
        let since = since.checked_sub(DIRTY_SLACK).unwrap_or(since);
        for (id, _, mtime) in self.scan_files()? {
            if mtime >= since && self.get(&id).is_err() {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "a chunk that cannot be removed is reported when read"
                )]
                let _ = fs::remove_file(self.path(&id));
            }
        }
        fs::remove_file(&marker).map_err(io)
    }

    /// Fetch a chunk, verifying its BLAKE3 hash. Corruption is an I/O error.
    pub fn get(&self, id: &ChunkId) -> FsResult<Vec<u8>> {
        let raw = fs::read(self.path(id)).map_err(io)?;
        let corrupt = || FsError::Io("chunk is corrupt".into());
        if raw.len() < HEADER_LEN || raw[0] != FORMAT_VERSION || raw[1] != CIPHER_NONE {
            return Err(corrupt());
        }
        let len = plain_len(&raw) as usize;
        let data = match raw[2] {
            CODEC_RAW => raw[HEADER_LEN..].to_vec(),
            CODEC_ZSTD => zstd::bulk::decompress(&raw[HEADER_LEN..], len).map_err(|_| corrupt())?,
            _ => return Err(corrupt()),
        };
        if data.len() != len || blake3::hash(&data).as_bytes() != id {
            return Err(corrupt());
        }
        Ok(data)
    }

    /// Delete a chunk; absent chunks are fine.
    pub fn remove(&self, id: &ChunkId) {
        let path = self.path(id);
        let _publish = self.publish.lock().unwrap();
        let Ok(len) = fs::metadata(&path).map(|m| m.len()) else {
            return; // an absent chunk is already removed
        };
        if fs::remove_file(&path).is_ok() {
            self.chunks.fetch_sub(1, Ordering::SeqCst);
            self.bytes.fetch_sub(len, Ordering::SeqCst);
        }
    }

    /// Bytes the chunk occupies on disk, if stored.
    pub fn size_of(&self, id: &ChunkId) -> Option<u64> {
        fs::metadata(self.path(id)).ok().map(|m| m.len())
    }

    /// Every stored chunk with its size on disk and last modification time.
    pub fn list(&self) -> FsResult<Vec<(ChunkId, u64, std::time::SystemTime)>> {
        self.scan_files()
    }

    fn scan_files(&self) -> FsResult<Vec<(ChunkId, u64, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for shard in fs::read_dir(&self.root).map_err(io)? {
            let shard = shard.map_err(io)?.path();
            for f in fs::read_dir(&shard).map_err(io)? {
                let f = f.map_err(io)?;
                let name = f.file_name().to_string_lossy().into_owned();
                let Ok(hash) = blake3::Hash::from_hex(&name) else {
                    continue; // temporary files
                };
                let meta = f.metadata().map_err(io)?;
                let mtime = meta.modified().map_err(io)?;
                out.push((*hash.as_bytes(), meta.len(), mtime));
            }
        }
        Ok(out)
    }

    /// What each stored chunk's envelope says about it. Reads only the
    /// headers, so nothing is decompressed.
    pub fn survey(&self) -> FsResult<Vec<(ChunkId, ChunkInfo)>> {
        let mut out = Vec::new();
        for shard in fs::read_dir(&self.root).map_err(io)? {
            let shard = shard.map_err(io)?.path();
            for f in fs::read_dir(&shard).map_err(io)? {
                let f = f.map_err(io)?;
                let name = f.file_name().to_string_lossy().into_owned();
                let Ok(hash) = blake3::Hash::from_hex(&name) else {
                    continue; // temporary files
                };
                let mut header = [0u8; HEADER_LEN];
                fs::File::open(f.path())
                    .and_then(|mut file| file.read_exact(&mut header))
                    .map_err(io)?;
                out.push((
                    *hash.as_bytes(),
                    ChunkInfo {
                        disk_bytes: f.metadata().map_err(io)?.len(),
                        plain_bytes: plain_len(&header).into(),
                        zstd: header[2] == CODEC_ZSTD,
                    },
                ));
            }
        }
        Ok(out)
    }

    pub fn stats(&self) -> FsResult<StoreStats> {
        Ok(StoreStats {
            chunks: self.chunks.load(Ordering::SeqCst),
            stored_bytes: self.bytes.load(Ordering::SeqCst),
        })
    }

    /// Count the store by walking it: only needed once, when it is opened.
    fn scan(&self) -> FsResult<StoreStats> {
        let mut stats = StoreStats::default();
        for shard in fs::read_dir(&self.root).map_err(io)? {
            let shard = shard.map_err(io)?.path();
            for f in fs::read_dir(&shard).map_err(io)? {
                let f = f.map_err(io)?;
                if f.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                stats.chunks += 1;
                stats.stored_bytes += f.metadata().map_err(io)?.len();
            }
        }
        Ok(stats)
    }
}
