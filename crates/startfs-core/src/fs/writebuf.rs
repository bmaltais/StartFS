//! Buffered writes. A write is held in memory, in disjoint regions per file
//! (a linker writes its output at scattered offsets), and committed in batches:
//! a commit re-chunks only the parts of the file the regions touch and reuses
//! the chunk references around them. Memory is
//! bounded by the buffer cap plus a few chunks, whatever the file's size.
//!
//! Content-defined cuts depend only on the bytes after the previous cut, so
//! re-chunking from the first affected chunk until a new cut lands on an old
//! boundary gives the same chunks as chunking the whole file.

use super::replicas::Pinned;
use super::*;
use crate::chunker::MAX_CHUNK;
use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Default cap on a file's buffered bytes before a batch is committed.
pub const DEFAULT_WRITE_BUFFER: u64 = 8 << 20;

/// Regions written closer than this to each other are re-chunked as one window,
/// the bytes between them read from the file: re-chunking each on its own would
/// cut the same chunks around them over and over.
const NEAR: u64 = MAX_CHUNK as u64;
/// The most a window of regions spans, so the memory a commit takes stays bounded.
const WINDOW_MAX: u64 = 64 << 20;

/// A run of a file's bytes: `data` from `start`.
#[derive(Clone)]
struct Region {
    start: u64,
    data: Vec<u8>,
}

impl Region {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

/// Writes held for one file: disjoint regions by start, none touching another. While
/// `busy`, a commit of these bytes is running without the buffers' lock (it may
/// be waiting on a peer): readers still see them, writers wait for it to end.
#[derive(Clone)]
pub(super) struct Dirty {
    regions: BTreeMap<u64, Vec<u8>>,
    mtime: Nanos,
    busy: bool,
    /// Commits of these bytes have failed `failures` times in a row; a background commit waits
    /// until `retry_at` before trying again.
    failures: u32,
    retry_at: Nanos,
}

impl Dirty {
    fn new(start: u64, data: Vec<u8>, mtime: Nanos) -> Self {
        Dirty {
            regions: BTreeMap::from([(start, data)]),
            mtime,
            busy: false,
            failures: 0,
            retry_at: 0,
        }
    }

    fn start(&self) -> u64 {
        self.regions.keys().next().copied().unwrap_or(0)
    }

    fn end(&self) -> u64 {
        (self.regions.iter().next_back()).map_or(0, |(s, d)| s + d.len() as u64)
    }

    /// Bytes held.
    fn len(&self) -> u64 {
        self.regions.values().map(|d| d.len() as u64).sum()
    }

    /// Put `data` at `offset`, over what was there; regions it overlaps or touches become one.
    fn put(&mut self, offset: u64, data: &[u8]) {
        let end = offset + data.len() as u64;
        let joined: Vec<u64> = (self.regions.range(..=end))
            .filter(|(s, d)| **s + d.len() as u64 >= offset)
            .map(|(s, _)| *s)
            .collect();
        // A region that starts at or before `offset` is grown in place: a sequential
        // writer extends the same region on every write, and rebuilding it each time
        // would copy everything buffered so far.
        let (start, mut merged, rest) = match joined.split_first() {
            Some((&s, rest)) if s <= offset => {
                (s, self.regions.remove(&s).expect("listed above"), rest)
            }
            _ => (offset, Vec::new(), joined.as_slice()),
        };
        for s in rest {
            let d = self.regions.remove(s).expect("listed above");
            let at = (*s - start) as usize;
            if merged.len() < at + d.len() {
                merged.resize(at + d.len(), 0);
            }
            merged[at..at + d.len()].copy_from_slice(&d);
        }
        let at = (offset - start) as usize;
        if merged.len() < at + data.len() {
            merged.resize(at + data.len(), 0);
        }
        merged[at..at + data.len()].copy_from_slice(data);
        self.regions.insert(start, merged);
    }

    /// The regions, in groups of near ones that are re-chunked as one window.
    fn into_windows(self) -> Vec<Vec<Region>> {
        let mut out: Vec<Vec<Region>> = Vec::new();
        for (s, d) in self.regions {
            let r = Region { start: s, data: d };
            match out.last_mut() {
                Some(w)
                    if r.start - w.last().expect("not empty").end() <= NEAR
                        && r.end() - w[0].start <= WINDOW_MAX =>
                {
                    w.push(r)
                }
                _ => out.push(vec![r]),
            }
        }
        out
    }
}

/// What a commit produced: the file's new content and size, and the tail of
/// the region it kept back in the buffer (a batch ends only on a real cut).
struct Spliced<'a> {
    content: Content,
    size: u64,
    rest: Option<Region>,
    /// The new chunks, stored and pinned until the file refers to them.
    _pinned: Option<Pinned<'a>>,
}

