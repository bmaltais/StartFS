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
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

pub type ChunkId = [u8; 32];

pub const HEADER_LEN: usize = 8;
const FORMAT_VERSION: u8 = 1;
const CIPHER_NONE: u8 = 0;
const CODEC_RAW: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const ZSTD_LEVEL: i32 = 3;
/// Threads that fsync chunk files at once when many are waiting.
const SYNC_THREADS: usize = 8;
/// Fewer chunks than this are synced one after the other.
const SYNC_PARALLEL_MIN: usize = 4;
/// `auto` samples this many spans of this many bytes to judge a chunk.
const PROBE_SPANS: usize = 4;
const PROBE_SPAN: usize = 16 * 1024;
/// `auto` keeps zstd output only when it is at least this fraction smaller
/// (1/8): a smaller gain is not worth decompressing on every read.
const MIN_SAVING_DIV: usize = 8;

/// Whether chunks are compressed before they are written to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    /// Try zstd only on chunks a quick sample shows to be compressible, and
    /// keep the result only when it saves a worthwhile fraction.
    #[default]
    Auto,
    /// Store chunks as they are.
    Off,
    /// Compress every chunk and keep the result whenever it is smaller.
    Always,
}

impl Compression {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Off,
            2 => Self::Always,
            _ => Self::Auto,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Off => 1,
            Self::Always => 2,
        }
    }
}

fn zstd_pack(data: &[u8]) -> Option<Vec<u8>> {
    zstd::bulk::compress(data, ZSTD_LEVEL).ok()
}

/// Whether `packed` bytes out of `plain` is a saving worth keeping.
fn saves_enough(packed: usize, plain: usize) -> bool {
    packed <= plain - plain / MIN_SAVING_DIV
}

/// A cheap guess at whether compressing all of `data` would pay: compress a
/// few spans spread across it. Small chunks are cheap enough to just try.
fn looks_compressible(data: &[u8]) -> bool {
    if data.len() <= 2 * PROBE_SPANS * PROBE_SPAN {
        return true;
    }
    let step = (data.len() - PROBE_SPAN) / (PROBE_SPANS - 1);
    let mut packed = 0;
    for k in 0..PROBE_SPANS {
        let span = &data[k * step..][..PROBE_SPAN];
        packed += zstd_pack(span).map_or(PROBE_SPAN, |z| z.len().min(PROBE_SPAN));
    }
    saves_enough(packed, PROBE_SPANS * PROBE_SPAN)
}

/// The envelope for `data` under the compression policy `mode`.
fn encode(data: &[u8], mode: Compression) -> Vec<u8> {
    let packed = match mode {
        Compression::Off => None,
        Compression::Always => zstd_pack(data).filter(|z| z.len() < data.len()),
        Compression::Auto => looks_compressible(data)
            .then(|| zstd_pack(data))
            .flatten()
            .filter(|z| saves_enough(z.len(), data.len())),
    };
    let codec = if packed.is_some() {
        CODEC_ZSTD
    } else {
        CODEC_RAW
    };
    let body = packed.as_deref().unwrap_or(data);
    let mut envelope = Vec::with_capacity(HEADER_LEN + body.len());
    envelope.extend_from_slice(&[FORMAT_VERSION, CIPHER_NONE, codec, 0]);
    envelope.extend_from_slice(&(data.len() as u32).to_le_bytes());
    envelope.extend_from_slice(body);
    envelope
}

/// The plaintext length from an envelope's header (at least [`HEADER_LEN`] bytes).
fn plain_len(envelope: &[u8]) -> u32 {
    u32::from_le_bytes(envelope[4..8].try_into().unwrap())
}

