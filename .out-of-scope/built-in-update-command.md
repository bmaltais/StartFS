# Built-in update command

StartFS does not have a `startfs update` command, signed releases, or a way to upgrade
other members from one machine.

## Why this is out of scope

Upgrading is the installer's job: `scripts/install.sh` already finds the release for the
machine's OS and CPU, verifies its checksum, stops the service, replaces the binary,
restarts the daemon and rolls back if it does not answer. A command inside the binary
would repeat all of that and add questions the installer does not have, such as how a
running daemon replaces itself while the filesystem is mounted.

- **Signatures.** Releases are checksummed over HTTPS from the same GitHub release. A
  signature would need a signing key and a way to distribute and rotate it; the tailnet
  is already fully trusted (see README, Known limits), so the extra step protects little.
- **Cluster-wide upgrade.** Reaching other machines needs host lists, ssh credentials
  and a policy for partial failure. Running the installer on each machine in a shell loop
  covers it (README, Upgrading).

## Prior requests

- #26: "Make upgrading a cluster safe: docs, version visibility, and release-based update"
