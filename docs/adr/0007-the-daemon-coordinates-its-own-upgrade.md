# The daemon coordinates its own upgrade, through the installer's logic

## Status

Accepted (#127). Supersedes the former out-of-scope entry "built-in update command", removed with this decision.

## Context

Upgrades were the installer's job, and a cluster-wide upgrade was a shell loop over ssh
(`scripts/rolling-upgrade.sh`). That needs host lists, ssh keys and a policy for partial
failure, and it can only see the cluster from the outside: it polls `status` and guesses when a
node is ready. The members already gossip who is up, who is behind and what the verdict is.

## Decision

The daemon takes the request (`startfs update [vX.Y.Z]` on any member; with no release, the latest stable one, resolved once by the issuer) and the members carry it
out among themselves over the mesh: no ssh, no host list. The daemon runs the installer's
logic (find the release, check its checksum, swap, roll back if the new daemon does not
answer) and does not grow a download path of its own. The same-protocol case is a rolling
upgrade with an **upgrade pause** per member; the different-protocol case stages everywhere,
then swaps together on one go-signal from the issuer, which lets the stages expire if it
disappears. A node with its mount in use stages but waits for the user.

The old reasons for refusing stand only in part. Duplicating the installer is avoided by
sharing it. Credentials and partial failure disappear because no machine is reached from
outside. Signatures stay out: the tailnet is trusted (README, Known limits) and the checksum
comes from the same release. Keeping the mount across the restart is still out of scope
(`.out-of-scope/upgrade-without-dropping-the-mount.md`).

## Considered options

- **Keep the script.** Rejected as the main path: it cannot pause a member, cannot stage
  before a lockstep swap and cannot tell a returning member what to catch up to.
- **Roll back every member when one fails after the go-signal.** Rejected: a distributed
  transaction at the worst moment. Each node rolls back only itself; staging catches most
  failures before anything restarts, and `status` shows a split cluster.

## Consequences

Any member can order every member to move to a release. That is acceptable on a fully trusted
tailnet; a cluster that cannot accept it would need the request authenticated first.
