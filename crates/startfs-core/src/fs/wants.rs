//! How many copies each chunk is wanted on, when directories have copies of their own.
//!
//! The filesystem's number of copies is the default. A directory with an entry (say `/scratch`,
//! at one copy) holds the files below it, down to the next directory with an entry, to its own
//! number. A chunk is content, not a place: several files can use it, so it is wanted on the
//! most copies any of them asks for. A chunk a file outside every entry also uses is wanted on
//! at least the default.
//!
//! Without entries none of this runs and every chunk is wanted on the default.

use super::*;
use std::collections::{BTreeMap, HashSet};

/// The copies setting this node works from.
#[derive(Default)]
pub(super) struct Policy {
    pub default: u32,
    pub dirs: BTreeMap<String, u32>,
}

/// What the last look at the tree found.
#[derive(Default)]
pub(super) struct Wants {
    /// The tree generation it was worked out at.
    pub generation: Option<u64>,
    /// For each chunk a file under an entry uses: the most copies those files ask for, and
    /// whether a file outside every entry uses it too.
    pub chunks: HashMap<ChunkId, (u32, bool)>,
    /// Chunks whose wanted copies changed since the replicator last asked.
    pub changed: HashSet<ChunkId>,
}

impl Wants {
    fn of(&self, id: &ChunkId, default: u32) -> u32 {
        match self.chunks.get(id) {
            Some(&(most, outside)) if outside => most.max(default),
            Some(&(most, _)) => most,
            None => default,
        }
    }
}

impl LocalFs {
    /// Work from this default number of copies and these directory entries from now on.
    pub fn set_copies_policy(&self, default: u32, dirs: &BTreeMap<String, u32>) {
        let mut policy = self.policy.lock().unwrap();
        if policy.default == default && policy.dirs == *dirs {
            return;
        }
        policy.default = default;
        policy.dirs = dirs.clone();
        // Looked at again at the next refresh.
        self.wants.lock().unwrap().generation = None;
    }

    /// Whether directories have copies of their own, or did when the tree was last looked at
    /// (so that taking the last one away is noticed).
    pub fn has_dir_copies(&self) -> bool {
        !self.policy.lock().unwrap().dirs.is_empty()
            || !self.wants.lock().unwrap().chunks.is_empty()
    }

    /// How many copies the chunk is wanted on, as the last [`Self::refresh_used`] found.
    pub fn wanted_copies(&self, id: &ChunkId, default: u32) -> u32 {
        self.wants.lock().unwrap().of(id, default)
    }

    /// The chunks whose wanted copies changed since this was last called.
    pub fn take_changed_wants(&self) -> Vec<ChunkId> {
        std::mem::take(&mut self.wants.lock().unwrap().changed)
            .into_iter()
            .collect()
    }

    /// Work out which chunks are wanted on which number of copies, from the tree.
    pub(super) fn work_out_wants(
        &self,
        t: &Tables<'_>,
        dirs: &BTreeMap<String, u32>,
    ) -> FsResult<HashMap<ChunkId, (u32, bool)>> {
        // The entries that name a directory the tree has.
        let mut named: HashMap<Ino, u32> = HashMap::new();
        for (path, copies) in dirs {
            let mut dir = Some(ROOT_INO);
            for part in path.split('/').filter(|p| !p.is_empty()) {
                dir = match dir {
                    Some(d) => t.entry(d, part)?,
                    None => None,
                };
            }
            if let Some(ino) = dir.filter(|ino| *ino != ROOT_INO)
                && t.inode(ino)?.kind == FileKind::Dir
            {
                named.insert(ino, *copies);
            }
        }
        if named.is_empty() {
            return Ok(HashMap::new());
        }
        let mut under: HashMap<Ino, Option<u32>> = HashMap::new();
        let mut chunks: HashMap<ChunkId, (u32, bool)> = HashMap::new();
        let mut entries = Vec::new();
        for row in t.dirents.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            entries.push((k.value().0, v.value()));
        }
        // First the files under an entry, then those outside every entry that use one of
        // their chunks.
        for outside in [false, true] {
            for &(parent, ino) in &entries {
                let copies = copies_under(t, &named, &mut under, parent)?;
                if copies.is_some() == outside {
                    continue;
                }
                let node = t.inode(ino)?;
                let Content::Chunks(refs) = node.content else {
                    continue;
                };
                for r in refs {
                    match (copies, chunks.get_mut(&r.id)) {
                        (Some(n), Some(c)) => c.0 = c.0.max(n),
                        (Some(n), None) => {
                            chunks.insert(r.id, (n, false));
                        }
                        (None, Some(c)) => c.1 = true,
                        (None, None) => {}
                    }
                }
            }
        }
        Ok(chunks)
    }

    /// Keep what was worked out, and note the chunks whose wanted copies it changed.
    pub(super) fn keep_wants(
        &self,
        generation: u64,
        chunks: HashMap<ChunkId, (u32, bool)>,
        default: u32,
    ) {
        let mut wants = self.wants.lock().unwrap();
        let before = std::mem::take(&mut wants.chunks);
        let old = Wants {
            chunks: before,
            ..Default::default()
        };
        let new = Wants {
            chunks,
            ..Default::default()
        };
        let ids: HashSet<ChunkId> = old
            .chunks
            .keys()
            .chain(new.chunks.keys())
            .copied()
            .collect();
        wants.changed.extend(
            ids.into_iter()
                .filter(|id| old.of(id, default) != new.of(id, default)),
        );
        wants.chunks = new.chunks;
        wants.generation = Some(generation);
    }
}

/// The copies of the nearest directory with an entry at or above `dir`, if any.
fn copies_under(
    t: &Tables<'_>,
    named: &HashMap<Ino, u32>,
    seen: &mut HashMap<Ino, Option<u32>>,
    dir: Ino,
) -> FsResult<Option<u32>> {
    if let Some(known) = seen.get(&dir) {
        return Ok(*known);
    }
    let found = match named.get(&dir) {
        Some(n) => Some(*n),
        None if dir == ROOT_INO => None,
        None => copies_under(t, named, seen, t.inode(dir)?.parent)?,
    };
    seen.insert(dir, found);
    Ok(found)
}
