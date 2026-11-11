//! Versioned history: the states an overwrite or a delete replaced.
//!
//! A version is kept for the filesystem's window from when it was replaced
//! (ADR 0008). Versions are derived from log entries like the merge registers,
//! so members holding the same entries hold the same versions; they live in
//! their own table so that rebuilding the tree never decodes them.

use super::merge::{Object, Version, dec, enc, made_up};
use super::*;
use crate::chunkstore::ChunkId;
use crate::control::{RestoreReport, VersionInfo};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

/// A state that lasted less than this is not kept: the files a build writes and
/// deletes within the replication hold still never leave the node.
const MIN_VERSION_LIFE: Duration = Duration::from_secs(30);

/// A restore point as `history` shows it: `YYYYMMDD-HHMMSS.mmm` in UTC.
pub fn restore_point(wall: Nanos) -> String {
    format!(
        "{}.{:03}",
        super::merge::stamp(wall),
        wall % 1_000_000_000 / 1_000_000
    )
}

/// The instant a restore point names: `YYYYMMDD-HHMMSS`, optionally with up to
/// nine digits of fractions of a second, in UTC. It stands for the end of the
/// precision given, so the restore point `history` shows for a version names
/// that version even though its stamp is finer than what is shown.
pub fn parse_restore_point(text: &str) -> Option<Nanos> {
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    let (date, time) = secs.split_once('-')?;
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(date, 8) || !digits(time, 6) || frac.len() > 9 || !digits(frac, frac.len()) {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (y, m, d) = (num(&date[..4])?, num(&date[4..6])?, num(&date[6..])?);
    let (hh, mm, ss) = (num(&time[..2])?, num(&time[2..4])?, num(&time[4..])?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    // Days-from-civil (proleptic Gregorian), after Howard Hinnant.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()?;
    let unit = 10u64.pow(9 - frac.len() as u32);
    let frac = if frac.is_empty() {
        0
    } else {
        frac.parse::<u64>().ok()?
    };
    Some(secs * 1_000_000_000 + frac * unit + (unit - 1))
}

/// When versions began on a database that a release without them wrote: the
/// rollout of 1.11.0 (2026-11-01 18:00 UTC). Before it, GC did not keep a deleted
/// file's chunks for the window, so an earlier deletion cannot be a version.
const VERSIONS_BEGAN: Nanos = 1_793_556_000 * 1_000_000_000;

/// The counter holding when versions began on this database.
pub(super) const VERSIONS_SINCE: &str = "versions_since";

/// When versions begin on a database being opened: a new one has had them from
/// the start, one written by an earlier release only from [`VERSIONS_BEGAN`].
pub(super) fn versions_since(fresh: bool) -> Nanos {
    if fresh { 0 } else { VERSIONS_BEGAN }
}

/// One earlier version of an object, and the stamp of what replaced it.
#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Past {
    pub(super) version: Version,
    pub(super) until: Hlc,
}

/// A row of the history table: the object, then the version's stamp.
pub(super) type Key = (u64, u64, u32, u32);

fn key(ino: Ino, h: Hlc) -> Key {
    (ino, h.wall, h.counter, h.node.0)
}

fn every(ino: Ino) -> std::ops::RangeInclusive<Key> {
    (ino, 0, 0, 0)..=(ino, u64::MAX, u32::MAX, u32::MAX)
}

/// Whether a state that began at `start` and ended at `end` is kept at `now`.
pub(super) fn kept(start: Hlc, end: Hlc, now: Nanos, window: Nanos) -> bool {
    end.wall.saturating_sub(start.wall) >= MIN_VERSION_LIFE.as_nanos() as u64
        && end.wall.saturating_add(window) > now
}

/// Whether a deleted object's last state, written at `last` and ended by the
/// deletion at `death`, is kept: as any version, and deleted after versions began.
fn last_kept(last: Hlc, death: Hlc, since: Nanos, now: Nanos, window: Nanos) -> bool {
    death.wall >= since && kept(last, death, now, window)
}

impl Tables<'_> {
    /// When versions began on this database (see [`versions_since`]).
    fn since(&self) -> FsResult<Nanos> {
        Ok(self
            .counters
            .get(VERSIONS_SINCE)
            .map_err(db)?
            .map_or(0, |g| g.value()))
    }

    /// Keep `version` of `ino` as replaced at `until`. Recording it again keeps
    /// the earliest replacement, so the result does not depend on arrival order.
    pub(super) fn retire(&mut self, ino: Ino, version: Version, until: Hlc) -> FsResult<()> {
        let k = key(ino, version.hlc);
        let until = match self.history.get(k).map_err(db)? {
            Some(g) => dec::<Past>(g.value())?.until.min(until),
            None => until,
        };
        self.history
            .insert(k, enc(&Past { version, until }).as_slice())
            .map_err(db)?;
        Ok(())
    }

    /// A version derived from `of` stamped `by` arrived: if `of` is already
    /// kept, it was replaced no later than that.
    pub(super) fn replaced_by(&mut self, ino: Ino, of: Hlc, by: Hlc) -> FsResult<()> {
        let k = key(ino, of);
        let Some(mut past) = self
            .history
            .get(k)
            .map_err(db)?
            .map(|g| dec::<Past>(g.value()))
            .transpose()?
        else {
            return Ok(());
        };
        if by < past.until {
            past.until = by;
            self.history.insert(k, enc(&past).as_slice()).map_err(db)?;
        }
        Ok(())
    }

    /// The kept versions of `ino`, oldest first.
    pub(super) fn versions_of(&self, ino: Ino) -> FsResult<Vec<Past>> {
        self.raw_versions_of(ino)?.iter().map(|r| dec(r)).collect()
    }

    /// [`Self::versions_of`], encoded, for a state transfer.
    pub(super) fn raw_versions_of(&self, ino: Ino) -> FsResult<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for row in self.history.range(every(ino)).map_err(db)? {
            out.push(row.map_err(db)?.1.value().to_vec());
        }
        Ok(out)
    }

    /// Fold in a version a peer sent in a state transfer.
    pub(super) fn join_version(&mut self, ino: Ino, raw: &[u8]) -> FsResult<()> {
        let past: Past = dec(raw)?;
        self.retire(ino, past.version, past.until)
    }

    /// Whether a deleted object still has a version kept at `now`: its last one,
    /// or an earlier one. Its record stays while it does, so its path is known.
    pub(super) fn keeps_versions(
        &self,
        ino: Ino,
        o: &Object,
        now: Nanos,
        window: Nanos,
    ) -> FsResult<bool> {
        let since = self.since()?;
        let last = o
            .winner()
            .zip(o.death())
            .is_some_and(|(v, d)| last_kept(v.hlc, d, since, now, window));
        Ok(last
            || self
                .versions_of(ino)?
                .iter()
                .any(|p| kept(p.version.hlc, p.until, now, window)))
    }

    /// Plaintext bytes of the chunks kept versions use and the tree (`tree`) does not.
    pub(super) fn version_only_bytes(
        &mut self,
        tree: &HashSet<ChunkId>,
        now: Nanos,
        window: Nanos,
    ) -> FsResult<u64> {
        Ok(self
            .version_chunks(now, window, false)?
            .into_iter()
            .filter(|(id, _)| !tree.contains(id))
            .map(|(_, len)| len)
            .sum())
    }

    /// Forget every version of an object that is itself forgotten.
    pub(super) fn forget_versions(&mut self, ino: Ino) -> FsResult<()> {
        self.history
            .retain_in(every(ino), |_, _| false)
            .map_err(db)?;
        Ok(())
    }

    /// The chunks (and their plaintext sizes) of every version kept at `now`: the earlier versions of
    /// objects, and the last version of each deleted one. Versions no longer
    /// kept are forgotten on the way when `prune` is set.
    pub(super) fn version_chunks(
        &mut self,
        now: Nanos,
        window: Nanos,
        prune: bool,
    ) -> FsResult<HashMap<ChunkId, u64>> {
        let since = self.since()?;
        let mut out = HashMap::new();
        let mut gone = Vec::new();
        for row in self.history.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let past: Past = dec(v.value())?;
            if !kept(past.version.hlc, past.until, now, window) {
                gone.push(k.value());
            } else if let Content::Chunks(refs) = past.version.content {
                out.extend(refs.into_iter().map(|r| (r.id, u64::from(r.len))));
            }
        }
        if prune {
            for k in gone {
                self.history.remove(k).map_err(db)?;
            }
        }
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            if made_up(k.value()) {
                continue; // a conflict copy's own register
            }
            let o: Object = dec(v.value())?;
            let (Some(death), Some(last)) = (o.death(), o.winner()) else {
                continue;
            };
            if last_kept(last.hlc, death, since, now, window)
                && let Content::Chunks(refs) = &last.content
            {
                out.extend(refs.iter().map(|r| (r.id, u64::from(r.len))));
            }
        }
        Ok(out)
    }
}

