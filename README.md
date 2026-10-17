# StartFS

A small, decentralised filesystem for a Tailscale tailnet. Each machine runs one
daemon and mounts the same filesystem. Data is split into content-defined chunks,
deduplicated, compressed, and kept in N copies across the member machines. If a
machine disappears for good, the others re-replicate its data back to N copies.

One binary, `startfs`, is both the daemon and the CLI. See `DESIGN.md` for the
design and `docs/validation.md` for what has and has not been measured.

Status: v1, tested on Linux only. **Read "Known limits" before using it for real data.**

## Install

Every machine needs [Tailscale](https://tailscale.com) running (peers are identified
through `tailscaled`) and a FUSE library. Prebuilt binaries are attached to each
[release](https://github.com/bmaltais/StartFS/releases); no Rust toolchain is needed.

| Platform | Tarball suffix |
|---|---|
| Linux, 64-bit Intel/AMD | `x86_64-linux` |
| Linux, ARM64 (Raspberry Pi 4/5, ARM servers) | `aarch64-linux` |
| macOS, Apple Silicon (experimental) | `aarch64-macos` |

### Linux

1. Install FUSE 3 and Tailscale with your distro's package manager:

   | Distro | Command |
   |---|---|
   | Omarchy / Arch | `sudo pacman -S fuse3 tailscale` |
   | Ubuntu / Debian | `sudo apt install fuse3` (Tailscale: <https://tailscale.com/download/linux>) |
   | Fedora | `sudo dnf install fuse3 tailscale` (add the Tailscale repo first, see the link above) |

   Then `sudo systemctl enable --now tailscaled && sudo tailscale up`.

2. Install the latest release for your CPU (detects x86_64 or aarch64, checks the
   checksum, installs `~/.local/bin/startfs`, then enables and starts the systemd user
   service that mounts `~/StartFS`, restarts after a crash and starts at boot):

   ```sh
   curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
   ```

   Set `STARTFS_VERSION=v1.0.2` to pin a release, `INSTALL_DIR` to change the
   target, or `STARTFS_NO_SERVICE=1` to install the unit without starting it. Make sure `~/.local/bin` is on your `PATH` (it is on Omarchy; on Ubuntu and
   Fedora, log out and in again after the directory is first created). `startfs --version`
   shows what is installed.

The service is the only step after the install: check it with
`systemctl --user status startfs`.

To try it by hand instead: `startfs daemon --mount ~/StartFS`.

The binaries are built on Ubuntu 22.04 and need glibc 2.35 or newer, which every
currently supported Ubuntu, Fedora and Arch release has. On an older distro, build
from source.

### macOS (experimental)

The macOS mount adapter is written but has **never been run**, and the macOS build is
best-effort, so a release may not include a Mac tarball. Treat it as unsupported until
someone verifies it.

1. Install [Tailscale](https://tailscale.com/download/mac) and FUSE-T:
   `brew install --cask fuse-t` (macFUSE also works; see `dist/README.md`).
2. Run the same installer (it installs to `~/.local/bin`; add `INSTALL_DIR=/usr/local/bin`
   with `sudo` if you prefer):

   ```sh
   curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
   ```

   If you downloaded the tarball in a browser, macOS may refuse to run the binary:
   clear the flag with `xattr -d com.apple.quarantine ~/.local/bin/startfs`.
3. Set `whois_socket` in `config.toml` to the Tailscale socket (see the config table
   below), then start it by hand with `startfs daemon --mount ~/StartFS`, or at login
   with the launchd agent: edit `USERNAME` in `com.startfs.daemon.plist`, copy it to
   `~/Library/LaunchAgents/` and run
   `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.startfs.daemon.plist`.

### Building from source

Needs a Rust toolchain and the FUSE development headers (`libfuse3-dev` on
Debian/Ubuntu, `fuse3-devel` on Fedora, `fuse3` on Arch):

```sh
cargo build --release
install -Dm755 target/release/startfs ~/.local/bin/startfs
install -Dm644 dist/startfs.service ~/.config/systemd/user/startfs.service
```

## Upgrading

Run the installer again on each machine. It is the same command as the first install:

```sh
curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
```

On a machine that already has StartFS, this is what it does, in order:

1. **Picks the release.** The latest published release, or the one named by
   `STARTFS_VERSION=v1.0.2`. If that is the version already installed it prints
   "already up to date" and stops without touching anything. If it is older than the
   installed one it refuses, unless you named it with `STARTFS_VERSION`, in which case
   it warns and goes ahead.
2. **Downloads and verifies.** It fetches the tarball for this OS and CPU and checks its
   SHA-256 checksum. Nothing on your machine changes until this passes.
3. **Warns about protocol changes.** If the new release speaks a different
   [protocol version](GLOSSARY.md) from the installed one (`startfs --version` shows
   both), it prints a warning: members on different protocols cannot sync, so upgrade
   every member.
4. **Stops the service.** The daemon unmounts `~/StartFS` cleanly. Programs with files
   open on the mount will see it go away; close them first.
5. **Replaces the binary**, keeping the old one next to it as `startfs.prev`, and
   refreshes the systemd unit (Linux) before reloading it.
6. **Starts the service and checks it.** It waits up to 15 seconds for the new daemon to
   answer `startfs status`.
7. **Rolls back if it does not answer.** The old binary is restored, the service is
   started again, and the installer exits with an error, so the loop below stops at the
   first machine that failed.

It never touches your data directory (`~/.local/share/startfs`) or `config.toml`. A
service that was stopped before the upgrade stays stopped. With `STARTFS_NO_SERVICE=1`
it replaces the files and leaves the service alone. On macOS it restarts the launchd
agent only if one is already loaded; it does not set one up.

**Upgrade every member.** Nothing is lost while members run different releases, but if
the release changes the protocol (step 3 warned you) the upgraded machine and the others
cannot sync until all of them match. Until then `startfs status` lists the others as
down with the reason, for example:

```
  2   100.64.0.7:7600   down  100G offered
      cannot reach: protocol version mismatch: the peer speaks version 5, this node speaks version 4; upgrade this node
```

For several machines:

```sh
for host in alpha beta gamma; do
  ssh "$host" 'curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh' || break
done
```

Afterwards run `startfs status` on any machine: every member should be `up`.

Downgrading is allowed but not recommended: a newer build can write state an older one
refuses to read. If you built from source, rebuild and `systemctl --user restart
startfs.service` instead.

## Quickstart

The daemon is already running after the Linux install (on macOS or a manual install, run
`startfs daemon --mount ~/StartFS &` first).

**First machine:** create the filesystem. `--example` prints the whole command to paste
on the others:

```sh
$ startfs create --name home --copies 2 --example
startfs join startfs1-AbC...
```

**Each further machine:** paste that line (add `--offer 100G` for the space this machine
contributes):

```sh
startfs join startfs1-AbC... --offer 100G
```

That is all: `~/StartFS` is now the same filesystem on both. Use it like any directory.

- `startfs token --example` prints a fresh join command later; tokens expire after 24h
  and `startfs revoke <token>` cancels one. A token names the machine by its MagicDNS name
  when the tailnet has one, otherwise by its tailnet IP.
- Without `--example`, `create` and `token` print just the token, for scripts:
  `TOKEN=$(startfs token)`.

## Use it with an AI assistant

`skills/startfs/SKILL.md` teaches a coding assistant how to install, set up and check
StartFS. For Claude Code, copy it into place:
`mkdir -p ~/.claude/skills && cp -r skills/startfs ~/.claude/skills/`.

## Space savings

Identical data is stored once (deduplication) and chunks are compressed, so your files
take less of the space the machines offer than their sizes add up to. The saving depends
on your data: copies, backups and text shrink a lot, already-compressed media barely at all.

Two numbers can therefore differ. A file manager or `du --apparent-size` shows the logical
size of the files; `df` on the mount shows what is actually stored. For example, a 2.5 MiB
folder showed 1.7 MiB used. Per-file sizes never show the saving, only the filesystem-wide
used figure does.

## Surviving machine loss

There is no central server: every machine runs the same daemon, and the data lives on
the member machines themselves. `--copies N` (set once at `create`, for the whole
filesystem) keeps N copies of every chunk on N different machines.

- **Up to N-1 machines can fail without losing data**, provided the writes had finished
  replicating. With `--copies 2` one machine can die; with `--copies 3`, two.
- **No action is needed to recover.** A machine unseen for longer than `repair_grace_secs`
  (30 minutes by default) is treated as gone and the others re-replicate its chunks back to N
  copies. If it comes back, its copies count again. Use `startfs remove <member>` to
  declare a dead machine gone sooner.
- **The margin shrinks until repair finishes.** If a second machine is lost before the first
  one's data is back to N copies, you are one failure closer to loss. `startfs status`
  shows copies possible versus wanted and the repair progress.
- A short outage is not a loss. A machine that is merely offline keeps working locally and
  catches up when it returns; concurrent edits to the same file become conflict copies.

This was validated with real processes (`kill -9` of members, 2 and 3 copies, see
`docs/validation.md`), not yet on real separate machines.

## Status

`startfs status` answers "is my data safe right now?": copies possible versus
wanted, usable size, members and their state, files not yet fully replicated,
repair progress, scrub state, cache usage, warnings, and conflicts.

Other commands: `startfs leave` (hand this node's data over and leave),
`startfs remove <member>` (drop a dead member; its data is re-replicated),
`startfs config --offer 500G`.

## Configuration

Optional TOML file at `~/.config/startfs/config.toml` (override with `--config`).
Data lives in `~/.local/share/startfs` (override with `--data-dir`). Defaults:

| key | default | meaning |
|---|---|---|
| `port` | 7600 | QUIC port peers connect to |
| `offer_bytes` | 10 GiB | space this node offers (0 = pure client) |
| `cache_bytes` | 1 GiB | LRU cache of chunks fetched from peers |
| `staging_bytes` | 4 GiB | unreplicated writes held before writes are refused |
| `sync_quiet_ms` | 1000 | how long without a metadata commit before recent creates are made durable on disk |
| `write_commit_delay_ms` | 1000 | how long a file must go unwritten before the background tick commits its buffered writes |
| `write_buffer_bytes` | 8 MiB | most of one file's writes held in memory before a batch is committed (never below 8 MiB) |
| `token_ttl_secs` | 86400 | join token lifetime |
| `repair_grace_secs` | 1800 | on `create`: how long a member may be unseen before re-replication |
| `gc_retention_secs` | 604800 | on `create`: how long deleted chunks are kept |
| `repair_bytes_per_sec` | 8 MiB | repair rate limit |
| `scrub_period_secs` | 30 days | full integrity pass period |
| `state_chunk_bytes` | 1 MiB | size of each piece when sending the whole metadata state to a member that missed a log compaction |
| `mountpoint` | unset | mount here when `--mount` is not given |
| `allowed_tags` | `[]` | Tailscale ACL tags a peer must carry (empty: any tailnet machine) |
| `whois_socket` | platform default | `tailscaled` socket |

## Known limits and non-goals

Limits found while validating v1 (details and numbers in `docs/validation.md`):

- **Creates reach disk about a second late.** `create` and closing a small file do not wait for
  an fsync; the daemon makes them durable after `sync_quiet_ms` without activity, on `fsync`, before
  talking to peers and on clean shutdown. A power cut (not a killed process) can lose the last
  second of creates unless the file was fsynced.
- **Writes are buffered.** A write is held in memory (up to `write_buffer_bytes` per file) and
  committed on `flush`, `fsync` and close, when the buffer fills, and once the file has been
  quiet for `write_commit_delay_ms`. Other nodes see a new file's content after that point.
  Reads see buffered data at once, but if the daemon is killed before a commit, the
  buffered writes are lost, as with any POSIX filesystem that has not been synced. A clean
  shutdown commits them. A write can also fail at `flush`/`close` (for example with no space left),
  so check the result of `close()`.
- Many small files are slow to create (about 2.5 ms each over FUSE).
- **Advisory locks are local only.** `fcntl`/`flock` locks work between processes on the same
  machine but are not coordinated between nodes: two machines can both "hold" a lock on the
  same file. Do not rely on them to guard shared files across nodes.
- Not verified: real multi-machine use, a real tailnet, macOS.

Non-goals for v1: Windows, erasure coding, encryption at rest or per-user access
control, ACLs, directory hardlinks, cross-node locks, pinning, snapshots, a web UI, filesystem
auto-discovery, and more than about 20 nodes. The tailnet is fully trusted: every
member can read everything.
