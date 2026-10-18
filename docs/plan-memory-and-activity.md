# Plan: activity view and memory bounds

Written 2026-10-18, after a large copy left the cluster showing `data: AT RISK` for a long time and
the daemon on 3090 at 2.4 GB of memory. Investigation: #65. This plan says which issues to do, in
what order, and how to test each when it is ready. Update it as issues land.

## What we know (from #65, measured on `de69ac3`, before #62)

- Memory is on the **receiving** side and scales with the burst; it is not given back afterwards.
- The writer stays small even with a peer frozen, so the send queue did not stack at that scale.
- redb's cache (1 GiB default, unset in the code) is not the cause on the live node: its file is 43 MB.
- About 40% of the memory left after a burst is allocator retention; about 90 MB stayed live.
- Not established: what took 3090 to 2.4 GB. A multi-GB inbound burst is untested.
- Deleting data does not free disk for 7 days (`gc_retention_secs`, fixed at `create`) and only once
  every member's frontier has been seen, so a flapping member holds reclaim back.

## Order

| # | Issue | Why here | Depends on |
|---|---|---|---|
| 0 | #70 memory profile script | merged; the measuring tool for everything below | none |
| 1 | #63 activity view, one node | the only `ready-for-agent` item; no protocol change; shows the rest on a real node | none |
| 2 | #66 bound the inbound path | the actual defect; needs a short grilling round first (byte bound, frame caps per message type, protocol bump or not). #62 only caps messages drained per tick (it is a liveness fix); the inbox is still an unbounded channel, and a smaller drain lets a burst queue up longer, so read #62 before starting | #63 helps measure it |
| 3 | #67 redb cache size | small, independent, safe to do alongside #2 | none |
| 4 | #68 drop unclaimed chunk replies | small, independent | none |
| 5 | #69 return freed memory to the OS | only worth deciding once #66 changes the peak | #66 |
| 6 | #64 cluster-wide activity | needs a protocol bump (ADR 0002) | #63; ship in the same release as #66 if #66 also bumps the protocol, so members upgrade in lockstep once |

## When to test

Run a checkpoint after the issue in the first column has merged and the nodes involved are
upgraded. Each checkpoint names the data it needs (see "Test data") and what must hold.

| After | Checkpoint | How | Must hold |
|---|---|---|---|
| now (before #63) | Baseline on current `main` | memory runs done 2026-10-18, see `docs/baselines/`; **still to do:** `profile-write.sh` on a quiet machine, right before #63 | numbers exist for `main` with #62 in |
| #63 | Instrumented burst on the real cluster | upgrade one node, `make-tree.py` with a new seed into its mount, run `startfs activity` every minute | each reading in the skill section is explained by `status`; overhead under 1% |
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
- Delete test trees from the mount when done; space comes back after the 7 day retention.
- Do not delete and re-copy the same tree to repeat a test: deleting frees nothing for 7 days, and
  the copy re-references the chunks still there, so almost nothing crosses the network.
- Restarting a daemon resets its counters and memory; note any restart next to a result.
- Peers that are down change what you see. Check `startfs status` and `tailscale ping` for the VPSes
  before blaming the software; vps1 and vps2 took turns being unreachable during the original report.

## Open questions

- What did #62 change about the memory behaviour? Baseline rerun: settled receiver memory looks higher (258 to 271 against 149 MiB) and the malloc settings now cut the peak by half; one run each, repeat before relying on either.
- `quic.rs` `local_api` tests failed once in `scripts/check.sh` and passed on rerun and alone; a flaky test, not investigated.
- Is a way to purge deleted data on request wanted, given the 7 day retention? Not filed.
- Is the 3090 daemon's 2.4 GB reproducible with a multi-GB inbound burst? Test once #63 can show it.