/// One state the file at a path has had: its version and how it ended.
#[derive(Clone)]
pub(super) struct Span {
    pub(super) kind: FileKind,
    pub(super) version: Version,
    /// What replaced it; `None` while it is what the path holds.
    pub(super) end: Option<Hlc>,
    pub(super) deleted: bool,
}

impl Tables<'_> {
    /// The directory and name a path ends in, through deleted directories too.
    pub(super) fn locate(&self, path: &str) -> FsResult<(Ino, String)> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let (name, dirs) = parts.split_last().ok_or(FsError::Invalid)?;
        let mut dir = ROOT_INO;
        for d in dirs {
            dir = match self.entry(dir, d)? {
                Some(ino) if self.inode(ino)?.kind == FileKind::Dir => ino,
                _ => self.deleted_dir(dir, d)?.ok_or(FsError::NotFound)?,
            };
        }
        Ok((dir, (*name).to_string()))
    }

    /// The directory last deleted from `parent` under `name`.
    fn deleted_dir(&self, parent: Ino, name: &str) -> FsResult<Option<Ino>> {
        let mut found: Option<(Hlc, Ino)> = None;
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let o: Object = dec(v.value())?;
            if o.kind != Some(FileKind::Dir) || o.alive() {
                continue;
            }
            if let Some((_, (at, p, n))) = o.home()
                && *p == parent
                && n == name
                && found.is_none_or(|(f, _)| *at > f)
            {
                found = Some((*at, k.value()));
            }
        }
        Ok(found.map(|(_, ino)| ino))
    }

    /// Every kept state of the files and symlinks named `name` in `dir`, oldest first.
    pub(super) fn spans(
        &self,
        dir: Ino,
        name: &str,
        now: Nanos,
        window: Nanos,
    ) -> FsResult<Vec<Span>> {
        let mut places = self.places(&HashSet::from([dir]), now, window)?;
        Ok(places.remove(&(dir, name.to_string())).unwrap_or_default())
    }

    /// The directories under `top`, `top` included, deleted ones too: each by where
    /// it was last placed.
    fn subtree(&self, top: Ino) -> FsResult<HashSet<Ino>> {
        let mut parent_of: HashMap<Ino, Ino> = HashMap::new();
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let o: Object = dec(v.value())?;
            if o.kind == Some(FileKind::Dir)
                && let Some((_, (_, parent, _))) = o.home()
            {
                parent_of.insert(k.value(), *parent);
            }
        }
        let mut dirs = HashSet::from([top]);
        loop {
            let before = dirs.len();
            for (dir, parent) in &parent_of {
                if dirs.contains(parent) {
                    dirs.insert(*dir);
                }
            }
            if dirs.len() == before {
                return Ok(dirs);
            }
        }
    }

    /// Every kept state of every file and symlink named in one of `dirs`, by
    /// directory and name, oldest first. One pass over the registers.
    fn places(
        &self,
        dirs: &HashSet<Ino>,
        now: Nanos,
        window: Nanos,
    ) -> FsResult<BTreeMap<(Ino, String), Vec<Span>>> {
        let since = self.since()?;
        let mut out: BTreeMap<(Ino, String), Vec<Span>> = BTreeMap::new();
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let ino = k.value();
            if made_up(ino) {
                continue;
            }
            let o: Object = dec(v.value())?;
            let Some(kind) = o.kind.filter(|k| *k != FileKind::Dir) else {
                continue;
            };
            let mut here: Vec<(Ino, String)> = o
                .names
                .values()
                .filter_map(|n| n.loc.as_ref())
                .filter(|(_, p, _)| dirs.contains(p))
                .map(|(_, p, n)| (*p, n.clone()))
                .collect();
            here.dedup();
            if here.is_empty() {
                continue;
            }
            let mut spans = Vec::new();
            for past in self.versions_of(ino)? {
                if kept(past.version.hlc, past.until, now, window) {
                    spans.push(Span {
                        kind,
                        version: past.version,
                        end: Some(past.until),
                        deleted: false,
                    });
                }
            }
            if let Some(last) = o.winner() {
                match o.death() {
                    None => spans.push(Span {
                        kind,
                        version: last.clone(),
                        end: None,
                        deleted: false,
                    }),
                    Some(d) if last_kept(last.hlc, d, since, now, window) => spans.push(Span {
                        kind,
                        version: last.clone(),
                        end: Some(d),
                        deleted: true,
                    }),
                    Some(_) => {}
                }
            }
            for place in here {
                let list = out.entry(place).or_default();
                list.extend(spans.iter().map(Span::clone));
            }
        }
        for list in out.values_mut() {
            list.sort_by_key(|s| s.version.hlc);
        }
        Ok(out)
    }
}

