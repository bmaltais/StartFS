# Plan: activity view and memory bounds

Written 2026-10-18, after a large copy left the cluster showing `data: AT RISK` for a long time and
the daemon on 3090 at 2.4 GB of memory. Investigation: #65. This plan says which issues to do, in
what order, and how to test each when it is ready. Update it as issues land.

Revised the same day after the activity view (1.4.0) and the purge (1.5.0) ran on the real cluster:
the problem is bigger and different from what the plan assumed. See "What the live runs showed".

Revised again on 2026-10-19 after #85 was diagnosed and fixed (1.5.1) and vps2 was put back into the
live cluster: the weak nodes are not limited by their disk alone, and one of them took a healthy node
down. See "What the 2026-10-19 runs showed" and the new order.

## What we know (from #65, measured on `de69ac3`, before #62)

- Memory is on the **receiving** side and scales with the burst; it is not given back afterwards.
- The writer stays small even with a peer frozen, so the send queue did not stack at that scale.
- redb's cache (1 GiB default, unset in the code) is not the cause on the live node: its file is 43 MB.
- About 40% of the memory left after a burst is allocator retention; about 90 MB stayed live.
- Not established: what took 3090 to 2.4 GB. A multi-GB inbound burst is untested.
- Deleting data does not free disk for 7 days (`gc_retention_secs`, fixed at `create`) and only once
  every member's frontier has been seen, so a flapping member holds reclaim back.

## What the live runs showed (2026-10-18)

Samples: `docs/baselines/2026-10-18-live-1.4.0-burst/`. A 374 MB copy into one node of the 7-member
cluster, then a rolling upgrade of all members, then a purge.

