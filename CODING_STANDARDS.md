# Coding standards

Read during review, not implementation. Mechanical rules live in CI (`.github/workflows/ci.yml`), not here.

## Errors

- A failure to persist or apply state (membership, metadata, log entries, counters, buffered writes) is logged or returned. Ignoring a `Result` is for teardown (joins, socket and file cleanup) and best-effort sends to a peer that may be gone; clippy enforces the `reason` on each such `allow`.
- A failure that a background loop retries every tick is logged once per distinct message, not per tick.

## Concurrency

- The harness runs `tick` inside any thread that waits on a peer, so a lock held across a chunk fetch or the wait hook is re-entered by that same thread. Code reachable from `tick` takes such a lock with `try_lock` and skips the round when it is busy.

## Tests

- A test of behaviour goes through the cluster harness: `FsOps`, the daemon control interface, the simulated network and clock. Tests of internals are rejected.
- A test that spawns real processes or sockets reads each request fully before replying and polls for readiness. Timing guesses make it flaky under load.