/// The state among `spans` that was current at `at`.
fn at_instant(spans: Vec<Span>, at: Nanos) -> Option<Span> {
    spans
        .into_iter()
        .filter(|s| s.version.hlc.wall <= at && s.end.is_none_or(|e| e.wall > at))
        .max_by_key(|s| s.version.hlc)
}

/// What a restore wrote: the files brought back, and those it could not
/// write because something of another kind (a directory) is there now.
#[derive(Default)]
struct Restoring {
    restored: u64,
    blocked: u64,
    /// Deleted directories brought back so far.
    revived: HashSet<Ino>,
    /// Whether entries were logged that the tree does not show yet.
    rebuild: bool,
}

impl LocalFs {
    /// The versions of the file at `path`, oldest first: those still kept and
    /// what the path holds now. Not found if the path has neither.
    pub fn history(&self, path: &str) -> FsResult<Vec<VersionInfo>> {
        let (now, window) = (self.clock.now(), self.window());
        let spans = self.run(false, |t| {
            let (dir, name) = t.locate(path)?;
            t.spans(dir, &name, now, window)
        })?;
        if spans.is_empty() {
            return Err(FsError::NotFound);
        }
        Ok(spans
            .into_iter()
            .map(|s| VersionInfo {
                written: s.version.hlc.wall,
                replaced: s.end.map(|e| e.wall),
                deleted: s.deleted,
                size: s.version.size,
            })
            .collect())
    }

