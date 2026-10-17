# Coding standards

Read during review, not implementation. Mechanical rules live in CI (`.github/workflows/ci.yml`), not here.

## Errors

- A failure to persist or apply state (membership, metadata, log entries, counters) is logged or returned. Ignoring a `Result` is for teardown (joins, socket and file cleanup) and best-effort sends to a peer that may be gone.
- Mark a deliberate ignore with a short comment saying why it is safe.

## Tests

- A test of behaviour goes through the cluster harness: `FsOps`, the daemon control interface, the simulated network and clock. Tests of internals are rejected.
- A test that spawns real processes or sockets reads each request fully before replying and polls for readiness. Timing guesses make it flaky under load.
