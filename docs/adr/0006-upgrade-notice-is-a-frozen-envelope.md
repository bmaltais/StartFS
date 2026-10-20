# The upgrade notice is a frozen envelope that every release understands

## Status

Accepted (#127). Amends ADR 0002.

## Context

Every message starts with a version byte and `decode` drops anything whose byte is not ours.
After a lockstep upgrade a member that was away can neither hear nor be heard by the rest, so
it cannot even learn which release to catch up to. Refusing to start a lockstep upgrade while
any member is absent would make the upgrade that needs the most help the one that is hardest
to run.

## Decision

Reserve one version byte value for an **upgrade notice**: a tiny message saying which release
the cluster is on and its protocol number. `decode` routes that byte to the notice handler
instead of rejecting it. The notice's form never changes in any later release. It names a
release and never carries a binary or a command, so a false notice can at worst point a member
at a real release, which the member fetches itself and checks with the installer's checksum. A
member moves only to a higher release than its own and trusts a notice only from a peer in its
membership list.

Strict lockstep stays (ADR 0002): two members still talk only when their protocol versions are
equal. The notice is a side channel for catching up, not a second protocol.

## Considered options

- **Refuse a lockstep upgrade while a member is absent.** Rejected: one laptop asleep would
  block the whole cluster, or the absent member would be left to be upgraded by hand.
- **One version of overlap** (ADR 0002's rejected option). Still rejected: it doubles the
  testing surface of every message change. A frozen notice is one message, not all of them.
- **Carry the catch-up in an ordinary message.** Impossible: the old node refuses it.

## Consequences

The notice's form is a permanent constraint. It must carry no field that a future release could
need to change, and a change to it is a breaking change to every deployed daemon. Releases
before the one that introduces it cannot be called back by it, so the first lockstep hop to it
is manual (`scripts/rolling-upgrade.sh --lockstep`).