    /// Bring the file at `path` back to the version it held at `at`, or for a
    /// directory (deleted or not) every file and symlink last placed under it
    /// that held a kept version then; files that came later are left alone.
    /// Each is written over what its path holds now, or created again, and the
    /// deleted directories it needs come back. These are ordinary writes, so
    /// they replicate, and what they replace is kept as a version in turn.
    /// Not found if `path` names neither a directory nor a file kept at `at`.
    pub fn restore(&self, path: &str, at: Nanos) -> FsResult<RestoreReport> {
        self.flush_all_logged(true, 0);
        let (now, window) = (self.clock.now(), self.window());
        self.run(true, |t| {
            let (dir, name) = t.locate(path)?;
            let shown = t.entry(dir, &name)?;
            let shown_dir = match shown {
                Some(ino) if t.inode(ino)?.kind == FileKind::Dir => Some(ino),
                _ => None,
            };
            let mut r = Restoring::default();
            let file = t.spans(dir, &name, now, window)?;
            if shown_dir.is_none() && !file.is_empty() {
                let span = at_instant(file, at).ok_or(FsError::NotFound)?;
                if !self.put_back(t, dir, &name, span, now, &mut r)? {
                    return Err(FsError::Exists);
                }
            } else {
                let top = match shown_dir {
                    Some(ino) => ino,
                    None => t.deleted_dir(dir, &name)?.ok_or(FsError::NotFound)?,
                };
                let dirs = t.subtree(top)?;
                for ((d, n), spans) in t.places(&dirs, now, window)? {
                    if let Some(span) = at_instant(spans, at)
                        && !self.put_back(t, d, &n, span, now, &mut r)?
                    {
                        r.blocked += 1;
                    }
                }
            }
            if r.rebuild {
                self.reconcile(t)?;
            }
            Ok(RestoreReport {
                restored: r.restored,
                blocked: r.blocked,
            })
        })
    }

