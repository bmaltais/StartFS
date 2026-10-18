---
name: startfs
description: StartFS, a filesystem replicated across a user's Tailscale machines at ~/StartFS. Use to install it, create a filesystem, add another machine, check whether data is safe, or remove or retire a machine.
---

# StartFS

Every machine runs one daemon and mounts the same filesystem at `~/StartFS`; data is kept
in N copies across the machines. One binary, `startfs`, is daemon and CLI.

## Install

Needs Tailscale running, plus `fuse3` (Linux) or FUSE-T (macOS).

```sh
curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
```

Done when `startfs --version` prints a version (for example `startfs 1.0.1 (protocol 4)`)
and `startfs status` answers (on Linux the
installer started the `startfs` systemd user service; read failures with
`journalctl --user -u startfs`). On macOS start the daemon by hand:
`startfs daemon --mount ~/StartFS &`.

## Upgrade

Run the same install command on every machine. It skips a machine that is already current,
otherwise stops the service, swaps the binary (the old one stays as `startfs.prev`),
restarts and checks the daemon, and rolls back if it does not answer. Data and config are
untouched. If it warns that the protocol changed, upgrade every member: until they match,
`startfs status` lists the others as down with `cannot reach: protocol version mismatch`.
Done when `startfs status` on each machine shows all members `up`.
`STARTFS_VERSION=vX.Y.Z` pins a release (older ones warn).

## Create a filesystem (first machine)

```sh
startfs create --name NAME --copies N --example
```

`--copies` is how many machines hold each piece of data; 2 or 3 is typical. The output is
a ready-to-run line, `startfs join startfs1-...`.

## Add a machine

Run the line from `create` on the new machine, adding `--offer SIZE` (for example `100G`)
for the space it contributes. Tokens expire after 24h: `startfs token --example` prints a
fresh line, `startfs revoke TOKEN` cancels one. Ssh into the new machine to run it when the
user cannot.

Done when `startfs status` on either machine lists both under `members:` as `up`.

## Check safety

```sh
startfs status
```

Read the `data:` line first:

| `data:` | Means |
|---|---|
| `SAFE` | every file has as many copies as the machines allow, and the wanted number is possible |
| `BEST-EFFORT` | fewer machines than wanted copies, so the full count is impossible; normal until more machines join |
| `DEGRADED` | a file is short of copies, a repair is running, or a member is `down` (a `lost` member no longer counts once its data is re-replicated) |
| `AT RISK` | some data has a single copy left; act now |

For anything but `SAFE`, quote the `redundancy:` line, the `members:` states, any
`cannot reach:` reason under a member and any `repair:` line with the verdict. `AT RISK` and a `down` member the user cannot bring back
call for a new machine to join, or `remove` once the machine is gone for good.

## Retire or remove a machine

- Planned exit: `startfs leave` on that machine hands its data to the others first.
- Machine gone for good: `startfs remove MEMBER` from a survivor (the number is in
  `startfs status`); its data is re-replicated to N copies.
- Machine merely offline: wait. Its copies count again when it returns; after 30 minutes
  unseen the others start re-replicating its data, and `remove` is final.

## Tell the user

- Creates and small writes reach disk about a second late; a power cut can lose the last
  second unless the file was fsynced. Programs should check the result of `close()`.
- `fcntl`/`flock` locks work within one machine only, so two machines can both hold the
  same lock.
- Every tailnet member can read everything; there is no per-user access control or
  encryption at rest.
- Real multi-machine use and macOS are unverified (see "Known limits" in the README).

`startfs config --offer SIZE` changes a machine's contribution later. Global flags
`--data-dir DIR` and `--config FILE` go before the command.
