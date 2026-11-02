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
- **Repeat reads come from the kernel's page cache.** `open` used to drop the kernel's cached
  pages every time. It now keeps them while the file's size, mtime and ctime are those of the
  last open (`FuseAdapter::op_open`), so a change here or by a peer still drops them. Reading a
  128 MiB file a second time through the mount: 0.048 s before, 0.017 s after (`ab.sh`, two
  rounds, same on a third read). The first read after a write is still cold.
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
transaction. Now recently read chunks stay decoded in memory (`read_cache_bytes`, 32 MiB when measured; now derived from memory) and
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
on), so this single-node benchmark cannot resolve the cost: the sum is within noise (-0.8%). It has
no peers, so it does not cover the per-message counting. That was measured on three loopback nodes
(3 copies, a fixed-seed `make-tree.py` tree of 3000 small and 150 x 1 MiB files, 6 runs each,
alternating, `activity` on or off on all three nodes, CPU from `/proc/PID/stat` over the copy until
staging drained, 274 MiB per run):

| Median over 6 runs | `activity` on | `activity` off |
|---|---|---|
| CPU of the writer's daemon | 9.47 s | 9.60 s |
| CPU of a receiving daemon | 3.15 s | 3.11 s |
| both together | 12.62 s | 12.71 s (on is -0.7%) |
| time until staging drained | 9.0 s | 10.3 s |

Single runs vary by 3 to 6% either way, so the counters cost less than the benchmark can see;
that meets the "under 1%" criterion as far as it can be measured, and the default stays on.

Observed on three loopback nodes (3 copies, one frozen with SIGSTOP, `make-tree.py` default tree
copied into another): the tick averaged 43 ms with 42.8 ms in the replicate phase and was busy
82 to 88% of the minute, while the frozen peer's path carried only 30 KiB/s out and every one of the
4,200 queued chunks timed out about every 5 s (47,804 timeouts in 60 s). The live peer took 20 MiB/s
at first and 2 MiB/s once the tick was saturated. This is a finding to follow up, not fixed here.

## 7. Applying a peer's burst of metadata (#85, partly)

`reconcile` rebuilt the whole tree (every directory entry, name and inode) for each applied
message, so a message cost writes in proportion to the tree, not to its entries. Replaying 4,000
created files into a second node in 5-entry messages (a throwaway test around `catch_up` and
`apply_remote`): 63,321 write syscalls and 9.0 s with one rebuild per message, 9,452 and 0.17 s with
one rebuild per 128 messages.

The daemon now rebuilds once per receive drain and applies each peer's entries as one message.
On three loopback nodes (`scripts/profile-apply.sh`, 4,200 files, 3 copies, `sudo strace -f -y -e
trace=pwrite64,fdatasync` on one receiver, single runs), `pwrite64` calls to `meta.redb`: 5,788 on
main, 3,355 now (`fdatasync` 331, 207). That is 1.7x fewer, short of the order of magnitude #85
asks for, and the copy time did not change.

With the disk capped (`sudo systemd-run --scope -p IOWriteIOPSMax=/dev/mapper/root2 500`, 1,550
files) the copy took 97 s on main and 99 s now, against about 5 s uncapped. A plain copy is then
limited by the chunk files (one write and one fsync each, 4,189 per receiver for 4,200 files), not
by `meta.redb`. The live pattern (almost only `meta.redb` writes) followed a rolling restart, so
#85 stays open until that case is reproduced and fixed.

Still, `reconcile` rewrote every directory entry, name and inode on each rebuild, so even one
rebuild per tick cost writes in proportion to the tree. Applying one new file on a node that holds
N files, write syscalls (a throwaway test around `catch_up` and `apply_remote`):

| files held | 2,000 | 8,000 | 20,000 |
|---|---|---|---|
| rewrite everything | 87 | 316 | 779 |
| write only what differs | 17 | 20 | 22 |

