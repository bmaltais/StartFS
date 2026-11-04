//! A write that stores a chunk already on disk, committing while a removal of unused chunks
//! has looked but not yet removed, must not lose the chunk (the races behind #252 and #254).

use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const DAY: Duration = Duration::from_secs(24 * 3600);
const HOUR: Duration = Duration::from_secs(3600);
const SIZE: usize = 3 << 20;

/// Let `by` pass in steps, letting the cluster work after each.
fn pass(c: &Cluster, by: Duration, step: Duration) {
    let mut left = by;
    while !left.is_zero() {
        let s = step.min(left);
        c.advance(s);
        c.sync();
        left -= s;
    }
}

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

#[test]
fn a_resize_that_reuses_a_chunk_being_removed_keeps_it() {
    let c = Cluster::with_filesystem(1, 1, "");
    let fs = c.fs(0);
    let data = noise(SIZE, 1);
    // `gone` holds `data` alone; `grown` holds it followed by more, so its chunks differ
    // from `data`'s at the end. Truncating `grown` back to `SIZE` stores `data`'s last chunk.
    let gone = put(&fs, "gone", &data);
    let mut longer = data.clone();
    longer.extend(noise(SIZE, 2));
    let grown = put(&fs, "grown", &longer);
    fs.flush(gone).unwrap();
    fs.flush(grown).unwrap();
    c.sync();
    fs.unlink(ROOT_INO, "gone").unwrap();
    c.sync();

    // Everything but the last step of the retention period has passed: the next
    // collection reclaims `gone`'s chunks.
    pass(&c, 6 * DAY, 12 * HOUR);
    let raced = Arc::new(AtomicBool::new(false));
    let (hook_fs, flag) = (c.fs(0), raced.clone());
    fs.before_next_removal(move || {
        hook_fs
            .setattr(
                grown,
                SetAttr {
                    size: Some(SIZE as u64),
                    ..SetAttr::default()
                },
            )
            .unwrap();
        flag.store(true, Ordering::SeqCst);
    });
    pass(&c, 2 * DAY, 12 * HOUR);

    assert!(raced.load(Ordering::SeqCst), "no removal ran");
    assert_eq!(fs.read(grown, 0, SIZE as u32).unwrap(), data);
}
