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

   Set `STARTFS_VERSION=v1.3.1` to pin a release, `INSTALL_DIR` to change the
   target, or `STARTFS_NO_SERVICE=1` to install the unit without starting it. If `~/.local/bin` is not on your `PATH` (it is on Omarchy; on Ubuntu and
   Fedora it is only after you log out and in again), the installer ends by printing the
   `export PATH=...` line to run in the current shell. `startfs --version` shows what is
   installed.

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
   `STARTFS_VERSION=v1.3.1`. If that is the version already installed it prints
   "already up to date" and stops without touching anything. If it is older than the
   installed one it refuses, unless you named it with `STARTFS_VERSION`, in which case
   it warns and goes ahead.
2. **Downloads and verifies.** It fetches the tarball for this OS and CPU and checks its
   SHA-256 checksum. Nothing on your machine changes until this passes.
3. **Warns about protocol changes.** If the new release speaks a different
   [protocol version](GLOSSARY.md) from the installed one (`startfs --version` shows
   both), it prints a warning: members on different protocols cannot sync, so upgrade
   every member.
4. **Stops the service.** First it lists the programs with a working directory or open
   file on `~/StartFS`, since they lose the mount; on a terminal it asks before going on
   (`STARTFS_FORCE=1` never asks; Linux only, macOS skips the check). Then the daemon unmounts the mount, or detaches it
   after a few seconds if something is still using it. `startfs status` says "stopping"
   meanwhile. Press Ctrl-C at any point after this and the installer puts the old binary
   and the running service back.
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

`df` on the mount shows the logical size of your files as used, against the usable size of
the cluster (the space the machines offer, divided by the number of copies). It does not show
the saving. `startfs status` does, in its `storage:` lines: the size of the files, what is
left after deduplication, how well this node's chunks compress, and what share of the
cluster's replicas this node holds. That share is why this node's disk use can be a small
fraction of your data: each machine stores only its part of every copy. `startfs status --json`
has the same figures under `efficiency`.

## Surviving machine loss

There is no central server: every machine runs the same daemon, and the data lives on
the member machines themselves. `--copies N` (set once at `create`, for the whole
filesystem) keeps N copies of every chunk on N different machines.

- **Up to N-1 machines can fail without losing data**, provided the writes had finished
  replicating. With `--copies 2` one machine can die; with `--copies 3`, two.
- **No action is needed to recover.** A machine unseen for longer than `repair_grace_secs`
  (30 minutes by default) is treated as gone and the others re-replicate its chunks back to N
  copies. If it comes back, its copies count again. Use `startfs remove <member>` to
  declare a dead machine gone sooner. A chunk left with fewer than 2 reachable copies is not
  made to wait: a few seconds after a machine goes down, stand-in copies bring it back to 2
  (and are trimmed again if the machine returns).
- **The margin shrinks until repair finishes.** If a second machine is lost before the first
  one's data is back to N copies, you are one failure closer to loss. `startfs status`
  shows copies possible versus wanted and the repair progress.
