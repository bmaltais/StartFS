# Memory baseline on `main` after #62, 2026-10-18

Raw samples are in this directory (`burst.csv`, `stall.csv`, `burst-trim.csv`). Columns: seconds
since start, phase, RSS in MiB of the writer A and the receivers B and C, A's peak, and A's staging.
Compare later runs against this; see `docs/plan-memory-and-activity.md` for what to run and when.

- **Code:** `main` at `b69b396` (code identical to `ee5a374`, which includes the #62 liveness change).
- **Machine:** 3090, 12 cores, 80 GB RAM, Linux 7.2; the user was using the machine lightly.
- **Load:** `scripts/profile-memory.sh` defaults: 4000 files of 8 to 72 KiB plus 200 of 1 MiB
  (about 350 MiB), three loopback daemons, 3 copies.
- **One run each. Run-to-run variance is unknown;** repeat before reading small differences as real.

## Results

| Run | Writer A peak | Receiver B peak, last | Receiver C peak, last | Watched |
|---|---|---|---|---|
| burst | 59 MiB | 278, 271 | 278, 258 | 90 samples after the write (187 s) |
| `MODE=stall` (C frozen during the write, then resumed) | 55 MiB | 264, 86 | 494, 461 | 8 samples frozen, 12 after resume (47 s) |
| burst with `MALLOC_ARENA_MAX=1 MALLOC_TRIM_THRESHOLD_=131072` | 27 MiB | 131, 100 | 133, 91 | 8 samples (22 s) |

## Against the first measurements in #65 (before #62)

| Run | #65, receivers peak, settled | Now, receivers peak, settled |
|---|---|---|
| burst | 320, 149 | 278, 258 to 271 |
| burst with malloc settings | 294, 92 | 133, 91 to 100 |
| stall, the caught-up peer | 469, flat | 494, 461 |

## Reading

- The writer stays small in every run (under 60 MiB), as before. Memory is on the receivers.
- Receivers still hold about what was pushed to them after the burst; nothing was handed back in 187 s.
  Settled memory is higher than in #65 (258 to 271 against 149 MiB); one run each, so not conclusive.
- The malloc settings now cut the **peak** by more than half (278 to 133 MiB), where in #65 they
  cut only what was left afterwards. A real difference or a single-run effect; repeat before using it for #69.
- The peer that caught up after being frozen reaches about 490 MiB for about 350 MiB of data.
- The trim and stall runs were watched for under a minute, so "settled" there is not established.

## Write path (`profile-write.txt`)

`scripts/profile-write.sh`, one node, 1 copy, run on a quiet machine after the memory runs. Wall
time through the FUSE mount against a plain directory, with the daemon's CPU:

| Load | Local | Through FUSE | Daemon user / sys |
|---|---|---|---|
| 128 MiB `dd bs=1M` | 0.038 s | 0.343 s | 0.24 s / 0.18 s |
| 1000 x 8 B (inline) | 0.032 s | 0.319 s | 0.11 s / 0.07 s |
| 1000 x 16 KiB (chunks) | 0.031 s | 0.738 s | 0.22 s / 0.41 s |
| 200 x 1 MiB | 0.008 s | 0.414 s | 0.20 s / 0.39 s |

This is the figure the under-1% overhead check on #63 compares against: rerun it with `activity`
on and off. One run; the machine was otherwise idle.

## How this was produced

```sh
scripts/profile-write.sh > profile-write.txt                  # write path
scripts/profile-memory.sh                                   # burst.csv (WATCH=90)
MODE=stall WATCH=8 RESUME=12 scripts/profile-memory.sh      # stall.csv
MALLOC_ARENA_MAX=1 MALLOC_TRIM_THRESHOLD_=131072 WATCH=8 scripts/profile-memory.sh   # burst-trim.csv
```
