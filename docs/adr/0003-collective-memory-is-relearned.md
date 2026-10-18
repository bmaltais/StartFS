# Cluster knowledge is gossiped and relearned, never written to disk

## Status

Accepted.

## Context

A daemon knows things about the other members that are not part of the filesystem: when each was
last heard from, and what each last reported about its replication. These live in memory. A
restarted daemon starts with none of it, and a member that is asleep cannot tell it again.
Writing it to disk would keep it across a restart of one node, but a node that was down when the
fact arrived would still lack it, and the stored copy would go stale unnoticed.

## Decision

Keep this knowledge in memory only, and make every member relay what it knows in `Health`, the
message every member already sends each hello interval. A restarted daemon relearns it from any
live peer within one interval. The memory lives in the running daemons and is kept current by each
others' updates, so it is not lost as long as one daemon that knows it keeps running. The price is
that a restart of every member at once forgets it, which is accepted: nothing is lost
that the members cannot observe again once they are awake.

When adding something a daemon learns about its peers, decide whether a restarted daemon needs it.
If so, relay it in `Health` as `seen` and `reports` do, and add a harness test that restarts a node
and checks that it relearns the value.

## What is relayed

- When each member was last seen (`Health.seen`).
- The single-copy chunks a member that is not up last reported (`Health.reports`).
- When each member was last seen asleep (`Health.slept`), which sets how long stand-in copies are
  kept. Ages are rounded up when sent, so a time handed from node to node never creeps later.

## What is not, and how it recovers

- Which peers hold which chunks: relearned by asking, as the replicator does after every restart.
- Which members have no room: relearned from their answers.
- Per-peer chunk latency, garbage-collection counters: local measurements, no one else needs them.

## Considered options

- **Persist it on each node.** Rejected: it survives only that node's restart, goes stale, and
  adds a file to migrate.
