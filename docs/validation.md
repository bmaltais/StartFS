# v1 success-criteria validation

Measured on one Linux machine (btrfs, 12 cores, FUSE mounts, release build), all daemons on
loopback with a faked `tailscaled` whois. Numbers come from the scripts in `scripts/`, which
build the release binary and run real `startfs daemon` processes. Rerun them to reproduce
(each takes under two minutes except `validate-idle.sh`, about four).

## Verification matrix

| # | Criterion | Result | Evidence |
|---|---|---|---|
| 1 | Setup under 5 min, about 3 commands | **Met** on loopback | `scripts/validate-setup.sh` |
| 2 | Permanent node loss returns data to N copies, no user action | **Met** (real processes and harness) | `scripts/validate-loss.sh`; `repair.rs::after_the_grace_period_every_chunk_is_back_to_n_copies_without_user_action` |
| 3 | No data loss when any N-1 nodes fail, once replicated | **Met** (real processes and harness) | `scripts/validate-loss.sh`; `repair.rs::any_n_minus_one_nodes_can_fail_once_replication_completed` |
| 4 | Idle daemon under about 100 MB and negligible CPU | **Met**, idle CPU no longer grows with stored data | `scripts/validate-idle.sh`, see below |
| 5 | Cached data at near-local-disk speed | **Partly**: reads and writes scale roughly linearly; writes still 6x to 9x slower than local disk | `scripts/validate-bench.sh`, see below |
| 6 | Install docs for Linux and macOS | Done; macOS untested | `README.md`, `dist/README.md` |

Not verifiable here: real multiple machines, a real tailnet (whois was faked; the real
`tailscaled` API path is covered only by a fake in tests), real network latency or loss,
macOS (the adapter has never run), and long-term behaviour (days of uptime).

## 1. Setup time and command count

`scripts/validate-setup.sh`: daemon on A, `create`, daemon on B, `join`, until both list 2 nodes.

- Wall time for the whole flow: **about 0.1 s**, against a 5 minute budget. Join is one round
  trip; the time on real machines is dominated by typing the token across.
- Commands: `startfs daemon` and `startfs create` on the first machine, `startfs daemon` and
  `startfs join <token>` on the second: **4 commands, 2 per machine**. With the daemon installed
  as a service (`dist/startfs.service`) the user types `create` and `join`, plus one
  `systemctl --user enable --now startfs` per machine; so "about 3" holds on a service install
  only if you count the service enable once. Installation itself (build, copy the binary) is
  extra and is not timed.
- The script also checks that a file written on A is readable on B through the mounts.

## 2 and 3. Loss and repair

`scripts/validate-loss.sh`: three daemons, 2 copies, a 20 MiB file, `repair_grace_secs = 3`.
`kill -9` of node C, nothing else done by the user.

- Replication of the file completed in 0.5 s.
- Node C shown as `lost` and the file back to full redundancy on A and B: **14.5 s** with a 3 s
  grace (grace plus detection plus rate-limited repair, measured at the former default of 8 MiB/s). B's stored
  data went from 55 MiB to 121 MiB, so data was really copied. The default grace is 30 minutes.
- Then `kill -9` of A as well, leaving only B: the file read from B's mount matched the original
  (sha256). That is 2 of 3 nodes failed with N = 2, after replication had completed.
- The harness tests (in-process clusters with a fake clock) cover the rest: tiers, rate limit,
  repair order, rejoin trimming, `leave`/`remove`, and every combination of N-1 failures.

Limits: loopback, one machine, one file, 3 nodes. No partitions or flaky links.

## 4. Idle memory and CPU

`scripts/validate-idle.sh 30 128`: two daemons (2 copies), RSS and CPU from `/proc`, 30 s windows.

| State | Node | RSS | CPU (one core) |
|---|---|---|---|
| Idle, empty filesystem | A | 10.6 MiB | 0.5 % |
| Idle, empty filesystem | B | 10.8 MiB | 0.5 % |
| Idle after writing a 128 MiB file and 2000 small files through A's mount | A | 97.7 MiB (peak 226) | **0.7 %** |
| Same | B | 41.7 MiB | 0.7 % |
| After B read the 128 MiB file | A / B | 97.7 / 50.3 MiB | 0.7% / 0.7% |