On the 500 IOPS live disk 779 writes is about 1.5 s for one entry per tick, and the live tree held
26,719 objects. That is the likely source of the live flood: a tick that applies entries always pays
the whole tree. The rebuild still reads the whole tree (CPU), but the disk writes follow the change.
Receiver `meta.redb` `pwrite64` for the 4,200-file copy on three loopback nodes (single runs under
`strace`): 5,788 before #87, 3,355 after it, 1,724 and 1,538 with this change.

With the disk capped to 500 IOPS and 60 MB/s (`scripts/profile-apply.sh`, `TRACE=1`, 1,000 small and
20 large files, one run per build): `meta.redb` `pwrite64` 3,913 on the build before #87 and 2,392
now; copy time 85 s and 91 s; tick average 48 and 51 ms; inbox 7 and 0 entries at the end. A tree
that small does not stall either build, and the copy is limited by the chunk fsyncs, so the
issue's acceptance check (tick under a second, inbox near 0 on a 4,000-file burst) cannot tell the
builds apart on loopback. The evidence for the fix is the table above; the live check needs a
node with a tree of about 26,000 objects.

### The live disks

The live VPSes (vps1, vps2) are Azure Standard_B2als_v2 (2 vCPU, 4 GiB; the VM allows 3,750
uncached IOPS and 119 MiB/s, so the VM is not the limit) with an Azure Standard HDD (256 GiB): a hard limit of 500 IOPS and
60 MB/s, so the 540 writes/s at 200 ms seen in #85 is the disk's ceiling, not drained burst credits
(no CPU steal either). Slow-disk runs cap the device with `IOWriteIOPSMax=... 500` and
`IOWriteBandwidthMax=... 60M` (see `scripts/profile-restart.sh`).

