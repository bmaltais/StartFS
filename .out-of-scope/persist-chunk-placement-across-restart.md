# Persisting chunk placement across a restart

StartFS does not save what a node knew about where chunks are held, so that a restarted daemon could skip most of its re-check. After a restart the daemon asks its peers about every chunk it holds (GLOSSARY: Re-checking).

## Why this is out of scope

The re-check is short and cheap at the cluster's size, so persistence would save little. Measured on 2026-10-20 on the live cluster (1.10.0, 7 members) by restarting vps1, a cloud member holding 4,233 chunks:

- The re-check ran for about 9 s, then `status` stopped showing it.
- The node was busy 10% of the time over the first 31 s, with a longest tick of 172 ms and an empty replication queue.
- `data: SAFE` came back 31.7 s after the restart. The last 22 s were not the re-check.

The loopback run of `scripts/profile-restart.sh` (1,550 files, 3 nodes) gave the same picture: 1 s of CPU, tick busy 3%, SAFE after about 31 s.

The slowdown on 2026-10-18 (seven restarts in a row leaving four nodes minutes behind) predates the upgrade pause (#127, 1.10.0), which stops the other members reacting to a restart. That, not the restarted node's own re-check, is the likelier cause.

Persisting would also cost something: a stored placement can go stale when the member set or placement changes, and a wrong entry could make a node believe a chunk is safely placed when it is not. The design would have to reconcile with ADR 0003, which keeps what members know about each other in memory and relearns it from peers.

## Reconsider if

A node with a much larger chunk count (hundreds of thousands) is measured re-checking for minutes after a restart with an unchanged member set, using `scripts/profile-restart.sh` or a real restart (`startfs status` re-checking count, `startfs activity`, time to `SAFE`).

## Prior requests

- #128: "Decide whether to persist chunk placement knowledge across a restart"