- **Machines that sleep are covered for.** See [Machines that sleep](#machines-that-sleep).
- A short outage is not a loss. A machine that is merely offline keeps working locally and
  catches up when it returns; concurrent edits to the same file become conflict copies.

This was validated with real processes (`kill -9` of members, 2 and 3 copies, see
`docs/validation.md`), not yet on real separate machines.

## Machines that sleep

Laptops and desktops that suspend at night, or from time to time, would otherwise keep the
filesystem in a degraded state while they are away, and then trigger a round of copying
(and later trimming) every time. StartFS covers for them instead.

- **After 5 minutes unseen**, a member is taken for asleep. The others make **stand-in
  copies** of its chunks on machines that are awake, until every chunk is back to N
  reachable copies. A chunk is not listed as degraded in `startfs status` while stand-ins
  cover for the machine. (A member unseen for under 5 minutes, such as a reboot, causes no
  copying beyond the usual floor of 2 reachable copies.)
- **When the machine wakes**, the stand-ins are **kept** for `standin_days` (3 by default)
  from the last time it was seen asleep, then trimmed lazily.
- **If it sleeps again inside that window, nothing is copied**: the stand-ins are already
  there, so the chunks never go degraded. A machine that sleeps every night therefore pays
  the copying once, the first night. As long as it keeps sleeping, its stand-ins stay;
  the cluster holds N+1 copies of the chunks it shares with that machine. When it stops
  sleeping, the extras go after the retention.
- **A machine that never returns** is handled as before: after `repair_grace_secs` it is
  treated as lost and its chunks are re-replicated for good.

Examples, with `--copies 2` on a desktop, a laptop and a server:

- *The laptop sleeps every night from 23:00 to 07:00.* At 23:05 the desktop and server
  copy the laptop's chunks between themselves. At 07:00 the laptop wakes and catches up.
  The extra copies stay. Tonight nothing moves and `status` stays healthy; before, each
  night meant re-replicating everything the laptop holds, then trimming it all again.
- *The desktop is off for a week's holiday.* It is unseen for more than 30 minutes, so
  it is declared lost and repaired for good, exactly as before.
- *A weekend laptop is used once a month.* Its extras are trimmed three days after each
  use, so the cost is one copy pass per use and no standing extra disk use.

Costs and limits:

- `startfs status` shows, under each member whose stand-ins are being kept, how long they
  stay ("extra copies kept for 2d 4h in case it sleeps again"); `status --json` has it as
  `standin_secs_left` on the member.
- `startfs status` also shows, under each member that is down or lost, when it was last
  seen ("last seen 2h 14m ago"). The members pool what they each remember, so the
  freshest sighting anywhere counts, and a restarted daemon still shows the right gap;
  only if no member remembers one does it say "over", counting from its own start.
  `status --json` has it as `last_seen` (`secs_ago`, `exact`).
- A member that is down or lost may hold chunks no one else has yet. `startfs status`
  says so ("at risk: 3 chunks may exist only on surface…") from the single-copy count in
  that member's last report, and keeps the verdict at risk until it returns. The members
  relay these reports, so a restarted daemon still knows. Chunks written after the last
  report are not covered. `status --json` has it as `stranded`.
- Stand-ins use disk on the machines that cover: roughly the share of data the sleeping
  machines hold, for up to `standin_days` after they were last asleep.
- The first sleep of a machine pays one copy pass, rate limited like other repair
  (`repair_bytes_per_sec`).
- Stand-ins are stored on members that have room; a member that is full is skipped, and
  if no member can take them the chunk is listed as degraded as before.
- The retention is remembered in memory by each daemon, so after a daemon restart the
  extras it holds are trimmed as soon as the machine they covered for is back.
- While a member's stand-ins are kept, ordinary hand-offs of chunks it shares (after a new
  machine joins, say) are also held back until the retention ends, so those chunks can sit
  above N copies for a while.
- `startfs config --copies` works while stand-ins are kept: the surplus goes once the
  retention has passed.
- A machine that was itself asleep does not take its peers for asleep when it wakes.
- `standin_days` is set when the filesystem is created, like `repair_grace_secs`. Set it to
  `0` to turn stand-ins off and get the earlier behaviour.

## Status

`startfs status` answers "is my data safe right now?": copies possible versus
wanted, usable size, members and their state, files not yet fully replicated,
repair progress, scrub state, cache usage, warnings, and conflicts.
`startfs status --json` prints the same as JSON, for scripts and GUIs.

`startfs stats` asks every member that is up for its storage figures, right then, and
prints one line each: space used on its disk, chunks, compression, its share of the
replicas and what it offers. A member that does not answer is listed as `no answer`.
Run it on any node. `startfs stats --json` gives the full figures per member.

