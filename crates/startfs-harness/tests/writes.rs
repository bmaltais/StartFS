//! Buffered, chunk-incremental writes, driven only through the filesystem-operations interface.

use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};

const MIB: usize = 1 << 20;

fn read_all(fs: &impl FsOps, ino: Ino) -> Vec<u8> {
    fs.read(ino, 0, u32::MAX).unwrap()
}

#[test]
fn a_file_written_in_small_pieces_occupies_about_its_own_size() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "big", 0o644).unwrap();
    let data = noise(20 * MIB, 1);
    for (n, piece) in data.chunks(128 * 1024).enumerate() {
        assert_eq!(
            fs.write(f.ino, (n * 128 * 1024) as u64, piece).unwrap(),
            piece.len()
        );
    }
    fs.flush(f.ino).unwrap();

    assert_eq!(read_all(&fs, f.ino), data);
    let stored = fs.stats().unwrap().stored_bytes as usize;
    assert!(
        stored < data.len() * 105 / 100,
        "{stored} bytes stored for a {} byte file",
        data.len()
    );
}

fn put_in_pieces(fs: &impl FsOps, ino: Ino, at: usize, data: &[u8], piece: usize) {
    for (n, p) in data.chunks(piece).enumerate() {
        assert_eq!(fs.write(ino, (at + n * piece) as u64, p).unwrap(), p.len());
    }
}

#[test]
fn writes_are_readable_through_the_same_handle_before_any_flush() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
    let data = noise(300_000, 2);
    put_in_pieces(&fs, f.ino, 0, &data, 7_000);

    assert_eq!(read_all(&fs, f.ino), data);
    assert_eq!(
        fs.read(f.ino, 250_000, 100).unwrap(),
        &data[250_000..250_100]
    );
    assert_eq!(fs.getattr(f.ino).unwrap().size, 300_000);
    assert_eq!(fs.lookup(ROOT_INO, "f").unwrap().size, 300_000);

    // A write over committed data reads back over it, with a hole zero-filled.
    fs.flush(f.ino).unwrap();
    fs.write(f.ino, 299_990, b"0123456789ABCDEF").unwrap();
    fs.write(f.ino, 400_000, b"far").unwrap();
    let got = read_all(&fs, f.ino);
    assert_eq!(&got[..299_990], &data[..299_990]);
    assert_eq!(&got[299_990..300_006], b"0123456789ABCDEF");
    assert!(got[300_006..400_000].iter().all(|&b| b == 0));
    assert_eq!(&got[400_000..], b"far");
}

#[test]
fn overwriting_the_middle_of_a_large_file_stores_only_the_affected_chunks() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "big", 0o644).unwrap();
    let mut data = noise(32 * MIB, 3);
    put_in_pieces(&fs, f.ino, 0, &data, MIB);
    fs.flush(f.ino).unwrap();
    let before = fs.stats().unwrap();

    let patch = noise(100_000, 4);
    data[16 * MIB + 5..][..patch.len()].copy_from_slice(&patch);
    fs.write(f.ino, (16 * MIB + 5) as u64, &patch).unwrap();
    fs.flush(f.ino).unwrap();

    assert_eq!(read_all(&fs, f.ino), data);
    let after = fs.stats().unwrap();
    // The edit sits inside one chunk of 1 to 4 MiB (two if it straddles a cut).
    assert!(after.chunks - before.chunks <= 2, "{before:?} -> {after:?}");
    assert!(after.stored_bytes - before.stored_bytes <= 8 * MIB as u64);
}

