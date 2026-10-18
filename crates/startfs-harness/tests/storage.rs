//! Chunking, dedup, compression and verification, observed through the
//! filesystem-operations interface (file contents and `stats`, the store's `df`).

use startfs_core::ops::*;
use startfs_harness::Cluster;

/// Deterministic incompressible bytes.
fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

#[test]
fn tiny_files_are_inlined_and_use_no_chunk() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    put(&fs, "tiny", &[7u8; 100]);
    assert_eq!(fs.stats().unwrap().chunks, 0);
}

#[test]
fn small_files_take_the_single_chunk_fast_path() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let data = random(60 * 1024, 1);
    let ino = put(&fs, "small", &data);
    assert_eq!(fs.stats().unwrap().chunks, 1);
    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn large_files_are_split_into_megabyte_scale_chunks_and_read_back() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let data = random(12 * 1024 * 1024, 2);
    let ino = put(&fs, "big", &data);
    let chunks = fs.stats().unwrap().chunks;
    // 12MB with 1-4MB chunks: at least 3, at most 12.
    assert!((3..=12).contains(&chunks), "chunks = {chunks}");

    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), data);
    // A read straddling chunk boundaries.
    let off = 3 * 1024 * 1024 - 10;
    assert_eq!(
        fs.read(ino, off as u64, 5_000_000).unwrap(),
        &data[off..off + 5_000_000]
    );
    // Reading past the end is short, and beyond it empty.
    assert_eq!(
        fs.read(ino, data.len() as u64 - 3, 100).unwrap(),
        &data[data.len() - 3..]
    );
    assert!(fs.read(ino, data.len() as u64 + 5, 100).unwrap().is_empty());
}

#[test]
fn identical_content_is_stored_once() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let data = random(6 * 1024 * 1024, 3);
    put(&fs, "a", &data);
    let once = fs.stats().unwrap();
    put(&fs, "b", &data);
    put(&fs, "c", &data);
    assert_eq!(fs.stats().unwrap(), once);

    let small = random(10_000, 4);
    put(&fs, "s1", &small);
    let with_small = fs.stats().unwrap();
    put(&fs, "s2", &small);
    assert_eq!(fs.stats().unwrap(), with_small);
}

#[test]
fn editing_a_large_file_stores_only_the_changed_chunk() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let mut data = random(16 * 1024 * 1024, 5);
    let ino = put(&fs, "big", &data);
    let before = fs.stats().unwrap().chunks;

    fs.write(ino, 8 * 1024 * 1024, b"a small edit in the middle")
        .unwrap();
    data[8 * 1024 * 1024..][..26].copy_from_slice(b"a small edit in the middle");

    let after = fs.stats().unwrap().chunks;
    assert!(after - before <= 2, "{} new chunks", after - before);
    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn compressible_data_shrinks_and_incompressible_data_is_stored_as_is() {
    let c = Cluster::start(1);
    let fs = c.fs(0);

    put(&fs, "zeros", &vec![0u8; 1024 * 1024]);
    let zeros = fs.stats().unwrap();
    assert!(zeros.stored_bytes < 10_000, "stored {}", zeros.stored_bytes);

    let data = random(100_000, 6);
    put(&fs, "noise", &data);
    let noise = fs.stats().unwrap().stored_bytes - zeros.stored_bytes;
    // Raw payload plus only a tiny fixed envelope; no compression overhead.
    assert!((100_000..=100_016).contains(&noise), "stored {noise}");
}

#[test]
fn corrupted_chunks_are_detected_and_never_returned() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let data = random(50_000, 7);
    let ino = put(&fs, "f", &data);
    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), data);

    c.corrupt_chunks(0);
    let err = fs.read(ino, 0, u32::MAX).unwrap_err();
    assert_eq!(err.errno(), 5); // EIO
}

#[test]
fn rewriting_a_file_across_the_inline_and_chunk_thresholds_keeps_content() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let ino = put(&fs, "grow", b"tiny");
    let big = random(200_000, 8);
    fs.write(ino, 0, &big).unwrap();
    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), big);
    fs.setattr(
        ino,
        SetAttr {
            size: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), &big[..10]);
}

#[test]
fn large_data_survives_a_crash_and_restart() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let data = random(5 * 1024 * 1024, 9);
    put(&fs, "persist", &data);
    c.crash(0);
    c.restart(0);
    let f = fs.lookup(ROOT_INO, "persist").unwrap();
    assert_eq!(fs.read(f.ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn status_reports_dedup_and_compression_efficiency() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    c.control(0).create("home", 1).unwrap();

    let text = vec![b'a'; 1024 * 1024];
    let noise = random(100_000, 3);
    put(&fs, "text", &text);
    put(&fs, "text-copy", &text);
    put(&fs, "noise", &noise);
    put(&fs, "tiny", &[7u8; 100]);
    c.sync();

    let e = c.control(0).status().unwrap().efficiency;
    let size = (2 * text.len() + noise.len() + 100) as u64;
    assert_eq!(e.inline_bytes, 100);
    assert_eq!(e.logical_bytes, size);
    // The copy shares every chunk with the original.
    assert_eq!(e.referenced_bytes, (text.len() + noise.len()) as u64);
    // One node holds the only copy, so it holds everything.
    assert_eq!(e.local_plain_bytes, e.referenced_bytes);
    assert!(e.local_disk_bytes < e.local_plain_bytes / 5, "{e:?}");
    assert!(e.zstd_chunks >= 1, "{e:?}");
    assert!(e.raw_chunks >= 1, "{e:?}");
    assert_eq!(e.zstd_chunks + e.raw_chunks, fs.stats().unwrap().chunks);
}

#[test]
fn space_reports_logical_used_bytes_and_the_clusters_usable_size() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    c.control(0).create("home", 1).unwrap();
    put(&fs, "text", &vec![b'a'; 1024 * 1024]);
    put(&fs, "tiny", &[1u8; 100]);
    c.sync();

    let space = fs.space().unwrap();
    assert_eq!(space.used_bytes, 1024 * 1024 + 100);
    let usable = c.control(0).status().unwrap().usable_bytes;
    assert!(usable > 0);
    assert_eq!(space.total_bytes, usable);
}
