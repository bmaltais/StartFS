//! Buffered writes. A write is held in memory, one contiguous region per file,
//! and committed in batches: a commit re-chunks only the part of the file the
//! region touches and reuses the chunk references on either side. Memory is
//! bounded by the buffer cap plus a few chunks, whatever the file's size.
//!
//! Content-defined cuts depend only on the bytes after the previous cut, so
//! re-chunking from the first affected chunk until a new cut lands on an old
//! boundary gives the same chunks as chunking the whole file.

use super::*;
use crate::chunker::MAX_CHUNK;
use std::cell::Cell;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Default cap on a file's buffered bytes before a batch is committed.
pub const DEFAULT_WRITE_BUFFER: u64 = 8 << 20;

/// Writes held for one file: `data` is the file's bytes from `start`. While
/// `busy`, a commit of these bytes is running without the buffers' lock (it may
/// be waiting on a peer): readers still see them, writers wait for it to end.
#[derive(Clone)]
pub(super) struct Dirty {
    start: u64,
    data: Vec<u8>,
    mtime: Nanos,
    busy: bool,
    /// Commits of these bytes have failed `failures` times in a row; a background commit waits
    /// until `retry_at` before trying again.
    failures: u32,
    retry_at: Nanos,
}

impl Dirty {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

/// What a commit produced: the file's new content and size, and the tail of
/// the region it kept back in the buffer (a batch ends only on a real cut).
struct Spliced {
    content: Content,
    size: u64,
    rest: Option<Dirty>,
}

/// How long a background commit leaves a file alone after its `failures`th failure in a row:
/// doubling from a second to a minute.
fn retry_delay(failures: u32) -> Nanos {
    Duration::from_secs(1 << failures.saturating_sub(1).min(6)).as_nanos() as Nanos
}

/// Whether `new` stops referring to a chunk that `old` referred to.
fn drops_chunks(old: &Content, new: &Content) -> bool {
    let Content::Chunks(old) = old else {
        return false;
    };
    let kept: HashSet<ChunkId> = match new {
        Content::Chunks(refs) => refs.iter().map(|r| r.id).collect(),
        Content::Inline(_) => HashSet::new(),
    };
    old.iter().any(|r| !kept.contains(&r.id))
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
        let end = offset + data.len() as u64;
        let (mut tried_full, mut tried_apart) = (false, false);
        // Commit before merging, so a commit that fails rejects this write
        // and leaves the buffer as it was. A commit runs without the lock, so
        // the buffer is looked at again once it is done.
        loop {
            while dirty.get(&ino).is_some_and(|d| d.busy) {
                dirty = self.committed.wait(dirty).unwrap();
            }
            let Some(d) = dirty.get(&ino) else { break };
            if !tried_full && d.data.len() as u64 + data.len() as u64 >= self.buffer_cap() {
                tried_full = true;
                drop(dirty);
                self.commit(ino, false, true)?;
            } else if !tried_apart && !(offset <= d.end() && end >= d.start) {
                tried_apart = true;
                drop(dirty);
                self.commit(ino, true, true)?;
            } else {
                break;
            }
            dirty = self.dirty.lock().unwrap();
        }
        match dirty.get_mut(&ino) {
            Some(d) if offset >= d.start => {
                // Overwrite or extend in place: the common sequential case.
                if end > d.end() {
                    d.data.resize((end - d.start) as usize, 0);
                }
                d.data[(offset - d.start) as usize..][..data.len()].copy_from_slice(data);
                d.mtime = now;
            }
            Some(d) => {
                let mut merged = vec![0; (d.end().max(end) - offset) as usize];
                merged[(d.start - offset) as usize..][..d.data.len()].copy_from_slice(&d.data);
                merged[..data.len()].copy_from_slice(data);
                *d = Dirty {
                    start: offset,
                    data: merged,
                    mtime: now,
                    busy: false,
                    failures: 0,
                    retry_at: 0,
                };
            }
            None => {
                // The first buffered write of a file: the tick commits it once it is quiet.
                self.wake_tick();
                dirty.insert(
                    ino,
                    Dirty {
                        start: offset,
                        data: data.to_vec(),
                        mtime: now,
                        busy: false,
                        failures: 0,
                        retry_at: 0,
                    },
                );
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
        let queued: u64 = dirty.values().map(|d| d.data.len() as u64).sum();
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
        let dropped = Cell::new(false);
        let done = self.fetching_as(true, Durability::None, |t, spill| {
            let mut i = Self::file(t, ino)?;
            let Some(spliced) = self.splice(&i, &d, last, spill)? else {
                return Ok(None);
            };
            dropped.set(drops_chunks(&i.content, &spliced.content));
            i.content = spliced.content;
            i.size = spliced.size;
            i.mtime = d.mtime;
            i.ctime = d.mtime;
            t.put(ino, &i)?;
            self.emit_set_at(t, ino, &i, d.mtime)?;
            Ok(Some(spliced.rest))
        });
        // A commit that drops chunk references stays durable, since the old chunks
        // may be collected; one that only adds them waits for the periodic sync,
        // an fsync or shutdown, like any other lazy commit.
        let done = match done {
            Ok(Some(rest)) if dropped.get() => self.sync().map(|()| Some(rest)),
            other => other,
        };
        let mut dirty = self.dirty.lock().unwrap();
        let out = match done {
            Ok(Some(rest)) => {
                match rest {
                    Some(rest) => dirty.insert(ino, rest),
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
        d: &Dirty,
        last: bool,
        spill: &mut Spill,
    ) -> FsResult<Option<Spliced>> {
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
                    rest = Some(Dirty {
                        start: at,
                        data: win[(at - sa) as usize..].to_vec(),
                        mtime: d.mtime,
                        busy: false,
                        failures: 0,
                        retry_at: 0,
                    });
                }
            }
            let committed = &pieces[..used];
            self.make_room(committed)?;
            let mut out: Vec<ChunkRef> = refs[..a].to_vec();
            let mut size = sa;
            for p in committed {
                out.push(ChunkRef {
                    id: self.keep(p)?,
                    len: p.data.len() as u32,
                });
                size += p.data.len() as u64;
            }
            out.extend_from_slice(&refs[tail_from..]);
            size += i.size - bounds[tail_from];
            return Ok(Some(Spliced {
                content: Content::Chunks(out),
                size,
                rest,
            }));
        }
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

    /// The buffered bytes for `[from, to)` of `ino`'s file, with where they
    /// start, and where the buffer ends.
    pub(super) fn buffered_part(
        &self,
        ino: Ino,
        from: u64,
        to: u64,
    ) -> Option<(u64, Vec<u8>, u64)> {
        let dirty = self.dirty.lock().unwrap();
        let d = dirty.get(&ino)?;
        let (s, e) = (from.max(d.start), to.min(d.end()));
        let part = if s < e {
            d.data[(s - d.start) as usize..(e - d.start) as usize].to_vec()
        } else {
            Vec::new()
        };
        Some((s, part, d.end()))
    }
}