1. **The writer, not only the receiver, grows without bound.** Daemon RSS 111 MiB to 10 GiB, not given
   back. No counted structure explains it (#78).
2. **The single tick thread is the limit, with every peer online.** 94 to 98% busy for about 6
   minutes, ~8,500 timeouts a minute on all six paths: a retry loop on a slow tick (#74).
3. **A restart makes every node re-check every chunk.** Rolling restarts of 7 nodes meant about 14
   overlapping full rescans per node and `DEGRADED` for ~7 minutes, with almost no data moved (#82).
4. **Slow cloud disks are the worst case.** vps1 and vps2 (the 100 GiB nodes that, by offer-weighted
   placement, hold the most data) hit their IOPS ceiling: ~540 writes/s at ~200 ms, the daemon issuing
   thousands of small `meta.redb` page writes a second, tick blocked in write throttling, inbox 13k
   messages and still growing 30 minutes later. They stayed `up` (hellos have their own thread since
   #62) while minutes behind, which nothing reports (#85, #84, #83).
5. **The purge works, and the gate works.** Five nodes freed ~50 to 350 MB each in under a minute and
   stayed `SAFE`; their reclaim waited for vps1 exactly as designed. The two stuck nodes never saw the
   `Purge` message because it sat in their inbox.
6. **Operating lessons.** Do not restart a struggling node (another rescan). Do not purge or upgrade
   the next node until `startfs activity` shows an empty inbox and `data: SAFE`. A node whose disk
   cannot keep up can be stopped (`systemctl --user stop startfs`) without losing data; the others
   re-replicate its chunks after the 30 minute grace period.

State of the live cluster on 2026-10-18: vps2 stopped on purpose, vps1 still stuck; the others `SAFE`.

## What the 2026-10-19 runs showed

The live disks (vps1, vps2): Azure `Standard_B2als_v2` (2 burstable vCPU, 4 GiB; the VM allows 3,750
uncached IOPS, so the VM is not the limit) with an Azure Standard HDD of 256 GiB: a hard 500 IOPS and
60 MB/s. That is the 540 writes/s at 200 ms of #85, a real ceiling and not drained burst credits. CPU
steal was 0 on both, but a drained credit bank may not show as steal; the portal metric "CPU Credits
Remaining" is what settles that, and nobody has looked at it yet.

1. **#85 was `reconcile`.** It rewrote every directory entry, name and inode on each rebuild (and, until
   #87, once per message), so applying even one entry cost writes proportional to the whole tree: 779
   write syscalls for one new file on 20,000 files, about 1.5 s on a 500 IOPS disk, on every tick that
   applied entries. Fixed in 1.5.1 (#87, #89): 22 write syscalls, flat. Not yet seen on the live nodes.
   The loopback copy and restart runs never reproduced the live flood (the copy is limited by chunk
   fsyncs, the restart by the rescan), so the proof is the one-entry test, not an end-to-end run.
2. **Upgrading the five nodes in the cluster on 1.5.1 was calm.** About 22,000 objects on fast disks:
   quiet in 1 and 2.7 minutes; `mini1` went `DEGRADED`, briefly `AT RISK` (2 chunks), with a burst of
   54,582 inbox entries, `SAFE` after 80 s.
3. **Putting vps2 back as a new empty member did not work.** Every node ran 76 to 98% busy; about
   5,300 chunks waited for a copy; `data: AT RISK` was shown while `repair` said 0 at risk. On vps2
   the tick was 91 to 99% busy with one thread at 99.9% CPU, the replicate phase 89 ms of a 102 ms
   tick (its cost grows with the queue), a queue stuck at 4,575 chunks, `cpu pressure` 39%, the daemon
   at 1.4 GiB of 3.8 GiB. The disk was idle (0 to 40 writes per 5 s), so this is CPU, not the #85
   flood. It was removed again and stopped; its old data is kept at `~/.local/share/startfs.old-20261019`.
4. **A healthy node was taken down.** `mini1` (7.8 GiB) grew to 5.6 GiB resident, filled its 4 GiB of
   swap, had one tick of 195.7 s and was `down` for about 3 minutes, and is still at 5.6 GiB with an
   empty inbox. No restart or crash. This is #78 on a receiver, and it is the most dangerous open
   item: it can take any node out.
5. **Retries add load.** A timed-out send is retried at once; peers online showed 9,108 retried
   against 15,180 acknowledged (ocilab1) and 9,174 against 12,230 (mini1).
6. **Weak nodes hold the most data.** Placement is weighted by `offer`, and the VPSes offered 100 GiB
   against 10 GiB for the desktops, so the slowest machines got the largest share. Lowering a weak
   node's offer (`startfs config --offer`) is an operating measure that needs no code, until #84.

A node cannot tell why it is slow (an empty credit bank and an IOPS cap both look like long ticks),
so the answer is to react to its own symptoms: #91.

## Order

Ranked by value on 2026-10-19: what takes a node out comes first, then what makes everything slow,
then visibility, then the rest.

| # | Issue | Why here | Depends on |
|---|---|---|---|
| 0 | #70 memory profile script | done; the measuring tool for everything below | none |
| 1 | #63 activity view, one node | done (#73, #75), released as 1.4.0, run live | none |
| 2 | #77 purge | done (#80), released as 1.5.0 (protocol 12); run live | none |
| 3 | #85 `meta.redb` write flood | the cause (`reconcile` rewriting the tree) is fixed in 1.5.1 (#87, #89, #88); keep it open until a weak node with a large tree is seen live. Still to do: the rebuild still reads the whole tree (CPU), and the local-write rebuilds are unmeasured | none |
| 4 | #78 memory growth, with #66 bound the inbound path and #91 step 1 | **first now**: `mini1` went to 5.6 GiB and dropped out; this takes healthy nodes down and is unbounded. The memory is outside the structures `activity` lists, so find it (allocator? decoded or transfer buffers? state chunks?) with `profile-memory.sh` while a member joins, then bound it by bytes | #63 |
| 5 | #74 tick saturation and retry loop, with #91 steps 2 and 3 | second: the replicate phase costs time proportional to the queue and retries add load to a node already behind; give the tick a budget, shed optional work over it, back off per peer on timeouts | none |
| 6 | #82 rescan after restart, joins and flapping | third: also covers adding a member (every node re-plans placement and rescans); pace it, re-check only what changed, and show "re-checking" instead of `AT RISK` when `repair` says 0 at risk | none |
| 7 | #91 self-protecting nodes (umbrella) | steps 1 to 3 are the items above, no protocol change; step 4 (advertise load) goes with #84 | #78, #74 |
| 8 | #83 I/O pressure and write rate in `activity`; #84 report lagging nodes and weight placement | #83 is small and safe now and feeds #91; #84 changes the hello (protocol bump), so ship it with #66 or #64; it is what finally lets a weak node hold less | #85 for the numbers; #84 with #64 |
| 9 | #67 redb cache size, #68 unclaimed chunk replies | small, independent | none |
| 10 | #69 return freed memory to the OS | decide once #78/#66 change the peak; may be part of #78 | #66 |
| 11 | #64 cluster-wide activity | needs a protocol bump (ADR 0002); would have shown vps1 as minutes behind | #63; same release as #66/#84 |

Operating measure until #84: give the weak nodes a small offer so they hold less, and add a member
only when the cluster is quiet. Not part of this order: #45, #56, #59.

## Releases and upgrades

Each step that must be seen on the real cluster needs a release first: a "Release X.Y.Z" PR (the
usual six-file bump, see the release process in the repo notes), merged, then the `vX.Y.Z` tag, which
runs the release workflow. Then upgrade **every member** with `STARTFS_VERSION=vX.Y.Z` and the
installer, not just one node, so each node can show its own `startfs activity` and the checkpoint
sees every path from both ends.

- A change to `PROTOCOL_VERSION` (probably #66 and #64) requires every member to be upgraded
  together (ADR 0002); the installer warns. State it in the release PR's Merge Danger.
- A release without a protocol bump (1.4.0, for #63) works with mixed versions, but upgrade all
  members anyway for the checkpoint.
- Upgrading restarts the daemon and its mount (#45): do it when nothing is writing, and note the
  restart next to any result, since it resets counters and memory.
- After upgrading, confirm each member runs the new version (`startfs --version`) before the test.

Releases so far for this plan: 1.4.0 (#63, no protocol change); 1.5.0 (#77 purge, **protocol 12**:
upgrade every member together, then `startfs purge` clears the test data).

How to upgrade every member (learned the hard way on 2026-10-18, when seven restarts in a row left
four nodes minutes behind):

- Upgrade one member at a time. Between members wait until, on the member just upgraded and on one
  other, `startfs activity` shows an inbox near 0 and a normal tick, and `startfs status` says
  `data: SAFE`. For a protocol bump the members cannot talk until all match, so upgrade them close
  together, but still expect a rescan on each (#82) and do not start a burst or a purge until they
  settle.
- Check the slow-disk members (the VPSes) with `cat /proc/pressure/io`; if it stays high after their
  restart, leave them alone and wait, do not restart again.
- `startfs stats` and `startfs purge` need every member responsive; a member stuck behind a long
  inbox shows "no answer" and holds the reclaim gate for the others.

## When to test

Run a checkpoint after the issue in the first column has merged and the nodes involved are
upgraded. Each checkpoint names the data it needs (see "Test data") and what must hold.

| After | Checkpoint | How | Must hold |
|---|---|---|---|
| now (before #63) | Baseline on current `main` | memory and write-path runs done 2026-10-18, see `docs/baselines/`; rerun `profile-write.sh` on a quiet machine for the #63 overhead comparison | numbers exist for `main` with #62 in |
| #63 | Instrumented burst on the real cluster | done 2026-10-18: release 1.4.0, a 374 MB copy sampled every 20 s (`docs/baselines/2026-10-18-live-1.4.0-burst/`); overhead -0.7% on 3 loopback nodes (`docs/validation.md` section 6) | met; it found #74, #78, #82, #85 |
| #85 | Throttled-disk run | done 2026-10-19 (`docs/validation.md` section 7): loopback with the device capped to 500 IOPS and 60 MB/s; a 1,000-file tree is too small to stall either build, so the proof is the one-entry-on-a-big-tree test (779 -> 22 write syscalls) | met in the test; the live check (a weak node with about 26,000 objects) is open |
| #78, #66, #91 | A member joins a capped node | `profile-memory.sh` or `profile-restart.sh` with a new empty member joining, node capped to 2 vCPU (`CPUQuota=`), 4 GiB (`MemoryMax=`) and 500 IOPS | no node above its normal memory plus the bound; no member leaves `up`; the tick stays in a normal range after a few seconds |
| #91 | Live: vps2 back in | after the items above, add vps2 alone, sample with `startfs activity` and `/proc/pressure` | vps2 `up` throughout, `data` returns to `SAFE`, no other node above its baseline |
| #82 | Rolling restart | restart each node of the loopback or real cluster in turn, sample `status` and `activity` | `data` stays `SAFE` (or says re-checking); question rate bounded by changed placement |
| #66 | Bound holds | `profile-memory.sh` at the baseline load and at 5x (`FILES=20000 BIG=1000`), both modes | receiver peak stays near the bound while the load grows 5x |
| #66 | Live catch-up | restart a node that was away, with `startfs activity` open on a peer | inbound bytes stay under the bound; RSS does not climb past baseline plus the bound |
| #67, #68 | Regression | `profile-write.sh`, harness tests | no slowdown; tests pass |
| #69 | Allocator decision | `profile-memory.sh` with and without the malloc settings | adopt only if settled memory falls and writes lose 1% or less |
| #64 | Cluster view | upgrade every member, run `activity --cluster` during a `make-tree.py` copy | each node's own view matches its row of the matrix; a stalled peer shows as a path where the ends disagree |
| all of the above | Closing run | one more burst on the real cluster with a fresh seed | no node above its baseline plus the bound; `data` returns to SAFE; staging drains |

Not part of this plan: #45 (upgrade without dropping the mount), #56 (size the decoded-chunk cache,
untriaged; triage it with #66), #59 (sites).

## How to test each issue when it is ready

Behaviour is tested through the cluster harness (`crates/startfs-harness`) and the process seam
(`crates/startfs/tests/`); see `AGENTS.md`. `scripts/check.sh` must pass for every issue. Below is what
else to run, and what counts as done.

### #63 activity view

1. **Harness:** known traffic over a simulated minute gives exactly the expected rates per peer
   path; a down member has a row with its timeouts; the window reports how long it has collected.
2. **Process test:** `startfs activity` runs against a real daemon; `startfs config --activity off`
   disables the counters and the view says so; figures with no OS source print "unavailable".
3. **Overhead:** `scripts/profile-write.sh` with `activity` on and off. Under 1% slower on the write
   benchmark, otherwise the default becomes off. Record the numbers in `docs/validation.md`.
4. **On the real cluster** (after upgrading a node): start a copy of **new** data of a similar shape
   (many small folders; see "Retesting on real data" below), then run `startfs activity` every
   minute. Check against the readings the skill section lists: tick-busy, per-peer timeouts, queue
   length, memory pressure. Every reading must be explainable from what you see in `status`.
5. **Skill:** `skills/startfs/SKILL.md` has the activity section, written from the output of step 4,
   not from guesses.

### #66 bounded inbound path

1. **Harness / transport seam:** a receiver whose loop is stalled holds at most the bound in
   inbound bytes, and the sender slows down; a frame claiming more than its cap is refused before the
   allocation; a normal message under the bound behaves as before.
2. **Memory script**, on the same load as the #65 baseline:
   `scripts/profile-memory.sh` and `MODE=stall scripts/profile-memory.sh`.
   Pass if receiver peak RSS is clearly below the baseline peaks (320 MB burst, 469 MB for the
   caught-up peer) and does not grow with `FILES` and `BIG`. Also run with `FILES=20000 BIG=1000`
   to see that the peak stays near the bound while the load grows.
3. **Throughput:** `scripts/profile-write.sh` shows no meaningful drop.
4. **Protocol:** if the wire format or limits change, the version is bumped and the installer's
   protocol warning fires; upgrade every member together.
5. **Live:** after the upgrade, watch RSS and the inbox figures from #63 on a receiver during a real
   catch-up (for example after restarting a node that was away).

### #67 redb cache size

Run with a large metadata set (for example `FILES=50000 BIG=0` through `profile-memory.sh`) and
check RSS against the chosen cache size; `profile-write.sh` shows many-small-files speed unchanged.
Record the setting and numbers in `docs/validation.md` and, if configurable, the README config table.

### #68 unclaimed chunk replies

Harness: a reply that arrives after its fetch timed out is gone after the fixed period (on the
simulated clock); the map's bytes stay under the cap; a reply that arrives in time still serves
its fetch.

### #69 return freed memory

After #66. Run `profile-memory.sh` with and without the allocator settings
(`MALLOC_ARENA_MAX=1 MALLOC_TRIM_THRESHOLD_=131072`), and `profile-write.sh`. Adopt only if settled
memory falls meaningfully (the #65 run went 149 to 92 MB) and write throughput drops by 1% or less;
otherwise close the issue with the numbers.

### #64 cluster-wide activity

1. **Harness:** several members; the matrix shows both ends of each path; a member that never
   answers is listed and totals are marked partial; nothing is sent unless a command asks.
2. **Protocol:** version bumped, release notes say to upgrade every member, installer warns.
3. **Live:** upgrade all members, then compare each node's own `startfs activity` with the matrix;
   a path where the two ends disagree must be a real problem (for example the flapping VPS).

## The baseline to beat

The current baseline (after #62) is in `docs/baselines/2026-10-18-main-b69b396/README.md`, with the
raw samples. The table below is the earlier one from #65 (before #62), kept for comparison.

`scripts/profile-memory.sh`, `FILES=4000 BIG=200`, 3 copies, commit `de69ac3` (rerun it on the
current `main` first, because #62 changed the receive path):

| Run | Writer RSS | Receiver peak | Receiver settled |
|---|---|---|---|
| burst | 35 MB | 320 MB | 149 MB |
| burst, `MALLOC_ARENA_MAX=1 MALLOC_TRIM_THRESHOLD_=131072` | 16 MB | 294 MB | 92 MB |
| `MODE=stall` (C frozen, then resumed) | 68 to 76 MB | 277 MB; the caught-up peer 469 MB | flat |

## Test data

Never copy by hand; build the tree with code so runs are comparable and the data is new.

```sh
# a fresh tree (new seed = new content), then copy it into the mount under test
scripts/make-tree.py /tmp/burst                       # 4000 files of 8 to 72 KiB, 200 of 1 MiB, 400 dirs
scripts/make-tree.py /tmp/burst-big --files 20000 --big 1000 --dirs 2000   # 5x
scripts/make-tree.py /tmp/same --seed 7               # the same tree every time, for before/after
cp -r /tmp/burst ~/StartFS/burst-$(date +%s)
```

- The default seed is the clock, so every run is new data. That matters: chunks are stored by
  content, so copying a tree the cluster already holds sends almost nothing.
- A fixed `--seed` gives the same bytes every time. Use it in the isolated cluster, where each run
  starts empty, so before and after are the same load. On the real cluster use a fresh seed.
- `scripts/profile-memory.sh` builds its own random files (`FILES`, `BIG`). It is equivalent for
  the isolated runs; switching it to `make-tree.py` is a small follow-up if the shapes should match
  exactly.
- Delete test trees from the mount when done, then `startfs purge` (1.5.0 or newer, every member up and
  responsive) frees the space at once. Without a purge it comes back only after the 7 day retention.
- Do not delete and re-copy the same tree to repeat a test without a purge between: the copy
  re-references the chunks still there, so almost nothing crosses the network. Use a fresh seed.
- Restarting a daemon resets its counters and memory; note any restart next to a result.
- Peers that are down change what you see. Check `startfs status` and `tailscale ping` for the VPSes
  before blaming the software; vps1 and vps2 took turns being unreachable during the original report.

## Issues found by the live runs

- #74 tick saturation and retry loop; #78 memory growth to 10 GiB; #82 rescan after restart; #85
  `meta.redb` write flood; #83 I/O pressure in `activity`; #84 report lagging nodes and weight
  placement. #77 (purge) is done. From 2026-10-19: #91 (a node that is too slow should protect itself).
- The plan's earlier assumption that the writer stays small, and that the inbound path is "the actual
  defect", did not survive: the writer reached 10 GiB, and the slow-disk nodes were hurt by their own
  write volume more than by inbound bytes.

## Open questions

- What did #62 change about the memory behaviour? Baseline rerun: settled receiver memory looks higher (258 to 271 against 149 MiB) and the malloc settings now cut the peak by half; one run each, repeat before relying on either.
- `quic.rs` `local_api` tests failed once in `scripts/check.sh` and passed on rerun and alone; a flaky test, not investigated.
- Answered: purge on request was wanted and exists (#77, 1.5.0). The 3090 growth reproduced on the
  writer at 10 GiB (#78).
- Should `offer` stay the placement weight, or should a node have a separate `weight` (#84)? The
  slowest disks currently hold the most data.
- Answered: the `meta.redb` flood was `reconcile` rewriting the whole tree (#85, 1.5.1). Whether that was
  all of it on the live nodes is not yet seen.
- Was vps2 CPU-throttled (credits) during the join? Only the Azure metric "CPU Credits Remaining" for
  vps2 around 11:15 to 11:20 UTC on 2026-10-19 can say; if it is near 0 then that run measured the
  throttle, and a bigger VM would be the fix.
- Where do mini1's 5.6 GiB go when no counted structure holds them (#78)?
- What should a weak node be: a full member with a self-limited load (#91), or a light member holding
  fewer chunks (#84)? Probably both, in that order.
