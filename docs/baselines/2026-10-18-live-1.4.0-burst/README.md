# Live burst, release 1.4.0, 2026-10-18

`startfs activity` and `startfs status` on one node of the real cluster (7 members, 3 copies, all
peers online) while a fresh `scripts/make-tree.py` tree (seed 1792369946, 4200 files, 374,346,136
bytes) was copied into its mount. Samples every 20 s: `s-000` before, `w-001` during the copy,
`a-001` to `a-024` after (about 8 minutes). Taken right after all nodes were upgraded to 1.4.0 and
had settled; the copy left `data: AT RISK`, then `DEGRADED`, then `SAFE` at about +8 minutes.

| After the copy | `data` | tick busy | avg tick | queue | daemon RSS |
|---|---|---|---|---|---|
| before | SAFE | 10% | 1.1 ms | 0 | 111 MiB |
| +20 s | AT RISK | 39% | 5.9 ms | 4,199 | 203 MiB |
| +40 s | AT RISK | 69% | 17 ms | 4,163 | 2.9 GiB |
| +1 min | AT RISK | 98% | 65 ms | 3,861 | 4.9 GiB |
| +2 min | AT RISK | 98% | 60 ms | 3,341 | 5.6 GiB |
| +4 min | DEGRADED | 96% | 29 ms | 2,117 | 9.1 GiB |
| +8 min | SAFE | 21% | 1.3 ms | 0 | 10.0 GiB |

What it shows:

- The tick thread is the limit (#74): busy 94 to 98% for about 6 minutes, nearly all of it in the
  `replicate` phase, and every one of the six online paths timed out about 8,500 asks a minute.
  Timeouts with every peer online point to a retry loop on a slow tick, not to the network.
- Memory (#78): RSS rose 90 times for a 374 MB copy and stayed at 10 GiB after the queue drained.
  The inbox peaked at 306 KiB; unclaimed replies, the decoded cache and the indexes stayed small.
  The 67 GiB machine had no memory pressure and no swap.
- A first look at the same cluster minutes earlier, while the nodes were still restarting after the
  upgrade, showed the tick 82% busy and 1,400 to 1,600 asks in flight to vps1 and vps2 with none
  acknowledged. That cleared by itself once they were back; it was a restart, not a fault.

The test data stays on the cluster for the 7 day retention (#77 is the request to purge it).