/// The result of a sequence of writes, truncates and appends is the file a
/// byte-for-byte model gives, and its chunks are the ones chunking the final
/// content from scratch gives (a copy of it then adds no new chunk).
#[test]
fn random_writes_truncates_and_appends_match_whole_file_rewriting() {
    for seed in 1..=6u64 {
        let c = Cluster::start(1);
        let fs = c.fs(0);
        let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
        let mut model: Vec<u8> = Vec::new();
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |bound: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as usize % bound.max(1)
        };
        for step in 0..30 {
            match next(10) {
                0 => {
                    let size = if next(2) == 0 {
                        next(model.len() + 1)
                    } else {
                        next(14 * MIB)
                    };
                    fs.setattr(
                        f.ino,
                        SetAttr {
                            size: Some(size as u64),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    model.resize(size, 0);
                }
                1 => fs.flush(f.ino).unwrap(),
                kind => {
                    let len = match next(3) {
                        0 => next(3_000),
                        1 => next(200_000),
                        _ => next(3 * MIB),
                    };
                    let at = match kind {
                        2..=5 => model.len(),
                        6..=7 => next(model.len() + 1),
                        _ => model.len() + next(100_000),
                    };
                    let bytes = noise(len, seed * 1000 + step);
                    fs.write(f.ino, at as u64, &bytes).unwrap();
                    if model.len() < at + len {
                        model.resize(at + len, 0);
                    }
                    model[at..at + len].copy_from_slice(&bytes);
                }
            }
            assert_eq!(read_all(&fs, f.ino), model, "seed {seed} step {step}");
        }
        fs.flush(f.ino).unwrap();
        assert_eq!(read_all(&fs, f.ino), model, "seed {seed} after flush");

        let before = fs.stats().unwrap().chunks;
        let copy = fs.create(ROOT_INO, "copy", 0o644).unwrap();
        fs.write(copy.ino, 0, &model).unwrap();
        fs.flush(copy.ino).unwrap();
        assert_eq!(
            fs.stats().unwrap().chunks,
            before,
            "seed {seed}: different chunks"
        );
    }
}

/// A file committed while it is small enough to be stored inline, then written on past that,
/// keeps the size of what was written (it grew by the inline bytes a second time, as zeros at
/// its end: a linker then rejected an rlib that was otherwise whole).
#[test]
fn a_small_file_written_on_past_the_inline_limit_keeps_its_size() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    for (first, more_at, more) in [
        (262usize, 262usize, 45_824usize),
        (262, 0, 45_824),
        (100, 5_000, 45_824),
    ] {
        let f = fs
            .create(ROOT_INO, &format!("f-{first}-{more_at}"), 0o644)
            .unwrap();
        let mut model = noise(first, 1);
        fs.write(f.ino, 0, &model).unwrap();
        fs.flush(f.ino).unwrap();
        let bytes = noise(more, 2);
        fs.write(f.ino, more_at as u64, &bytes).unwrap();
        fs.flush(f.ino).unwrap();
        model.resize(model.len().max(more_at + more), 0);
        model[more_at..more_at + more].copy_from_slice(&bytes);
        assert_eq!(
            fs.getattr(f.ino).unwrap().size,
            model.len() as u64,
            "{first} then {more} at {more_at}"
        );
        assert!(
            read_all(&fs, f.ino) == model,
            "{first} then {more} at {more_at}: bytes"
        );
    }
}

/// Writes at scattered offsets, overlapping each other and what is committed, as a linker
/// writes: every read sees the latest bytes, and the file ends up cut into the same chunks as
/// the same bytes written in one go.
#[test]
fn scattered_overlapping_writes_read_back_and_chunk_like_one_write() {
    for seed in 1..=8u64 {
        let c = Cluster::start(1);
        let fs = c.fs(0);
        let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
        let mut model: Vec<u8> = Vec::new();
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |bound: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as usize % bound.max(1)
        };
        for step in 0..150 {
            match next(20) {
                0 => fs.flush(f.ino).unwrap(),
                1 => {
                    let from = next(model.len() + 1);
                    let len = next(6 * MIB);
                    let got = fs.read(f.ino, from as u64, len as u32).unwrap();
                    let end = (from + len).min(model.len());
                    assert_eq!(got, model[from..end], "seed {seed} step {step} read");
                }
                _ => {
                    let len = match next(4) {
                        0 => 1 + next(100),
                        1 | 2 => 1 + next(64 * 1024),
                        _ => 1 + next(MIB),
                    };
                    let at = next(24 * MIB);
                    let bytes = noise(len, seed * 1000 + step);
                    fs.write(f.ino, at as u64, &bytes).unwrap();
                    if model.len() < at + len {
                        model.resize(at + len, 0);
                    }
                    model[at..at + len].copy_from_slice(&bytes);
                }
            }
        }
        assert_eq!(read_all(&fs, f.ino), model, "seed {seed} before flush");
        fs.flush(f.ino).unwrap();
        assert_eq!(read_all(&fs, f.ino), model, "seed {seed} after flush");

        let before = fs.stats().unwrap().chunks;
        let copy = fs.create(ROOT_INO, "copy", 0o644).unwrap();
        fs.write(copy.ino, 0, &model).unwrap();
        fs.flush(copy.ino).unwrap();
        assert_eq!(
            fs.stats().unwrap().chunks,
            before,
            "seed {seed}: different chunks"
        );
    }
}

