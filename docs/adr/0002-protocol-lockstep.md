# Members must share one protocol version

## Status

Accepted (#26). Amended by ADR 0006 and ADR 0007 (#127): lockstep stays, with a catch-up channel.

## Context

Messages are tagged with a protocol version and refused unless it matches exactly, so
a release that changes a message shape cannot sync with the previous one. Upgrading
one machine at a time through such a release was possible only by accident of timing,
and the failures were confusing.

## Decision

Keep strict lockstep: two members talk only when their protocol versions are equal, and
a protocol change means upgrading every member. Make the mismatch visible instead of
tolerating it: the refusing side words the error from the reader's side and says which
node to upgrade, `startfs --version` prints the protocol, and `status` lists an
incompatible member with the reason it cannot be reached. The installer warns before
installing a release whose protocol differs.

## Considered options

- **One version of overlap**, so members could be upgraded one at a time. Rejected for
  now: every message change would need to work against two versions, which doubles the
  testing surface for a v1 filesystem of at most about 20 nodes, where upgrading all
  members together is cheap.

Revisit if a cluster appears that cannot afford its members being unable to sync for
the minutes an upgrade takes.

## Amendment (#127)

Strict lockstep still holds for every ordinary message. What changed is how a cluster gets
through a protocol change: the members coordinate it themselves (ADR 0007), and a member that
was away learns which release to catch up to from an upgrade notice, a frozen message every
release understands (ADR 0006).
