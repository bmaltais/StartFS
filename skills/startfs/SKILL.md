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
otherwise names any program with its working directory or an open file on `~/StartFS` (they lose the
mount; on a terminal it asks first, `STARTFS_FORCE=1` skips the question), stops the service, swaps the binary (the old one stays as `startfs.prev`),
restarts and checks the daemon, and rolls back if it does not answer; Ctrl-C after the stop puts the old binary and the running service
back. While the daemon stops, `startfs status` says `stopping`. Data and config are
untouched. If it warns that the protocol changed, upgrade every member: until they match,
`startfs status` lists the others as down with `cannot reach: protocol version mismatch`.
Done when `startfs status` on each machine shows all members `up`.
`STARTFS_VERSION=vX.Y.Z` pins a release (older ones warn).

For several machines, upgrade one at a time and wait until the cluster is calm before the next:
`scripts/rolling-upgrade.sh vX.Y.Z local HOST...` (ssh hosts, the machine that matters most last)
installs, then waits for `data: SAFE`, every member up, an empty inbox (`startfs activity`) and a
quiet tick, and stops on a node that does not get there. Never restart a node that is still
catching up: a restarted node re-checks all the chunks it holds (`status` shows `re-checking: N chunks`
while it waits for answers; that is not a shortfall), the others only those whose placement changed.
Expect a member that is down to show as `DEGRADED` until it is back.

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

## Explain a slow transfer or high memory

```sh
startfs activity          # --json for scripts
```

Figures are this machine's own, over the last minute (the header says how many seconds it has
collected; a daemon restart resets it, and a window of a few seconds says little). Run it on the
machine that is slow, and again a minute later to see what changed. Read, in this order:

| What you see | It points to | Next |
|---|---|---|
| `tick:` busy above ~80% (`status` then shows a `busy:` line), the `replication queue:` not shrinking, and one `phases` figure carrying the time | the single background tick thread is the limit, not the network; the phase says where (observed with a peer down: `replicate` 44 ms of a 45 ms tick, busy 85%, while traffic to the dead peer was only 30 KiB/s) | tell the user it is working as fast as one thread can; nothing on the network to fix |
| many `timed out` and `retried` on one peer path, that member `down` or `in flight` high, nothing `in` from it | that peer, or its Tailscale route, is the problem (observed: 47,804 timeouts in a minute to a frozen peer, 4,200 asks in flight, 0 acknowledged) | `startfs status` for the member's state and `cannot reach:` reason, then `tailscale ping` that machine |
| little traffic on every path, a long `replication queue:`, `busy` low | the daemon is waiting: on a slow peer or the 5 s retry delay | look at `in flight` and the oldest waiting age; a peer that answers late or not at all is the usual cause |
| `tick:` very long or `no ticks in the window`, an `inbox` large and growing, `io pressure` high (observed: 25 to 77% on the slow nodes, ticks of 34 to 116 s, thread stuck in `D`) | the machine's disk is the limit (observed on cloud disks with an IOPS cap), not the network; `daemon storage writes` rising fast between two readings means the daemon itself is the writer, flat means another process loads the disk | do not restart: a restart starts another re-check of every chunk the node holds (#82); wait for the backlog to drain. The node's gc and anything waiting on it is held up until it does |
| a node `up` in its peers' `status` but its own `activity` shows the long tick and the large inbox | the node is online but not processing; only its own `activity` shows it | read that node's `activity`, not a peer's `status`, before judging it healthy |
| `memory pressure`, `swap used` above zero, or `cgroup memory` close to its limit | the machine is starved of memory, which slows everything | free memory on that machine before blaming StartFS |
| `daemon:` resident size far above what `in memory:` accounts for (inbox, unclaimed chunk replies, decoded-chunk cache, indexes) | memory the allocator keeps or that is not counted yet, not queues | report the figures; do not tell the user a queue is the cause |
| a figure reads `unavailable` | this machine's OS gave no source for it (no `/proc` pressure files, macOS, no cgroup) | not that all is well: say it could not be measured |

Observed on a node under load: the first two rows (a frozen peer during a copy of many small
files). The waiting, memory-pressure and resident-size rows follow from what the figures measure but
have not yet been seen in a real run; say so if asked how sure the reading is.

`inbox` shows `no limit` when nothing bounds it: a large figure there is real memory held.
`unclaimed chunk replies` are capped (16 MiB) and dropped after a minute. `cgroup memory` can be the whole login session, not just StartFS.

What not to conclude: nothing here says what another machine sees (each end of a path counts
its own side); counters only cover the current daemon run; and `startfs config --activity off`
(default on, or `activity = false` in `config.toml`) stops the counting, after which only the
resource figures and queue are live. Report the figures and the reading; do not claim a cause
the table does not support.

## Free space after deleting files

Deleted files keep their data for 7 days. If the user is short of disk now, or has just deleted a
test tree, `startfs purge` frees it on every member. It cannot be undone, so run it only when the
user has said the deleted files are not needed. It refuses while any member is down (it names them;
bring them back, or `startfs remove` one that is gone for good). Afterwards `startfs status` shows
`garbage collection: purge in progress` while it works, and names a member holding it back. Only
files deleted before the purge are freed; run it again if a daemon restarted meanwhile. Every member
must run protocol 12 (release 1.5.0 or newer).

## Retire or remove a machine

- Planned exit: `startfs leave` on that machine hands its data to the others first.
- Machine gone for good: `startfs remove MEMBER` from a survivor (the number is in
  `startfs status`); its data is re-replicated to N copies.
- Machine merely offline: wait. Its copies count again when it returns; after 30 minutes
  unseen the others start re-replicating its data, and `remove` is final.

## Tell the user

- Creates and new file contents reach disk about a second late; a power cut can lose the last
  second unless the file was fsynced. Programs should check the result of `close()`.
- `fcntl`/`flock` locks work within one machine only, so two machines can both hold the
  same lock.
- Every tailnet member can read everything; there is no per-user access control or
  encryption at rest.
- Real multi-machine use and macOS are unverified (see "Known limits" in the README).

`startfs config --offer SIZE` changes a machine's contribution later. Global flags
`--data-dir DIR` and `--config FILE` go before the command.
