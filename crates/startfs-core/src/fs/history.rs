//! Versioned history: the states an overwrite or a delete replaced.
//!
//! A version is kept for the filesystem's window from when it was replaced
//! (ADR 0008). Versions are derived from log entries like the merge registers,
//! so members holding the same entries hold the same versions; they live in
//! their own table so that rebuilding the tree never decodes them.

use super::merge::{Object, Version};
use super::*;
use crate::chunkstore::ChunkId;
use crate::control::VersionInfo;
use std::collections::HashSet;
use std::time::Duration;

/// A state that lasted less than this is not kept: the files a build writes and
/// deletes within the replication hold still never leave the node.
pub const MIN_VERSION_LIFE: Duration = Duration::from_secs(30);

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

/// One earlier version of an object, and the stamp of what replaced it.
#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Past {
    pub(super) version: Version,
    pub(super) until: Hlc,
}

type Key = (u64, u64, u32, u32);

fn key(ino: Ino, h: Hlc) -> Key {
    (ino, h.wall, h.counter, h.node.0)
}

fn every(ino: Ino) -> std::ops::RangeInclusive<Key> {
    (ino, 0, 0, 0)..=(ino, u64::MAX, u32::MAX, u32::MAX)
}

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_stdvec(v).expect("serialises")
}

fn dec<'a, T: Deserialize<'a>>(raw: &'a [u8]) -> FsResult<T> {
    postcard::from_bytes(raw).map_err(|e| FsError::Io(format!("bad record: {e}")))
}

/// Whether a state that began at `start` and ended at `end` is kept at `now`.
fn kept(start: Hlc, end: Hlc, now: Nanos, window: Nanos) -> bool {
    end.wall.saturating_sub(start.wall) >= MIN_VERSION_LIFE.as_nanos() as u64
        && end.wall.saturating_add(window) > now
}

impl Tables<'_> {
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

    /// Forget every version of an object that is itself forgotten.
    pub(super) fn forget_versions(&mut self, ino: Ino) -> FsResult<()> {
        self.history
            .retain_in(every(ino), |_, _| false)
            .map_err(db)?;
        Ok(())
    }

    /// The chunks of every version kept at `now`: the earlier versions of
    /// objects, and the last version of each deleted one. Versions no longer
    /// kept are forgotten on the way when `prune` is set.
    pub(super) fn version_chunks(
        &mut self,
        now: Nanos,
        window: Nanos,
        prune: bool,
    ) -> FsResult<HashSet<ChunkId>> {
        let mut out = HashSet::new();
        let mut gone = Vec::new();
        for row in self.history.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let past: Past = dec(v.value())?;
            if !kept(past.version.hlc, past.until, now, window) {
                gone.push(k.value());
            } else if let Content::Chunks(refs) = past.version.content {
                out.extend(refs.into_iter().map(|r| r.id));
            }
        }
        if prune {
            for k in gone {
                self.history.remove(k).map_err(db)?;
            }
        }
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            if k.value() & (1 << 63) != 0 {
                continue; // a conflict copy's own register
            }
            let o: Object = dec(v.value())?;
            let (Some(death), Some(last)) = (o.death(), o.winner()) else {
                continue;
            };
            if kept(last.hlc, death, now, window)
                && let Content::Chunks(refs) = &last.content
            {
                out.extend(refs.iter().map(|r| r.id));
            }
        }
        Ok(out)
    }
}

/// One state the file at a path has had: its version and how it ended.
pub(super) struct Span {
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
        let mut out = Vec::new();
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let ino = k.value();
            if ino & (1 << 63) != 0 {
                continue;
            }
            let o: Object = dec(v.value())?;
            if o.kind.is_none_or(|k| k == FileKind::Dir) {
                continue;
            }
            let here = o
                .names
                .values()
                .filter_map(|n| n.loc.as_ref())
                .any(|(_, p, n)| *p == dir && n == name);
            if !here {
                continue;
            }
            for past in self.versions_of(ino)? {
                if kept(past.version.hlc, past.until, now, window) {
                    out.push(Span {
                        version: past.version,
                        end: Some(past.until),
                        deleted: false,
                    });
                }
            }
            let Some(last) = o.winner() else { continue };
            match o.death() {
                None => out.push(Span {
                    version: last.clone(),
                    end: None,
                    deleted: false,
                }),
                Some(d) if kept(last.hlc, d, now, window) => out.push(Span {
                    version: last.clone(),
                    end: Some(d),
                    deleted: true,
                }),
                Some(_) => {}
            }
        }
        out.sort_by_key(|s| s.version.hlc);
        Ok(out)
    }
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

    /// Set how long a replaced version is kept (the filesystem's window).
    pub fn set_keep_versions(&self, window: Duration) {
        self.keep_versions
            .store(window.as_nanos() as u64, Ordering::SeqCst);
    }

    /// The window, in nanoseconds.
    pub(super) fn window(&self) -> Nanos {
        self.keep_versions.load(Ordering::SeqCst)
    }
}