/// A commit of a whole buffer: the file's new content and size, the tail kept back,
/// and the pins of every chunk it stored.
struct Batch<'a> {
    content: Content,
    size: u64,
    rest: Option<Region>,
    _pinned: Vec<Pinned<'a>>,
}

/// Buffered bytes as runs, each with where it starts.
pub(super) type Runs = Vec<(u64, Vec<u8>)>;

/// How long a background commit leaves a file alone after its `failures`th failure in a row:
/// doubling from a second to a minute.
fn retry_delay(failures: u32) -> Nanos {
    Duration::from_secs(1 << failures.saturating_sub(1).min(6)).as_nanos() as Nanos
}

impl LocalFs {
    /// Cap on a file's buffered bytes. At least two maximum-size chunks, so a
    /// full buffer always holds a whole chunk to commit.
    pub fn set_write_buffer_limit(&self, bytes: u64) {
        self.write_buffer_limit.store(bytes, Ordering::SeqCst);
    }

    fn buffer_cap(&self) -> u64 {
        (self.write_buffer_limit.load(Ordering::SeqCst)).max(2 * MAX_CHUNK as u64)
    }

    pub(super) fn buffered_write(&self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<usize> {
        let mut dirty = self.dirty.lock().unwrap();
        if !dirty.contains_key(&ino) {
            self.view(|t| Self::file(t, ino).map(|_| ()))?;
        }
        self.admit(&dirty, data.len() as u64)?;
        let now = self.clock.now();
        let mut tried_full = false;
        // Commit before merging, so a commit that fails rejects this write
        // and leaves the buffer as it was. A commit runs without the lock, so
        // the buffer is looked at again once it is done.
        loop {
            while dirty.get(&ino).is_some_and(|d| d.busy) {
                dirty = self.committed.wait(dirty).unwrap();
            }
            let Some(d) = dirty.get(&ino) else { break };
            if !tried_full && d.len() + data.len() as u64 >= self.buffer_cap() {
                tried_full = true;
                drop(dirty);
                self.commit(ino, false, true)?;
            } else {
                break;
            }
            dirty = self.dirty.lock().unwrap();
        }
        match dirty.get_mut(&ino) {
            Some(d) => {
                d.put(offset, data);
                d.mtime = now;
            }
            None => {
                // The first buffered write of a file: the tick commits it once it is quiet.
                self.wake_tick();
                dirty.insert(ino, Dirty::new(offset, data.to_vec(), now));
            }
        }
        Ok(data.len())
    }

    /// Refuse a write that cannot fit the staging area once everything
    /// buffered is committed. A node without a staging cap never refuses here.
    fn admit(&self, dirty: &HashMap<Ino, Dirty>, incoming: u64) -> FsResult<()> {
        let limit = self.staging_limit.load(Ordering::SeqCst);
        if limit == u64::MAX || !self.replicating.load(Ordering::SeqCst) {
            return Ok(());
        }
        let queued: u64 = dirty.values().map(Dirty::len).sum();
        if self.staging.stats()?.stored_bytes + queued + incoming > limit {
            return Err(FsError::NoSpace);
        }
        Ok(())
    }

    /// Commit one file's buffer. `last` commits everything; otherwise the
    /// final chunk stays buffered, since more data may still change it. The
    /// buffers' lock is not held while it runs, since it can wait on a peer:
    /// the file's entry stays, marked busy, so reads see its bytes and writes
    /// to it wait. With `wait` false (the background tick) a file already being
    /// committed is skipped, and so is one whose last commits failed until its
    /// retry time: an explicit flush always tries.
    fn commit(&self, ino: Ino, last: bool, wait: bool) -> FsResult<()> {
        let d = {
            let mut dirty = self.dirty.lock().unwrap();
            loop {
                match dirty.get_mut(&ino) {
                    None => return Ok(()),
                    Some(d) if !wait && self.clock.now() < d.retry_at => return Ok(()),
                    Some(d) if !d.busy => {
                        d.busy = true;
                        break d.clone();
                    }
                    Some(_) if !wait => return Ok(()),
                    Some(_) => dirty = self.committed.wait(dirty).unwrap(),
                }
            }
        };
        let mtime = d.mtime;
        let done = self.commit_buffer(ino, d, mtime, last);
        let mut dirty = self.dirty.lock().unwrap();
        let out = match done {
            Ok(Some(rest)) => {
                match rest {
                    Some(r) => dirty.insert(ino, Dirty::new(r.start, r.data, mtime)),
                    None => dirty.remove(&ino),
                };
                Ok(())
            }
            Ok(None) => {
                Self::release_buffer(&mut dirty, ino);
                Ok(())
            }
            // The file was removed since it was written to.
            Err(FsError::NotFound) => {
                dirty.remove(&ino);
                Ok(())
            }
            Err(e) => {
                Self::release_buffer(&mut dirty, ino);
                if let Some(d) = dirty.get_mut(&ino) {
                    d.failures = d.failures.saturating_add(1);
                    d.retry_at = self.clock.now().saturating_add(retry_delay(d.failures));
                }
                Err(e)
            }
        };
        drop(dirty);
        self.committed.notify_all();
        out
    }

    /// Splice a buffer into its file, one window of near regions after another, and
    /// apply them as one new version. The chunk work (reading the neighbouring
    /// chunks, cutting, hashing, compressing, storing) runs without the node lock,
    /// on a snapshot of the file; only applying the result takes it, and does so
    /// only if the file is still as it was, else the splice starts again.
    /// Returns the tail kept back, or `None` when nothing was ready to commit.
    fn commit_buffer(
        &self,
        ino: Ino,
        d: Dirty,
        mtime: Nanos,
        last: bool,
    ) -> FsResult<Option<Option<Region>>> {
        let windows = d.into_windows();
        loop {
            let snap = self.view(|v| Self::file(v, ino))?;
            let was = encode(&snap);
            let mut spill = Spill::default();
            let spliced = loop {
                spill.miss = None;
                match self.splice_windows(&snap, &windows, last, &mut spill) {
                    Err(FsError::Unavailable) if spill.miss.is_some() => {
                        let id = spill.miss.take().expect("checked");
                        let data = self.fetch_remote(&id)?;
                        spill.got.insert(id, data);
                    }
                    other => break other,
                }
            };
            let spliced = match spliced {
                Ok(s) => s,
                // A chunk of the snapshot may have gone with a change made since.
                Err(_) if encode(&self.view(|v| Self::file(v, ino))?) != was => continue,
                Err(e) => return Err(e),
            };
            let Some(Batch {
                content,
                size,
                rest,
                _pinned,
            }) = spliced
            else {
                return Ok(None);
            };
            let applied = self.run_as(true, Durability::None, |t| {
                let mut i = Self::file(t, ino)?;
                if encode(&i) != was {
                    return Ok(false);
                }
                i.content = content.clone();
                i.size = size;
                i.mtime = mtime;
                i.ctime = mtime;
                t.put(ino, &i)?;
                self.emit_set_at(t, ino, &i, mtime)?;
                Ok(true)
            })?;
            if applied {
                return Ok(Some(rest));
            }
        }
    }

    /// Splice each window into `i` in turn: the new content and size, the tail kept back
    /// from the last window, and the pins of every chunk stored. `None` if nothing was
    /// ready to commit. Only the last window may keep a tail back (`last` false), so the
    /// others are committed whole; a last window not ready yet stays buffered as it was.
    fn splice_windows(
        &self,
        i: &Inode,
        windows: &[Vec<Region>],
        last: bool,
        spill: &mut Spill,
    ) -> FsResult<Option<Batch<'_>>> {
        let mut cur = i.clone();
        let (mut pins, mut rest, mut changed) = (Vec::new(), None, false);
        for (k, window) in windows.iter().enumerate() {
            let final_window = k + 1 == windows.len();
            let region = self.fill(&cur, window, spill)?;
            match self.splice(&cur, &region, last || !final_window, spill)? {
                Some(s) => {
                    cur.content = s.content;
                    cur.size = s.size;
                    pins.extend(s._pinned);
                    rest = s.rest;
                    changed = true;
                }
                None => {
                    // Not ready (`last` false, under a chunk): its own bytes stay buffered.
                    if window.len() > 1 {
                        // Several regions do not fit one kept-back tail: commit them.
                        let s = self
                            .splice(&cur, &region, true, spill)?
                            .expect("a whole commit is always ready");
                        cur.content = s.content;
                        cur.size = s.size;
                        pins.extend(s._pinned);
                        changed = true;
                    } else {
                        rest = Some(window[0].clone());
                    }
                }
            }
        }
        Ok(changed.then_some(Batch {
            content: cur.content,
            size: cur.size,
            rest,
            _pinned: pins,
        }))
    }