/// What an I/O error says when a chunk file was read but is not the chunk it is named for.
pub const CORRUPT: &str = "chunk is corrupt";

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
    /// A [`Compression`] as a number; read when a chunk is stored.
    compression: AtomicU8,
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
            compression: AtomicU8::new(Compression::default().as_u8()),
        };
        store.recover()?;
        let found = store.scan()?;
        *store.chunks.get_mut() = found.chunks;
        *store.bytes.get_mut() = found.stored_bytes;
        Ok(store)
    }

    /// Choose whether chunks stored from now on are compressed. Chunks already
    /// stored keep the codec they have, and read back the same either way.
    pub fn set_compression(&self, mode: Compression) {
        self.compression.store(mode.as_u8(), Ordering::SeqCst);
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
        let id = *blake3::hash(data).as_bytes();
        self.put_hashed(id, data)?;
        Ok(id)
    }

    /// [`Self::put`] for a caller that has already hashed `data` into `id`,
    /// which must be its BLAKE3 hash: a wrong id stores a chunk that fails
    /// verification when read.
    pub fn put_hashed(&self, id: ChunkId, data: &[u8]) -> FsResult<()> {
        self.store(id, data, true)
    }

    /// Like [`Self::put`], but the chunk may reach disk later: [`Self::sync`]
    /// makes it durable. A crash before then can lose the chunk, never keep
    /// a damaged one (see [`Self::open`]).
    ///
    /// `id` must be the BLAKE3 hash of `data`, as for [`Self::put_hashed`].
    pub fn put_lazy_hashed(&self, id: ChunkId, data: &[u8]) -> FsResult<()> {
        self.store(id, data, false)
    }

    fn store(&self, id: ChunkId, data: &[u8], durable: bool) -> FsResult<()> {
        let path = self.path(&id);
        if path.exists() {
            if durable {
                self.sync_chunks(&[id])?;
            }
            return Ok(());
        }
        if !durable {
            self.expect_unsynced(id)?;
        }
        let envelope = encode(
            data,
            Compression::from_u8(self.compression.load(Ordering::SeqCst)),
        );
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
            return Ok(());
        }
        fs::rename(&tmp, &path).map_err(io)?;
        self.chunks.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .fetch_add(envelope.len() as u64, Ordering::SeqCst);
        if let Some(added) = &self.added {
            added.lock().unwrap().push(id);
        }
        Ok(())
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
        let pending: Vec<ChunkId> = {
            let unsynced = self.unsynced.lock().unwrap();
            ids.iter()
                .filter(|id| unsynced.contains(*id))
                .copied()
                .collect()
        };
        let sync_one = |id: &ChunkId| match fs::File::open(self.path(id)).and_then(|f| f.sync_all())
        {
            // Removed since: nothing left to make durable.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other.map_err(io),
        };
        // Flushes of different files overlap on a device with a deep queue.
        let results: Vec<(ChunkId, FsResult<()>)> = if pending.len() < SYNC_PARALLEL_MIN {
            pending.iter().map(|id| (*id, sync_one(id))).collect()
        } else {
            let per = pending.len().div_ceil(SYNC_THREADS);
            std::thread::scope(|scope| {
                let workers: Vec<_> = pending
                    .chunks(per)
                    .map(|part| {
                        let sync_one = &sync_one;
                        scope.spawn(move || {
                            part.iter()
                                .map(|id| (*id, sync_one(id)))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .flat_map(|w| w.join().expect("sync worker does not panic"))
                    .collect()
            })
        };
        // Only chunks that reached disk leave the set.
        let mut first_err = Ok(());
        let mut unsynced = self.unsynced.lock().unwrap();
        for (id, result) in results {
            match result {
                Ok(()) => {
                    unsynced.remove(&id);
                }
                Err(e) => first_err = first_err.and(Err(e)),
            }
        }
        if unsynced.is_empty() {
            // Nothing is at risk now: a crash needs no recovery pass.
            #[allow(
                clippy::let_underscore_must_use,
                reason = "a marker left behind only costs a recovery pass"
            )]
            let _ = fs::remove_file(self.marker());
        }
        first_err
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
        let corrupt = || FsError::Io(CORRUPT.into());
        let mut file = fs::File::open(self.path(id)).map_err(io)?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact(&mut header).map_err(|_| corrupt())?;
        if header[0] != FORMAT_VERSION || header[1] != CIPHER_NONE {
            return Err(corrupt());
        }
        let len = plain_len(&header) as usize;
        // The body goes straight into its own buffer: reading the whole file and dropping the
        // header shifted every byte of a raw chunk down by eight.
        let body_len = file
            .metadata()
            .map_err(io)?
            .len()
            .saturating_sub(HEADER_LEN as u64);
        let mut body = Vec::with_capacity(body_len as usize);
        file.read_to_end(&mut body).map_err(io)?;
        let data = match header[2] {
            CODEC_RAW => body,
            CODEC_ZSTD => zstd::bulk::decompress(&body, len).map_err(|_| corrupt())?,
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