    /// Write `span` back as what `name` in `dir` holds. Returns false when
    /// something of another kind is there now, so it cannot be.
    fn put_back(
        &self,
        t: &mut Tables<'_>,
        dir: Ino,
        name: &str,
        span: Span,
        now: Nanos,
        r: &mut Restoring,
    ) -> FsResult<bool> {
        if span.end.is_none() {
            return Ok(true); // the path holds it already
        }
        let v = span.version;
        let mut inode = Inode {
            kind: span.kind,
            mode: v.mode,
            nlink: 1,
            size: v.size,
            mtime: v.mtime,
            ctime: now,
            parent: dir,
            content: v.content,
            xattrs: v.xattrs,
        };
        let shown = t.inodes.get(dir).map_err(db)?.is_some() && !r.revived.contains(&dir);
        if shown && let Some(ino) = t.entry(dir, name)? {
            let current = t.inode(ino)?;
            if current.kind != span.kind {
                return Ok(false);
            }
            inode.nlink = current.nlink;
            inode.parent = current.parent;
            t.put(ino, &inode)?;
            self.emit_set(t, ino, &inode)?;
        } else if shown {
            self.add(t, dir, name, inode)?;
        } else {
            self.revive(t, dir, r)?;
            let ino = t.alloc_ino(self.node())?;
            let snap = self.snapshot(&inode);
            self.emit(
                t,
                Op::Create {
                    ino,
                    kind: span.kind,
                    parent: dir,
                    name: name.to_string(),
                    snap,
                },
            )?;
            r.rebuild = true;
        }
        r.restored += 1;
        Ok(true)
    }

    /// Bring back a deleted directory, and the deleted ones it sits in, where they
    /// last were: placing a name again after its unlink makes it live.
    fn revive(&self, t: &mut Tables<'_>, dir: Ino, r: &mut Restoring) -> FsResult<()> {
        let mut cur = dir;
        while cur != ROOT_INO && !r.revived.contains(&cur) {
            let o = t.object(cur)?;
            if o.kind != Some(FileKind::Dir) || o.alive() {
                break;
            }
            let Some((slot, (_, parent, name))) = o.home().map(|(s, l)| (s, l.clone())) else {
                break;
            };
            self.emit(
                t,
                Op::Rename {
                    ino: cur,
                    slot,
                    parent,
                    name,
                },
            )?;
            r.revived.insert(cur);
            r.rebuild = true;
            cur = parent;
        }
        Ok(())
    }

    /// Set how long a replaced version is kept (the filesystem's window).
    pub fn set_keep_versions(&self, window: Duration) {
        self.keep_versions
            .store(window.as_nanos() as u64, Ordering::SeqCst);
    }

    /// Hold every version that ended after `floor`, the instant of the oldest mark, however
    /// short the window is; `None` when there is no mark.
    pub fn set_mark_floor(&self, floor: Option<Nanos>) {
        self.mark_floor.store(floor.unwrap_or(0), Ordering::SeqCst);
    }

    /// The window, in nanoseconds: the filesystem's, or back to the oldest mark if that is longer.
    pub(super) fn window(&self) -> Nanos {
        let window = self.keep_versions.load(Ordering::SeqCst);
        match self.mark_floor.load(Ordering::SeqCst) {
            0 => window,
            floor => window.max(self.clock.now().saturating_sub(floor)),
        }
    }
}