#[test]
fn a_flushed_file_replicates_and_stages_only_its_own_size() {
    const BIG: u64 = 10 << 30;
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    c.control(1).join(&token, Some(BIG)).unwrap();
    c.control(2).join(&token, Some(0)).unwrap();
    c.sync();

    let client = c.fs(2);
    let f = client.create(ROOT_INO, "big", 0o644).unwrap();
    let data = noise(20 * MIB, 5);
    put_in_pieces(&client, f.ino, 0, &data, 128 * 1024);
    client.flush(f.ino).unwrap();

    let staged = c.control(2).status().unwrap().staging;
    assert!(
        (staged.bytes as usize) < data.len() * 105 / 100,
        "{} bytes staged for a {} byte file",
        staged.bytes,
        data.len()
    );

    c.sync();
    let s = c.control(2).status().unwrap();
    assert_eq!(s.staging.chunks, 0);
    assert!(s.unreplicated.is_empty(), "{:?}", s.unreplicated);
    assert_eq!(c.fs(1).read(f.ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn background_ticks_do_not_commit_a_file_that_is_still_being_written() {
    // The harness commits on every tick by default; this asks for the real delay.
    let c = Cluster::start_with_config(1, "write_commit_delay_ms = 1000");
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "big", 0o644).unwrap();
    let data = noise(20 * MIB, 6);
    for (n, p) in data.chunks(128 * 1024).enumerate() {
        fs.write(f.ino, (n * 128 * 1024) as u64, p).unwrap();
        c.tick();
    }
    c.advance(std::time::Duration::from_secs(2));
    c.sync();

    assert_eq!(read_all(&fs, f.ino), data);
    let stored = fs.stats().unwrap().stored_bytes as usize;
    assert!(
        stored < data.len() * 105 / 100,
        "{stored} bytes stored for a {} byte file",
        data.len()
    );
}

#[test]
fn reopening_a_file_to_append_does_not_rewrite_its_tail_chunk_on_every_close() {
    let c = Cluster::start_with_config(1, "write_commit_delay_ms = 1000");
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "lib.a", 0o644).unwrap();
    let body = noise(2 * MIB, 8);
    fs.write(f.ino, 0, &body).unwrap();
    fs.flush(f.ino).unwrap();
    c.advance(std::time::Duration::from_secs(2));
    c.tick();
    let base = fs.stats().unwrap().stored_bytes;

    // `ar`, a logger or `>>` in a loop: open, append a little, close.
    let mut all = body.clone();
    for i in 0..100u64 {
        let piece = noise(4096, 100 + i);
        fs.write(f.ino, all.len() as u64, &piece).unwrap();
        fs.flush(f.ino).unwrap();
        all.extend_from_slice(&piece);
    }
    let rewritten = fs.stats().unwrap().stored_bytes - base;
    assert!(
        rewritten < 2 * MIB as u64,
        "{rewritten} bytes of chunks for 400 KiB appended"
    );

    c.advance(std::time::Duration::from_secs(2));
    c.sync();
    assert_eq!(read_all(&fs, f.ino), all);
}
