---
name: startfs
description: Install, set up, join machines to, and check StartFS, the replicated filesystem for a Tailscale tailnet. Use when the user mentions StartFS or ~/StartFS, wants a shared folder replicated across their machines, or asks whether their StartFS data is safe.
---

# StartFS

A decentralised filesystem for a Tailscale tailnet. Every machine runs one daemon and
mounts the same filesystem at `~/StartFS`. Data is deduplicated, compressed and kept in N
copies across the machines. One binary, `startfs`, is both daemon and CLI.

## Install (Linux, macOS)

Needs Tailscale running and `fuse3` (Linux) or FUSE-T (macOS). Then:

```sh
curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
startfs --version
```

On Linux this also enables and starts the `startfs` systemd user service, which mounts
`~/StartFS`. Check it with `systemctl --user status startfs`; logs are in
`journalctl --user -u startfs`. On macOS the adapter is untested and the daemon is
started by hand: `startfs daemon --mount ~/StartFS &`.

## Set up a filesystem

On the first machine (the daemon must be running):

```sh
startfs create --name NAME --copies N --example
```

`--copies` is how many machines hold each piece of data (2 or 3 is typical). The output is
a ready-to-run command, `startfs join startfs1-...`. Give it to the user, or run it on
each other machine over ssh, adding `--offer SIZE` (for example `100G`) for the space that
machine contributes. Tokens expire after 24h. `startfs token --example` makes a new one and
`startfs revoke TOKEN` cancels one.

Without `--example`, `create` and `token` print only the bare token, which is what to use
in scripts.

## Check it

```sh
startfs status
```

Answers "is my data safe right now?": copies possible versus wanted, members and their
state, files not yet fully replicated, repair progress, warnings and conflicts. Safe means
no under-replicated files and no members offline longer than expected. Report the
warnings, not just the summary.

## Other commands

- `startfs config --offer SIZE`: change how much space this machine contributes.
- `startfs leave`: hand this machine's data to the others and leave (do this before
  decommissioning a machine).
- `startfs remove MEMBER`: drop a dead machine for good; its data is re-replicated. Member
  numbers are in `startfs status`. Never run this for a machine that is only offline.
- Global flags go before the command: `--data-dir DIR`, `--config FILE`.

## Things to tell the user

- Writes reach disk about a second late. A power cut can lose the last second of creates
  unless the file was fsynced, so check the result of `close()` in programs.
- `fcntl`/`flock` locks are local to one machine and do not coordinate across machines.
- The tailnet is fully trusted: every member can read everything. There is no per-user
  access control or encryption at rest.
- Not verified on a real multi-machine tailnet or on macOS. Full list: README "Known
  limits"; design and numbers: `DESIGN.md`, `docs/validation.md`.