Other commands: `startfs leave` (hand this node's data over and leave),
`startfs remove <member>` (drop a dead member; its data is re-replicated),
`startfs config --offer 500G` (this node's space) and `startfs config --copies 3`, which
changes the number of copies for the whole filesystem, on every node. Raising it makes the
extra copies in the background (`status` shows the repair). Lowering is refused until every
file has all its copies and every member is reachable, then the surplus is trimmed lazily.

## Configuration

Optional TOML file at `~/.config/startfs/config.toml` (override with `--config`).
Data lives in `~/.local/share/startfs` (override with `--data-dir`). Defaults:

| key | default | meaning |
|---|---|---|
| `port` | 7600 | QUIC port peers connect to |
| `offer_bytes` | 10 GiB | space this node offers (0 = pure client) |
| `cache_bytes` | 1 GiB | LRU cache of chunks fetched from peers |
| `read_cache_bytes` | 32 MiB | recently read chunks kept decoded in memory, so reading a file in small pieces does not decode a chunk per read (0 = off) |
| `compression` | `auto` | `auto` compresses only chunks a quick sample shows to shrink, and keeps the result when it saves at least an eighth; `off` stores chunks as they are; `always` compresses every chunk and keeps any saving. Applies to chunks stored from now on |
| `staging_bytes` | 4 GiB | unreplicated writes held before writes are refused |
| `sync_quiet_ms` | 1000 | how long without a metadata commit before recent creates are made durable on disk |
| `write_commit_delay_ms` | 1000 | how long a file must go unwritten before the background tick commits its buffered writes |
| `write_buffer_bytes` | 8 MiB | most of one file's writes held in memory before a batch is committed (never below 8 MiB) |
| `token_ttl_secs` | 86400 | join token lifetime |
| `repair_grace_secs` | 1800 | on `create`: how long a member may be unseen before re-replication |
| `standin_days` | 3 | on `create`: days the extra copies made while a member slept are kept after it is back (0 = no stand-ins for sleeping members) |
| `gc_retention_secs` | 604800 | on `create`: how long deleted chunks are kept |
| `repair_bytes_per_sec` | 64 MiB | most bytes per second each node sends when repairing (restoring copies after a lost node or a `config --copies` increase); every holder sends its share, so the cluster's speed grows with its size |
| `scrub_period_secs` | 30 days | full integrity pass period |
| `state_chunk_bytes` | 1 MiB | size of each piece when sending the whole metadata state to a member that missed a log compaction |
| `mountpoint` | unset | mount here when `--mount` is not given |
| `allowed_tags` | `[]` | Tailscale ACL tags a peer must carry (empty: any tailnet machine) |
| `whois_socket` | platform default | `tailscaled` socket |

## Known limits and non-goals

Limits found while validating v1 (details and numbers in `docs/validation.md`):

- **Creates reach disk about a second late.** `create` and closing a file, whatever its size, do
  not wait for an fsync; the daemon makes them durable after `sync_quiet_ms` without activity, on `fsync`, before
  talking to peers and on clean shutdown. A power cut (not a killed process) can lose the last
  second of creates and new file contents unless the file was fsynced. Overwriting or truncating
  a file that has chunks still waits for disk, since the old chunks may be collected.
- **Writes are buffered.** A write is held in memory (up to `write_buffer_bytes` per file) and
  committed on `flush`, `fsync` and close, when the buffer fills, and once the file has been
  quiet for `write_commit_delay_ms`. Other nodes see a new file's content after that point.
  Reads see buffered data at once, but if the daemon is killed before a commit, the
  buffered writes are lost, as with any POSIX filesystem that has not been synced. A clean
  shutdown commits them. A write can also fail at `flush`/`close` (for example with no space left),
  so check the result of `close()`.
- Many small files are slower than on a local disk (about 0.2 to 0.3 ms each over FUSE, against
  0.03 ms).
- **Advisory locks are local only.** `fcntl`/`flock` locks work between processes on the same
  machine but are not coordinated between nodes: two machines can both "hold" a lock on the
  same file. Do not rely on them to guard shared files across nodes.
- Not verified: real multi-machine use, a real tailnet, macOS.

Non-goals for v1: Windows, erasure coding, encryption at rest or per-user access
control, ACLs, directory hardlinks, cross-node locks, pinning, snapshots, a web UI, filesystem
auto-discovery, and more than about 20 nodes. The tailnet is fully trusted: every
member can read everything.