`scripts/profile-restart.sh` (3 loopback nodes, 1,550 files, node B down for the second half of the
copy, capped at 500 IOPS, `LAG=1`, three runs per build): `meta.redb` `pwrite64` while B catches up
829, 861 and 895 before #87 and 426, 439 and 440 after; B still ends `DEGRADED` after 150 s on both
(CPU 50 to 100 s, tick 78 to 90% busy). The restart cost is the rescan (#82) and the tick (#74), and
it does not reproduce the live `meta.redb` flood (about 7,500 `pwrite64` in 6 s). One uncapped
restart of an idle cluster writes almost nothing to `meta.redb` and sends about 400 "do you hold
this chunk?" messages a second.

Re-check after a restart, 1.10.15 (2026-11-01). A restarted node asks the other holders of every
chunk it holds whether they still hold it: 4 messages per chunk with 3 copies (an ask and an answer
to each of the 2 others; counted in the harness at 500 and 2,000 chunks). `scripts/profile-restart.sh`
(3 loopback nodes, uncapped, done when B has heard everyone, has nothing queued or re-checking and
reads SAFE; until 1.10.15 it stopped at the SAFE a node reads before its peers' hellos arrive):

| files (about one chunk each) | 1,550 | 6,050 | 24,050 |
|---|---|---|---|
| seconds until settled | 1.2 | 2.4 | 4.5 |

About 0.15 ms a chunk past a second of fixed cost, so 200,000 chunks would take about half a minute.
Live, vps1 (8,827 chunks, Azure Standard HDD) had nothing left to re-check at its first sample,
22 s after `systemctl --user restart startfs`.

## 8. A new empty member joins (#78, #91 step 1)

`scripts/profile-join.sh`: four loopback nodes (10 GiB offers, 3 copies) hold 4,000 small files plus
100 files of 1 MiB; a fifth node joins with a 100 GiB offer. `CAP=1` runs the joiner like the live
vps2 (2 vCPU, 4 GiB, 500 IOPS, 60 MB/s). Resident memory of the joiner, single runs:

| | main (1.5.1) | trim | trim + bounds |
|---|---|---|---|
| joiner peak (`VmHWM`) | 811 to 976 MiB | 1,023 MiB | **149 to 187 MiB** |
| joiner after it settled | 780 to 840 MiB | 44 MiB | 30 to 44 MiB |
| the four existing nodes, peak | about 40 MiB | about 40 MiB | 79 to 93 MiB |
| seconds until `SAFE` | 19 | 19 | 21 |

Where the memory was: `mallinfo2` every 100 ms on the joiner showed *live* bytes climbing to about
800 MiB in 3 s and draining in 2.5 s (the inbox: nothing bounded it), and after that the allocator
holding 840 MiB of free pages with 24 MiB in use (`gdb` `malloc_stats()`: 363 MB in ten arenas;
`malloc_trim(0)` took the resident size from 343 to 44 MiB). Three changes:

- `malloc_trim(0)` from the tick every 2 s returns freed pages (`allocator.rs`).
- A receiver stops reading a chunk message while the inbox holds more than 64 MiB; QUIC flow
  control slows the sender.
- The replicator withholds a chunk push while the transport holds 32 MiB for that peer
  (`Transport::backlog`); without it the memory only moved to the senders (peak 191 to 410 MiB).

Tests: `a_slow_link_holds_no_more_chunk_bytes_than_the_backlog_limit` (harness; 69 MB queued to one
peer without the limit) and `a_receiver_that_falls_behind_holds_a_bounded_inbox_and_still_hears_metadata`
(real QUIC over loopback).

## 9. The metadata database's cache size (#67)

redb's default page cache is 1 GiB, so a large metadata file could fill that much anonymous memory.
`LocalFs::open` now sets 64 MiB (`METADATA_CACHE_BYTES`, a constant, not a setting: the live
cluster's metadata file is 43 MB, and a miss only costs a page read). `scripts/profile-write.sh`,
one run each, wall time through the FUSE mount:

| workload | default 1 GiB | 64 MiB |
|---|---|---|
| 128 MiB dd | 0.324 s | 0.307 s |
| 1000 x 8 B | 0.325 s | 0.321 s |
| 1000 x 16 KiB | 0.649 s | 0.646 s |
| 200 x 1 MiB | 0.433 s | 0.430 s |

No difference: this workload's metadata is smaller than either cache. It does not exercise a
metadata file larger than 64 MiB, which is where reads would start missing.

## 10. Transfer speed between nodes on a high-latency path

`scripts/profile-transfer.sh`, two nodes, a 256 MiB incompressible file, latency added to loopback
with `tc netem` (`RTT_MS`, needs `sudo`); `BIN` compares binaries. Two directions, median MiB/s
over the runs (3 to 6 each):

- **replicate** (`MODE=replicate`, 2 copies): A writes the file through its mount; the time is until
  it is fully replicated on B.
- **read** (`MODE=read`, 1 copy, B offers no space): B reads a file that only A holds, through its
  mount with `O_DIRECT`, so every chunk comes from A.

Changes compared: **base** (1.8.1), **buffers** (larger QUIC windows and UDP socket buffers),
**both** (buffers plus read-ahead and parallel chunk fetch).

| RTT | Direction | base | buffers | both |
|---|---|---|---|---|
| 0 | replicate | 123 | 107 (noise) | |
| 10 ms | replicate | 37.9 | 62.6 | 59.2 |
| 10 ms | read | 33.3 | 46.2 | **106.5** |
| 20 ms | replicate | 32.6 | 47.9 | 51.4 |
| 20 ms | read | 25.4 | 38.1 | **87.4** |
| 50 ms | replicate | 32.5 | 55.5 | |
| 50 ms | read | 14.5 | | **60** |
| 150 ms | replicate | 25.9 | 39.8 | |

Read-ahead does not touch the replication path, so its replicate columns are the same code as
buffers (the differences are noise). A blank cell was not measured.

- The replicator was not the limit: during the 50 ms transfer all of the file's ~100 chunks were
  in flight at once and the rate stayed flat, so the cap was the connection.
- Raising only the QUIC flow-control windows (8 MiB per stream, 64 MiB per connection) changed
  little (30 against 33 MiB/s at 50 ms). Asking for 8 MiB UDP socket buffers (the kernel grants up
  to `net.core.rmem_max`, 4 MiB here, against a 208 KB default) is what moved the rate: datagram
  drops at the receiving socket were making the congestion controller back off.
- BBR (`quinn::congestion::BbrConfig`) was tried and rejected: replication fell to about
  0.1 MiB/s on the same path.
- Remote reads fetched one chunk per round trip, so the rate was a chunk per RTT. A read that
  starts at or before a missing chunk now also requests the next chunks (up to 8, 8 MiB) in the
  same round trip, spread over the peers that are up, and keeps them in the read cache; a read that
  lands inside a chunk fetches only that chunk. `tests/read_ahead.rs` covers both.
- Runs vary a lot (the same replication took 4 to 8 s), so read the figures as ranges. All of this
  is one machine with delay-only netem (no loss, no jitter); real links were not measured.
  Read-ahead was not measured on random access beyond the harness test, nor with more than one
  peer holding the data.

**Hedged reads (#185).** Before, a read whose nearest peer had gone silent waited out the whole
reply wait (300 rounds: 3 s on a real daemon, 300 ms of simulated time in the harness) before asking the next peer. Now a peer
that has not answered after 3 times its usual round trip (at least 20 ms; 200 ms if never timed)
is asked in parallel with the next one, and the first good answer wins. Harness test only, not yet measured on a live link
(`read_order.rs`, nearest peer 5 ms away then silent, others 30 ms): the read takes under 150 ms
of simulated time, and the silent peer drops down the order within a few reads. Nothing is held
beyond the existing reply bookkeeping: a late reply is pruned like any unclaimed one.

**Read-ahead on the same terms (#185).** The chunks fetched along with the first are now asked of
the members placement gives them to (`likely_holders`; nodes do not track holders, but placement
is a function of the membership), not of every peer in turn, and the first chunk is fetched the
hedged way. A peer slow to answer a chunk nobody is waiting for is given up on after 4 times its
patience, not waited out chunk by chunk, and not asked of a second peer (only the chunk the
read waits for is hedged). A likely holder that is down is asked after the peers that are up. Harness (`read_ahead.rs`): with 1 copy over 4 peers no
request is turned away (2 were before); with the nearest of 3 peers silent, a read of a file
none of whose chunks are held takes under 150 ms of simulated time (658 ms before). Two nodes,
one holder, `profile-transfer.sh` `MODE=read`, 20 ms, 128 MiB, 2 rounds against 1.13.0: 78 to
108 MiB/s now, 35 to 112 before; same within noise, which is expected with one holder. Not
measured with a slow or silent peer on a live link.

## 11. Short-lived files and the replication hold

`scripts/profile-churn.sh`: three loopback nodes (3 copies); on A, 50 files of 1 MiB of fresh
random bytes are written, kept for the lifetime, then deleted. What the other two daemons wrote
(`/proc/PID/io`) and the replica bytes they keep after (deleted chunks stay for the GC retention):

| lifetime | hold 0: wrote / kept | hold 30 s (default): wrote / kept |
|---|---|---|
| 1 s | 116 / 100 MiB | 10 / 0 MiB |
| 5 s | 115 / 100 MiB | 11 / 0 MiB |
| 15 s | 116 / 100 MiB | 11 / 0 MiB |
| 45 s | 116 / 100 MiB | 118 / 100 MiB |

Without a hold even a file deleted after a second was pushed to both peers and kept there for the
retention (the write buffer and the sweep of unreferenced staged chunks only catch what is deleted
before the first push). What the peers write for a file deleted within the hold is the metadata
of its create and delete.

## 12. Write amplification during a build (#165)

`scripts/profile-amplification.sh` on one node through a real mount, bytes the daemon wrote per
byte of payload (`/proc/PID/io`), 1.13.1 against the fix. A build was reported at about 12x (42 GiB
written, 3.6 GiB stored); these steps reproduce its two causes.

| Step | 1.13.1 | Fixed | Bound |
|---|---|---|---|
| write 300 objects (20 MiB) | 1.09x | 1.08x | 2x |
| replace them (relink: `>` truncates first) | 7.07x | 1.30x | 2x |
| append 400 x 4 KiB, reopening the file each time | 202x | 1.33x | 3x |
| patch 50 files by 14 B | about 1 MiB per file | same | none |

Each `setattr` (every `>` on an existing file, `touch`, `cp -p`) was a durable commit: an fsync
and about 55 KiB of metadata pages, plus a read and re-store of the old content. It is lazy now,
except a resize that stores new chunks. Each close of a file that already has chunks committed it,
which stored the whole tail chunk again; it now waits for `write_commit_delay_ms` of quiet. A
patch still stores one new chunk (up to 1 MiB) per patched file, the cost of the chunk size. The
script exits non-zero when a bounded step goes over its bound (with `write_commit_delay_ms = 0`
in `NODE_CONFIG`, the append step fails at 202x).

## 13. A burst of chunks pushed to one member (#166)

`scripts/profile-burst.sh`: two loopback nodes, 2 copies, 4000 files of 64 KiB (half text, half
random) copied through A's mount; time until every chunk is on B, and B's `activity`. 1.13.2
against the fix, one run each (replication time varied by under 5% over repeats).

| B's disk | 1.13.2 | Fixed |
|---|---|---|
| this machine (fsync 0.24 ms, btrfs on NVMe) | 8.9 s | 5.2 to 5.5 s |
| +2 ms on every fsync (`SLOW_FSYNC_US=2000`) | 19.7 s | 8.6 s |

Before, a push cost the receiver one fsync, one create and one rename on its single tick thread
(system CPU 0.47 ms per chunk, user 0.38 ms), and each of them waited for the disk in turn. It now
writes a group of up to 64 pushes without syncing, syncs once (the flushes overlap), then answers
them all. Compression and the second hash of each chunk were measured and are small (about 0.1 ms
and 0.03 ms per chunk). Storing pushes lazily without grouping them was slower than before (each
chunk paid a dirty marker and a later reopen to sync it), which is why the answers wait for the
group. `a_burst_of_pushes_acknowledged_survives_a_power_cut_at_the_receiver` guards that nothing
acknowledged is lost.

On the live cluster (1.13.2, a cold build of gliff on the mount, 2026-11-01): mini1 reached 97% busy
with a receive phase of 237 ms per tick, 896 messages in its inbox and a longest tick of 2.2 s; vps2
100% busy, 252 ms, 808 messages; this node, sending, had 1349 sends time out and 1347 retried.
After 1.13.3 (same sampler; the build cache shortcuts a rerun, so the loads differ):

| Build | Written | mini1 busy, receive, inbox | vps2 busy, receive, inbox | Sends timed out |
|---|---|---|---|---|
| release, 1.13.2 (above) | 966 MB in 105 s | 97%, 237 ms, 896 | 100%, 252 ms, 808 | 1349 |
| release, 1.13.3 | 548 MB in 34 s | 62%, 83 ms, 296 | 72%, 132 ms, 693 | 25 |
| debug, 1.13.3 | 1.3 GB in 61 s | 100%, 368 ms, 1446 | 100%, 286 ms, 1681 | 5101 |

So the fix helps under a moderate load and does not stop a heavy one from saturating the weak
members. On a third 1.13.3 build (debug, 658 MB) the receiver's tick thread used about 38% of one
core (mini1 36%, vps2 40%), 95% of it user time: a tick reported as busy spends most of its time
waiting for the CPU or the disk less than it looks: sampled ten times a second during a 130 s
window with the build, the tick thread was running about 20% of the samples and in disk waits about
10%, and the disk was busy 9 to 12% of the window (io pressure stalls under a second in total).

Neither the disk nor the CPU is the limit, and what remains is a cost that follows the size of the
tree. `PRE=30000 scripts/profile-burst.sh` first fills the filesystem with 30,000 tiny files, then
runs the same 4000-chunk burst (single runs, tick thread CPU during the burst only):

| tree on B | replicated | receive phase | longest tick | tick thread CPU |
|---|---|---|---|---|
| empty | 5.4 s | 18 ms | 72 ms | 2.3 s user, 0.9 s system |
| 30,000 files | 10.4 s | 48 ms | 313 ms | 7.6 s user, 0.6 s system |

Twenty thousand tiny files alone (no chunks) cost the receiver 14.5 s of user CPU, 0.7 ms per file.
Section 7 says the same thing: each tick that applies entries rebuilds the tree by reading all of
it, and the live one holds more than 26,000 objects. That is the next thing to fix for a build to
stop saturating the weak members; the chunk sync of this section does not touch it (fixed in
1.13.4, #210; the rest of the story follows).

The tree rebuild was made incremental (1.13.4, #210) and the writer's own saturation fixed (1.13.6),
after which the receivers were the limit again. A throwaway lab (writer and four peers on loopback,
20,000 small files written through the writer's mount, each node's log kept apart) showed why: every
member passes the writer's entries on to every other, so a receiver got each entry about four times,
and every batch, even one of entries it already held, cost it a durable commit. 63% of its batches
applied nothing, at 1.7 ms each. 1.13.8 (#221) skips the commit for such a batch and applies a tick's
batches from all peers in one commit:

| Receiver during the burst | 1.13.7 | 1.13.8 |
|---|---|---|
| tick busy | 0.86 to 0.88 | 0.56 to 0.58 |
| receive phase | 7.5 to 8.5 ms | 1.9 to 2.0 ms |

Live, the same three-minute uncached gliff build on the mount (`scripts/sample-activity.sh`):

| Release | mini1 busy, receive, inbox | vps2 busy, receive, inbox | Sends timed out |
|---|---|---|---|
| 1.13.7 | 99%, 165 ms, 515 | 100%, 339 ms, 4027 | 2414 |
| 1.13.8 | 97%, 105 ms, 132 | 100%, 331 ms, 376 | 106 |
| 1.13.9 | 96%, 70 ms, 91 | 99%, 240 ms, 83 | 0 |

A synced 8 KiB write takes 6.2 ms on vps2's disk and 5.4 ms on mini1's (`dd oflag=dsync`), and with
5 ms added to every fsync in the lab all the receiver's remaining syncs were metadata commits, one per
tick with new entries. The next release applies peers' entries lazily and makes them durable in
groups (a tick that receives nothing, or every 100 ms); hellos and the entries a member passes on
stop at its durable frontier. Lab, receiver with +5 ms per fsync and a 25% CPU cap, two runs each:

| | 1.13.8 | lazy apply |
|---|---|---|
| tick busy | 0.86, 0.87 | 0.70, 0.70 |
| receive phase | 14.3, 13.2 ms | 3.4, 3.2 ms |
| fsyncs over the burst | 1752, 1818 | 1227, 1150 |
| every member converged | 20.8, 20.5 s | 18.0, 17.2 s |

`what_a_member_claimed_survives_a_power_cut_there` guards that a power cut never takes back what a
member's hellos or passed-on entries claimed (the harness's `power_cut` now loses lazy metadata
commits, as redb does after an unclean stop).

1.13.9 is the lazy apply: in the live row above the receivers keep up with the build (inboxes under
100, no sends timed out; both builds compiled the same 202 crates in the three minutes). #166 and
#210 are closed on it. Their peak busy is still 96 to 99%; what that time goes to during a build is
not broken down, since the sampler keeps only the peak.

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
