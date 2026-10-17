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
| 4 | Idle daemon under about 100 MB and negligible CPU | **Met when idle and small; not met after data is written** | `scripts/validate-idle.sh`, see below |
| 5 | Cached data at near-local-disk speed | **Partly**: large reads yes, writes **no** | `scripts/validate-bench.sh`, see below |
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
  grace (grace plus detection plus rate-limited repair at the default 8 MiB/s). B's stored
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
| Idle, empty filesystem | A | 8.7 MiB | 0.8 % |
| Idle, empty filesystem | B | 8.7 MiB | 0.8 % |
| Idle after writing a 128 MiB file and 2000 small files through A's mount | A | 93.5 MiB (peak 222) | **9.9 %** |
| Same | B | 40.3 MiB | 0.8 % |
| After B read the 128 MiB file | A / B | 93.5 / 48.9 MiB | 9.8 % / 0.8 % |

Findings, stated plainly:

- **Memory:** idle with little or no data is far under 100 MB. After a large write, the writing
  node stayed at 93.5 MiB (under the budget by a small margin) with a peak of 222 MiB while
  writing, because each write holds the whole file in memory (see section 5). A file larger
  than free RAM cannot be written at all.
- **CPU:** an empty idle daemon uses about 0.8 % of a core because the tick loop wakes every
  10 ms. That is arguably negligible but is not zero. **After data is written the idle CPU
  grows with the amount of data** (3.5 to 4.5 % after one 32 MiB file, about 10 % after 128 MiB
  and 2000 files, on the node that holds the data). That is not negligible. The tick thread is
  the one burning it (per-thread `/proc` times). From reading `daemon.rs`/`replication.rs`
  the likely cause is that every tick the replicator rebuilds and sorts its list of all tracked
  chunks; this has **not** been profiled. Not fixed here: it needs a redesign of the replicator
  poll to be event or timer driven.

## 5. Cached-data speed through the FUSE mount

`scripts/validate-bench.sh "16 32 64 128"`: one node, 1 copy, `dd bs=1M`, local baseline is a
directory on the same btrfs volume. "Warm page cache" reads hit the kernel cache for the
baseline but FUSE reads here always go through the daemon; `O_DIRECT` removes that difference.

| Size | Op | Local | FUSE | Ratio |
|---|---|---|---|---|
| 16 MiB | write | 0.006 s | 0.259 s | 43x slower |
| 32 MiB | write | 0.010 s | 0.716 s | 72x |
| 64 MiB | write | 0.017 s | 2.593 s | 153x |
| 128 MiB | write | 0.037 s | 10.197 s | 276x |
| 128 MiB | read, page cache | 0.009 s | 0.144 s | 16x |
| 128 MiB | read, `O_DIRECT` | 0.108 s | 0.138 s | 1.3x |
| 64 MiB | read, `O_DIRECT` | 0.086 s | 0.070 s | 0.8x |
| 1000 x 8 B | create and write | 0.030 s | 2.46 s | 82x (2.5 ms per file) |
| 1000 x 8 B | read | 0.011 s | 0.053 s | 4.8x |

- **Reads of cached data are near disk speed** when both sides bypass the page cache (0.8x to
  1.9x, about 0.9 GB/s). Compared with a warm kernel page cache FUSE reads are 7x to 16x slower
  in wall time, still about 0.9 GB/s in absolute terms, which is fast in practice.
- **Writes are a real defect.** Time quadruples when the size doubles (0.26, 0.72, 2.6, 10.2 s),
  i.e. quadratic. The cause is confirmed in the code: `LocalFs::write` calls `read_all`, patches
  the buffer and calls `store`, which re-chunks and re-hashes the whole file on every FUSE write.
  At 128 MiB the rate is about 12 MiB/s and falling; a 1 GiB file would take on the order of
  10 to 15 minutes by extrapolation (not measured). Consequences measured in the same runs:
  - peak daemon RSS 206 to 222 MiB for a 128 MiB write (whole file in memory, more than once);
  - storage amplification: each write leaves its superseded tail chunks behind until GC (7 days
    by default). A 32 MiB file occupied 74 MiB, a 20 MiB file occupied about 120 MiB (and was
    replicated at that size), a 128 MiB file plus small files occupied 297 MiB.
- Small-file creation costs 2.5 ms each through FUSE; each op is a redb transaction plus an
  operation-log entry. Not investigated further.

Criterion 5 ("normal operations on cached data at near-local-disk speed") is therefore met for
reading cached data and for small writes, and **not met for writing files beyond a few MiB**.

## Defects found, not fixed

Neither fix is small or clearly scoped, so both are left for their own tickets:

1. **Quadratic write path** (section 5): needs write buffering or chunk-level patching in
   `LocalFs::write`, with a flush on close/fsync, or at least reusing unchanged leading chunks.
2. **Idle CPU grows with stored data** (section 4): the tick-time replicator scan.

Both are called out in `README.md` under known limits.

## Reproducing

```sh
scripts/validate-setup.sh
scripts/validate-loss.sh 3
scripts/validate-idle.sh 30 128
scripts/validate-bench.sh "16 32 64 128"
```

Needs `/dev/fuse` and `fusermount3`, `python3`, and a Rust toolchain. State is kept under
`~/.cache/sfv.*` (short paths: the control socket path has a length limit) and removed on exit.