Before the idle-tick fix the same run measured A at 9.9 % (idle after the writes) and 9.8 %
(after B's read), B at 0.8 %, and 0.8 to 0.9 % for the empty filesystem. Numbers move a little
between runs, and the machine was shared with other builds while measuring, so read them to
within a few tenths of a percent.

Findings, stated plainly:

- **Memory:** idle with little or no data is far under 100 MB. After a large write, the writing
  node stayed at about 98 MiB (right at the budget) with a peak of 226 MiB while writing,
  because each write held the whole file in memory (fixed since, see section 5; this row was not
  re-measured).
- **CPU, fixed:** the empty idle daemon uses about 0.5 to 1 % of a core because the tick loop
  wakes every 10 ms; that is the floor. Before the fix idle CPU grew with stored data (about
  10 % at 128 MiB and 2000 files). Timing the phases of the tick confirmed the cause: the
  replicator took about 12 % of a core, nearly all of it listing the whole staging directory
  tree from disk every tick (and the log exchange about 2 %). The replicator now rescans staging
  only when a counter says a chunk was added or removed, and skips chunks that are settled, so a
  tick with nothing to do no longer touches the disk. After the fix an idle node holding the
  same data uses about the same CPU as an empty one. The remaining cost of the tick (log
  exchange, 10 ms wakeups) does not measurably grow with this amount of data; a much larger
  tree was not measured.

## 5. Cached-data speed through the FUSE mount

`scripts/validate-bench.sh "16 32 64 128"`: one node, 1 copy, `dd bs=1M`, local baseline is a
directory on the same btrfs volume. "Warm page cache" reads hit the kernel cache for the
baseline but FUSE reads here always go through the daemon; `O_DIRECT` removes that difference.

| Size | Op | Local | FUSE | Ratio |
|---|---|---|---|---|
| 16 MiB | write | 0.006 s | 0.053 s | 8.8x slower |
| 32 MiB | write | 0.011 s | 0.067 s | 6.1x |
| 64 MiB | write | 0.017 s | 0.136 s | 8.0x |
| 128 MiB | write | 0.038 s | 0.255 s | 6.7x |
| 128 MiB | read, page cache | 0.011 s | 0.157 s | 14x |
| 128 MiB | read, `O_DIRECT` | 0.159 s | 0.152 s | 1.0x |
| 64 MiB | read, `O_DIRECT` | 0.082 s | 0.078 s | 1.0x |
| 1000 x 8 B | create and write | 0.030 s | 0.158 s | 5.3x (0.16 ms per file) |
| 1000 x 16 KiB | `cp -r`, chunk-backed | 0.030 s | 0.34 s | about 11x (0.34 ms per file) |
| 1000 x 8 B | read | 0.011 s | 0.061 s | 5.5x |

The tiny-file row was 2.5 to 3.7 s before create and close stopped waiting for disk: each file cost
two metadata commits at two fsyncs each. Now only `fsync(2)`, a periodic sync and shutdown make
metadata durable, so a crash can lose the last seconds of closed but unfsynced files.

FUSE write times are the range over four runs on a shared machine; the spread is large, the
trend is not. Before write buffering the write rows were 0.259, 0.716, 2.593 and 10.197 s (43x to 276x
slower than local disk) and daemon peak RSS was 206 to 222 MiB for a 128 MiB write.

- **Reads of cached data are near disk speed** when both sides bypass the page cache (0.8x to
  1.9x, about 0.9 GB/s). Compared with a warm kernel page cache FUSE reads are 7x to 16x slower
  in wall time, still about 0.9 GB/s in absolute terms, which is fast in practice.
- **Writes are now roughly linear.** FUSE writes are buffered per file and committed in batches
  that re-chunk only the affected region (`crates/startfs-core/src/fs/writebuf.rs`). Across the
  four runs, 128 MiB took 4x to 9x as long as 16 MiB (8x is linear), where before it took 39x.
  Individual doublings vary between 1.3x and 2.6x with machine load, so a single
  doubling can exceed 2x. The rate was about 150 to 200 MiB/s then and is 300 to 500 MiB/s now, 6x to 9x slower than
  the local disk's page cache. Daemon peak RSS over the
  whole run, 128 MiB write included, was 57 to 65 MiB, bounded by the buffer cap rather than the
  file size. The harness tests `writes.rs::a_file_written_in_small_pieces_occupies_about_its_own_size`
  and `background_ticks_do_not_commit_a_file_that_is_still_being_written` show a 20 MiB file
  written in 128 KiB pieces storing about 20 MiB (it was 243 MB before), so superseded chunks are
  no longer created or replicated; storage use through a real mount was not measured.
- **Write-path profile.** A `strace -c` of the daemon during a copy found the time went to the
  disk, not to hashing or compression. Before the fixes below, 1000 files of 16 KiB took 4.5 s
  (4.5 ms each, 4.6 s of kernel CPU in the daemon): each file made about 700 `getdents64` and 350
  `openat` calls, from `ChunkStore::stats` walking the whole store on every write and every chunk
  promotion, and from the replicator listing the staging directory on every tick; and each file
  made about 2 chunk `fsync`s and 1 redb `fdatasync` before `close` returned. Now the store
  counts its chunks as it changes, the replicator follows additions, locally written chunks are
  synced with the commit that needs them (`LocalFs::sync`, the quiet-period tick, or any durable
  commit), and a commit that only adds chunks is lazy like any create. The same copy takes
  0.34 s, and a 128 MiB `dd` 0.26 to 0.50 s (it was 0.6 to 0.98 s). The cost moved to the later
  sync: data is on disk about `sync_quiet_ms` after `close`, not at `close`.

**Read and write path tuning.** `O_DIRECT` reads of a 256 MiB file through the mount, before and
after (one run each, shared machine). Before, every FUSE read loaded, decompressed and hashed the
whole 1 to 4 MiB chunk it touched, and every metadata read took the global lock and a write
transaction. Now recently read chunks stay decoded in memory (`read_cache_bytes`, 32 MiB) and
reads use read-only redb snapshots.

| Read, incompressible / text | Before | After |
|---|---|---|
| sequential, 128 KiB blocks | 1.48 s / 2.28 s | 0.17 s / 0.16 s |
| sequential, 4 KiB blocks, 16 MiB | 2.97 s / 8.15 s | 0.09 s / 0.09 s |
| sequential, 1 MiB blocks | 0.26 s / 0.39 s | 0.12 s / 0.14 s |
| random 4 KiB, 2000 reads over 100 MiB | 1.42 s / 4.10 s | 1.44 s / 2.86 s |

Random reads over a file larger than the cache still decode a chunk per miss. Writes of 256 MiB
plus `sync` (first write only; the second is deduplicated), seconds and cumulative disk use:

| Data | `always` (old behaviour) | `auto` (default) | `off` |
|---|---|---|---|
| random | 0.99 s | 0.84 s | 0.79 s |
| 6% compressible | 0.95 s, saves 6% | 0.79 s, stored as is | 0.71 s |
| text | 1.02 s, 7.5x smaller | 1.04 s, 7.5x smaller | 0.74 s |

Each chunk is also hashed once instead of three times on write, and chunk fsyncs of a sync run on
up to 8 threads (not separately measured).

Criterion 5 ("normal operations on cached data at near-local-disk speed") is met for reading
cached data. Writes no longer degrade with file size, but remain about 6x to 9x slower than
the local disk, so the criterion is only partly met.

## 6. Cost of the activity counters

`startfs activity` counts every message and times every tick (`activity = true`, the default).
`scripts/profile-write.sh` with `NODE_CONFIG='activity = false'` against the default, 8 runs each
alternating on and off, one node, 1 copy, median wall time of the FUSE side:

| Write | `activity` on | `activity` off | on vs off |
|---|---|---|---|
| 128 MiB `dd bs=1M` | 0.321 s | 0.313 s | +2.6% |
| 1000 x 8 B (inline) | 0.319 s | 0.321 s | -0.8% |
| 1000 x 16 KiB (chunks) | 0.649 s | 0.649 s | -0.2% |
| 200 x 1 MiB | 0.409 s | 0.429 s | -4.7% |
| sum of the four | 1.698 s | 1.712 s | -0.8% |

The runs differ by more than 1% in both directions (the fastest `dd` run was with the counters
on), so this benchmark cannot resolve the cost: the sum is within noise (-0.8%), not proven under
1%. One node has no peers, so this covers the tick timers and the window, not the per-message
counting, which was not measured separately. The default stays on on that basis; if a quieter
measurement (several nodes, `make-tree.py` copy, on vs off) shows 1% or more, the issue's rule is
to make the default off.

Observed on three loopback nodes (3 copies, one frozen with SIGSTOP, `make-tree.py` default tree
copied into another): the tick averaged 43 ms with 42.8 ms in the replicate phase and was busy
82 to 88% of the minute, while the frozen peer's path carried only 30 KiB/s out and every one of the
4,200 queued chunks timed out about every 5 s (47,804 timeouts in 60 s). The live peer took 20 MiB/s
at first and 2 MiB/s once the tick was saturated. This is a finding to follow up, not fixed here.

## Defects found, not fixed

None open. The quadratic write path found here is fixed (section 5), and so is the idle CPU
growth (section 4). The idle-memory figures in section 4 were measured before write buffering.

## Reproducing

```sh
scripts/validate-setup.sh
scripts/validate-loss.sh 3
scripts/validate-idle.sh 30 128
scripts/validate-bench.sh "16 32 64 128"
```

Needs `/dev/fuse` and `fusermount3`, `python3`, and a Rust toolchain. State is kept under
`~/.cache/sfv.*` (short paths: the control socket path has a length limit) and removed on exit.
