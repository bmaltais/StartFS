# Coding standards

Read during review, not implementation. Mechanical rules live in CI (`.github/workflows/ci.yml`), not here.

## Errors

- A failure to persist or apply state (membership, metadata, log entries, counters, buffered writes) is logged or returned. Ignoring a `Result` is for teardown (joins, socket and file cleanup) and best-effort sends to a peer that may be gone; clippy enforces the `reason` on each such `allow`.
- A failure that a background loop retries every tick is logged once per distinct message, not per tick.

## Durability

- Commits made with `Durability::None` are visible at once but not on disk. Whatever leaves the node (log entries served to peers, chunk acknowledgements, replies to control requests) is preceded by `LocalFs::sync`, so a power cut cannot reissue log positions peers already hold.
- A commit that drops references to chunks stays durable, since old chunks may be collected.
- Only `LocalFs::keep` stores chunks with `put_lazy`; every other writer uses `put`. A durable commit syncs the chunk stores first, so nothing durable refers to a chunk that is not.
- `Cluster::crash` drops the node, and `LocalFs`'s `Drop` syncs, as a clean shutdown does. A test of what a power cut loses uses `Cluster::power_cut`.

## Concurrency

- The harness runs `tick` inside any thread that waits on a peer, so a lock held across a chunk fetch or the wait hook is re-entered by that same thread. Code reachable from `tick` takes such a lock with `try_lock` and skips the round when it is busy.

## Docs

- A change to behaviour, the wire protocol or config updates what describes it: `DESIGN.md` (including its "Not done" lines), the README config table, `GLOSSARY.md` for a new domain term, `skills/startfs/SKILL.md` (what users are told) and `docs/validation.md` (measured numbers).

## Tests

- A test of behaviour goes through the cluster harness: `FsOps`, the daemon control interface, the simulated network and clock. Tests of internals are rejected.
- A test asserts the bound the spec states; when the code misses it, fix the code, not the assertion.
- A fix for something seen on a live cluster is shown to clear it with the same observation, not only a harness test: the harness has no QUIC links, so transport behaviour (stale connections, reconnects) is checked with `scripts/trace-restart.sh` or on the cluster.
- A test of durability fails when the code under test is made eager or lazy the wrong way. `crash` and `restart` keep lazy commits, so they prove nothing about a power cut; observe `has_unsynced` instead.
- A test that spawns real processes or sockets reads each request fully before replying and polls for readiness. Timing guesses make it flaky under load.
