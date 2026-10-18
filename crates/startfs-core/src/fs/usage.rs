//! Where the space went: logical size against what dedup and compression
//! leave, and the usable size the cluster reports through `df`.

use super::*;
use crate::control::Efficiency;
use std::collections::{HashMap, HashSet};

impl LocalFs {
    /// The cluster's usable size, kept current by the daemon.
    pub fn set_usable_bytes(&self, bytes: u64) {
        self.usable_bytes.store(bytes, Ordering::SeqCst);
    }

    /// Bytes the files in the tree add up to. Cheaper than [`Self::efficiency`],
    /// for callers such as `df` that poll.
    pub fn logical_bytes(&self) -> FsResult<u64> {
        self.flush_all_logged(false, 0);
        self.run(false, |t| {
            let mut total = 0;
            for row in t.inodes.iter().map_err(db)? {
                let inode = decode(row.map_err(db)?.1.value())?;
                if inode.kind == FileKind::File {
                    total += inode.size;
                }
            }
            Ok(total)
        })
    }

    /// Logical size against the chunks the tree references, and how those
    /// chunks are stored on this node.
    pub fn efficiency(&self) -> FsResult<Efficiency> {
        // Count what has been written so far, buffered or not.
        self.flush_all_logged(false, 0);
        let mut out = Efficiency::default();
        let mut referenced: HashMap<ChunkId, u64> = HashMap::new();
        self.run(false, |t| {
            for row in t.inodes.iter().map_err(db)? {
                let inode = decode(row.map_err(db)?.1.value())?;
                if inode.kind != FileKind::File {
                    continue;
                }
                out.logical_bytes += inode.size;
                match inode.content {
                    Content::Chunks(refs) => {
                        referenced.extend(refs.into_iter().map(|r| (r.id, u64::from(r.len))));
                    }
                    Content::Inline(bytes) => out.inline_bytes += bytes.len() as u64,
                }
            }
            Ok(())
        })?;
        out.referenced_bytes = referenced.values().sum();
        let held: HashSet<ChunkId> = referenced.keys().copied().collect();
        for (id, info) in self.chunks.survey()? {
            if !held.contains(&id) {
                continue;
            }
            out.local_plain_bytes += info.plain_bytes;
            out.local_disk_bytes += info.disk_bytes;
            if info.zstd {
                out.zstd_chunks += 1;
            } else {
                out.raw_chunks += 1;
            }
        }
        Ok(out)
    }
}