    /// One window as a single region: its regions over the file's bytes between them.
    fn fill<'w>(
        &self,
        i: &Inode,
        window: &'w [Region],
        spill: &mut Spill,
    ) -> FsResult<Cow<'w, Region>> {
        let start = window[0].start;
        let end = window.last().expect("not empty").end();
        if window.len() == 1 {
            return Ok(Cow::Borrowed(&window[0]));
        }
        let mut data = self.read_committed(i, start.min(i.size), end.min(i.size), spill)?;
        data.resize((end - start) as usize, 0);
        for r in window {
            data[(r.start - start) as usize..][..r.data.len()].copy_from_slice(&r.data);
        }
        Ok(Cow::Owned(Region { start, data }))
    }

    /// Mark a file's buffer as no longer being committed.
    fn release_buffer(dirty: &mut HashMap<Ino, Dirty>, ino: Ino) {
        if let Some(d) = dirty.get_mut(&ino) {
            d.busy = false;
        }
    }

    /// Re-chunk the region `d` over the file `i`. `None`: nothing is ready to commit.
    fn splice(
        &self,
        i: &Inode,
        d: &Region,
        last: bool,
        spill: &mut Spill,
    ) -> FsResult<Option<Spliced<'_>>> {
        let re = d.end();
        let new_total = i.size.max(re);
        let rs = d.start.min(i.size);
        let (refs, inline): (&[ChunkRef], Option<&Vec<u8>>) = match &i.content {
            Content::Chunks(r) => (r, None),
            Content::Inline(b) => (&[], Some(b)),
        };
        // bounds[k] is where chunk k starts; the last entry is the old size.
        let mut bounds = Vec::with_capacity(refs.len() + 1);
        let mut pos = 0u64;
        for r in refs {
            bounds.push(pos);
            pos += r.len as u64;
        }
        bounds.push(pos);
        let n = refs.len();
        let a = (0..n).find(|&k| bounds[k + 1] > rs).unwrap_or(n.max(1) - 1);
        let sa = bounds[a];
        let mut b = (a + 1..=n).find(|&k| bounds[k] >= re).unwrap_or(n);

        let mut win: Vec<u8> = inline.cloned().unwrap_or_default();
        let mut loaded = a;
        loop {
            let eof = b == n;
            for r in &refs[loaded..b] {
                win.extend_from_slice(&self.chunk(&r.id, spill)?);
            }
            loaded = b;
            let wend = if eof { new_total } else { bounds[b] };
            win.resize((wend - sa) as usize, 0);
            win[(d.start - sa) as usize..][..d.data.len()].copy_from_slice(&d.data);

            if sa == 0 && eof && win.len() <= INLINE_MAX {
                return Ok(last.then_some(Spliced {
                    size: new_total,
                    content: Content::Inline(win),
                    rest: None,
                    _pinned: None,
                }));
            }
            let pieces: Vec<Piece<'_>> = chunker::split(&win).into_iter().map(Piece::new).collect();
            // Where the new chunks rejoin the old ones, if they do: a cut at or
            // past the region that is also an old boundary. A cut is only
            // trusted when the chunker saw its whole maximum span.
            let mut rejoin = None;
            if !eof {
                let mut at = sa;
                for (idx, p) in pieces.iter().enumerate() {
                    let (s, e) = (at, at + p.data.len() as u64);
                    at = e;
                    if s + MAX_CHUNK as u64 > wend {
                        break;
                    }
                    if e >= re
                        && let Some(k) = bounds[a..=b].iter().position(|&x| x == e)
                    {
                        rejoin = Some((idx + 1, a + k));
                        break;
                    }
                }
            }
            if rejoin.is_none() && !eof {
                let target = bounds[b] + MAX_CHUNK as u64;
                b = (b + 1..=n).find(|&k| bounds[k] >= target).unwrap_or(n);
                continue;
            }
            let (mut used, tail_from) = rejoin.unwrap_or((pieces.len(), n));
            let mut rest = None;
            if rejoin.is_none() && !last {
                // Keep the last chunk back: it ends at the end of the data, not on a cut.
                if used <= 1 {
                    return Ok(None);
                }
                // Only a tail made of new bytes may stay in memory: bytes that
                // were already durable must not become volatile.
                let at = sa
                    + pieces[..used - 1]
                        .iter()
                        .map(|p| p.data.len() as u64)
                        .sum::<u64>();
                if at >= i.size {
                    used -= 1;
                    rest = Some(Region {
                        start: at,
                        data: win[(at - sa) as usize..].to_vec(),
                    });
                }
            }
            let committed = &pieces[..used];
            self.make_room(committed)?;
            let pinned = self.pin(committed.iter().map(|p| p.id));
            let mut out: Vec<ChunkRef> = refs[..a].to_vec();
            let mut size = sa;
            let ids = self.keep_all(committed)?;
            for (p, id) in committed.iter().zip(ids) {
                out.push(ChunkRef {
                    id,
                    len: p.data.len() as u32,
                });
                size += p.data.len() as u64;
            }
            out.extend_from_slice(&refs[tail_from..]);
            // The old chunks kept after the new ones. Inline content has none: all of it
            // was in the window, so counting `i.size` again would add phantom zeros.
            if inline.is_none() {
                size += i.size - bounds[tail_from];
            }
            return Ok(Some(Spliced {
                content: Content::Chunks(out),
                size,
                rest,
                _pinned: Some(pinned),
            }));
        }
    }

    /// The tick leaves a file alone for `delay` after its last write; see [`Self::close_ino`].
    pub fn set_write_commit_delay(&self, delay: Duration) {
        self.write_commit_delay
            .store(delay.as_nanos() as u64, Ordering::SeqCst);
    }

    /// A file is closed. Its buffer is committed now, except when that would write a chunk
    /// again that is already committed (an append or a patch to a file that has chunks) and the
    /// tick is set to wait for writes to stop: `ar`, a logger or `>>` in a loop reopen the file
    /// many times a second, and each close would store its whole tail chunk once more. The
    /// tick commits the buffer once the file has been quiet for the delay; reads see it meanwhile.
    pub(super) fn close_ino(&self, ino: Ino) -> FsResult<()> {
        if self.write_commit_delay.load(Ordering::SeqCst) > 0 {
            let start = self.dirty.lock().unwrap().get(&ino).map(Dirty::start);
            let rewrites = start.is_some_and(|start| {
                let committed = self.view(|t| Self::file(t, ino));
                committed.is_ok_and(|i| matches!(i.content, Content::Chunks(_)) && start <= i.size)
            });
            if rewrites {
                self.wake_tick();
                return Ok(());
            }
        }
        self.flush_ino(ino)
    }

    /// Commit what is buffered for `ino`.
    pub(super) fn flush_ino(&self, ino: Ino) -> FsResult<()> {
        self.commit(ino, true, true)
    }

    /// Commit every file's buffer. With `wait` false a buffer already being
    /// committed is skipped (a commit waiting on a peer may be what is running this).
    pub fn flush_all(&self, wait: bool) -> FsResult<()> {
        self.flush_idle(wait, 0)
    }

    /// Commit the buffers of files not written to for `idle` nanoseconds, so
    /// a file still being written keeps batching.
    fn flush_idle(&self, wait: bool, idle: Nanos) -> FsResult<()> {
        let now = self.clock.now();
        let inos: Vec<Ino> = (self.dirty.lock().unwrap().iter())
            .filter(|(_, d)| now.saturating_sub(d.mtime) >= idle)
            .map(|(ino, _)| *ino)
            .collect();
        let mut first_err = Ok(());
        for ino in inos {
            if let Err(e) = self.commit(ino, true, wait) {
                first_err = first_err.and(Err(e));
                if !wait {
                    // One file waiting on peers is all a tick can afford.
                    break;
                }
            }
        }
        first_err
    }

    /// [`Self::flush_all`] for callers with nobody to return the error to: it
    /// is logged, once per distinct message, since the commit is retried each tick.
    pub fn flush_all_logged(&self, wait: bool, idle: Nanos) {
        let result = self.flush_idle(wait, idle);
        self.log_failure(result, "could not commit buffered writes");
    }

    /// What a file reads as through its buffer: committed size, or further
    /// if buffered writes extend it.
    pub(super) fn with_buffered(&self, mut a: Attr) -> Attr {
        if let Some(d) = self.dirty.lock().unwrap().get(&a.ino) {
            a.size = a.size.max(d.end());
            a.mtime = a.mtime.max(d.mtime);
            a.ctime = a.ctime.max(d.mtime);
        }
        a
    }

    /// The buffered bytes within `[from, to)` of `ino`'s file, as runs with where each
    /// starts, and where the buffer ends.
    pub(super) fn buffered_part(&self, ino: Ino, from: u64, to: u64) -> Option<(Runs, u64)> {
        let dirty = self.dirty.lock().unwrap();
        let d = dirty.get(&ino)?;
        let parts = (d.regions.range(..to))
            .filter_map(|(start, data)| {
                let (s, e) = (from.max(*start), to.min(start + data.len() as u64));
                (s < e).then(|| (s, data[(s - start) as usize..(e - start) as usize].to_vec()))
            })
            .collect();
        Some((parts, d.end()))
    }
}
