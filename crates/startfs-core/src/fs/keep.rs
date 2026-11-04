//! Keep local: paths whose chunks this member holds in its cache whatever else it reads.
//!
//! The cache is least-recently-used and bounded, so a directory read once a month is soon
//! fetched again from a peer. Kept paths are the exception: their chunks are pinned in the cache
//! (see [`crate::cache::ChunkCache::set_pinned`]) and fetched now if this member lacks them.
//! A chunk this member already holds as a replica is local already and is not copied. The paths
//! are this member's own choice: they are not gossiped, and live in `keep-local` in its data
//! directory.

use super::*;
use std::collections::{BTreeSet, HashSet};

#[derive(Default)]
pub(super) struct Kept {
    pub paths: BTreeSet<String>,
    /// The tree generation the pins were last worked out at.
    pub generation: Option<u64>,
}

/// What keeping a path found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeepReport {
    /// Chunks the path's files use.
    pub chunks: u64,
    /// Fetched from a peer just now.
    pub fetched: u64,
    /// Not held here and not fetched: no peer that holds one answered.
    pub missing: u64,
}

fn normal(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

impl LocalFs {
    /// The paths this member keeps local.
    pub fn kept_paths(&self) -> Vec<String> {
        self.kept.lock().unwrap().paths.iter().cloned().collect()
    }

    /// Read back the paths kept by an earlier run.
    pub(super) fn read_kept(dir: &std::path::Path) -> BTreeSet<String> {
        std::fs::read_to_string(dir.join("keep-local"))
            .map(|text| {
                text.lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn write_kept(&self, paths: &BTreeSet<String>) -> FsResult<()> {
        let text: String = paths.iter().map(|p| format!("{p}\n")).collect();
        std::fs::write(self.dir.join("keep-local"), text).map_err(|e| FsError::Io(e.to_string()))
    }

    /// Keep the file or directory at `path` local, or with `keep` false let it go. Its chunks
    /// are pinned in the cache and the ones this member lacks are fetched, which waits on peers.
    pub fn keep_local(&self, path: &str, keep: bool) -> FsResult<KeepReport> {
        let path = normal(path);
        let wanted = if keep {
            Some(
                self.run(false, |t| chunks_under(t, &path))?
                    .ok_or(FsError::NotFound)?,
            )
        } else {
            None
        };
        {
            let mut kept = self.kept.lock().unwrap();
            if keep {
                kept.paths.insert(path.clone());
            } else {
                kept.paths.remove(&path);
            }
            self.write_kept(&kept.paths)?;
        }
        self.pin_kept(true);
        let mut report = KeepReport::default();
        for id in wanted.unwrap_or_default() {
            report.chunks += 1;
            if self.has_replica(&id) || self.has_staged(&id) || self.cache.touch(&id) {
                continue;
            }
            match self.fetch_remote(&id) {
                Ok(_) => report.fetched += 1,
                Err(_) => report.missing += 1,
            }
        }
        Ok(report)
    }

    /// Pin the chunks of every kept path, if the tree has changed since (or `force`).
    fn pin_kept(&self, force: bool) {
        let generation = self.tree_generation.load(Ordering::SeqCst);
        let paths: Vec<String> = {
            let kept = self.kept.lock().unwrap();
            if !force && (kept.generation == Some(generation) || kept.paths.is_empty()) {
                return;
            }
            kept.paths.iter().cloned().collect()
        };
        let pinned = self.run(false, |t| {
            let mut ids = HashSet::new();
            for path in &paths {
                ids.extend(chunks_under(t, path)?.unwrap_or_default());
            }
            Ok(ids)
        });
        match pinned {
            Ok(ids) => {
                self.cache.set_pinned(ids);
                self.kept.lock().unwrap().generation = Some(generation);
            }
            Err(e) => eprintln!("startfs: cannot work out which chunks to keep: {e}"),
        }
    }

    /// Follow the tree: a file added under a kept directory is pinned too.
    pub fn refresh_kept(&self) {
        self.pin_kept(false);
    }

    /// Drop the cached chunks that are not kept; returns the bytes freed. Replicas this member
    /// is responsible for are not touched: that is a matter of placement.
    pub fn free_space(&self) -> u64 {
        self.cache.free_unpinned()
    }
}

/// The chunks of the file at `path`, or of every file below the directory there; `None` when
/// nothing is there.
fn chunks_under(t: &Tables<'_>, path: &str) -> FsResult<Option<HashSet<ChunkId>>> {
    let mut at = ROOT_INO;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        at = match t.entry(at, part)? {
            Some(ino) => ino,
            None => return Ok(None),
        };
    }
    let mut ids = HashSet::new();
    let mut todo = vec![at];
    while let Some(ino) = todo.pop() {
        let node = t.inode(ino)?;
        if node.kind == FileKind::Dir {
            for row in t.dirents.range((ino, "")..(ino + 1, "")).map_err(db)? {
                todo.push(row.map_err(db)?.1.value());
            }
        } else if let Content::Chunks(refs) = node.content {
            ids.extend(refs.into_iter().map(|r| r.id));
        }
    }
    Ok(Some(ids))
}
