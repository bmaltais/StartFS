# Member provisioning runs over SSH from the CLI, never from the daemon

## Status

Accepted (#259, from the exploration in #143).

## Context

Adding a machine was three manual steps: make a token, install the release there, join. `startfs add`
sequences them over SSH. The question was who opens the SSH session.

## Decision

**The CLI does, as the invoking user.** `startfs add` runs in the user's terminal with their own
credentials (SSH agent, `tailscale ssh` and its ACLs, the sudo password on the target). The daemon
only mints and revokes the token, as it already does for `token` and `revoke`. There is no "ask the
cluster to add it" mode and no new protocol message.

**Why not the daemon.** Members can read everything but cannot run code on each other. A daemon that
could open SSH sessions to peers (needing stored keys or an agent) would turn one compromised member
into code execution on every other member. The CLI keeps that boundary: it can only do what the person
typing it could do.

**The token is short-lived and always revoked.** Tokens are multi-use until they expire or are revoked,
and making them single-use would be a protocol change. `add` mints one valid for 10 minutes
(`NewTokenFor`, a local control request) and revokes it on every path, success or failure.

**The joiner runs the cluster's release.** The install is pinned to the minting node's version, so a
joiner never lands on another protocol (updates are lockstep, ADR 0002, 0007).

**Failure never uninstalls.** The installer is re-runnable, so a failed add leaves an installed but
unjoined node and says to run `add` again.

## Consequences

- Encrypted filesystems are refused: joining needs the recovery key, which must not travel in a
  command line. Revisit with a keyless storage-only path (#238).
- Linux targets only, because no other OS has a release (the installer says so).
- The sequence is tested with a fake `Remote`; real SSH is checked by hand.
