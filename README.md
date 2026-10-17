# StartFS

A small, decentralised filesystem for a Tailscale tailnet. Each machine runs one
daemon and mounts the same filesystem. Data is split into content-defined chunks,
deduplicated, compressed, and kept in N copies across the member machines. If a
machine disappears for good, the others re-replicate its data back to N copies.

One binary, `startfs`, is both the daemon and the CLI. See `DESIGN.md` for the
design and `docs/validation.md` for what has and has not been measured.

Status: v1, tested on Linux only. **Read "Known limits" before using it for real data.**

## Install

Requirements: a Rust toolchain to build, and [Tailscale](https://tailscale.com)
running on every machine (peers are identified through `tailscaled`).

### Linux

```sh
sudo pacman -S fuse3          # or: apt install fuse3 / dnf install fuse3
cargo build --release
install -Dm755 target/release/startfs ~/.local/bin/startfs
```

To run it as a user service that mounts `~/StartFS` at boot, follow the comments in
`dist/startfs.service`. To try it by hand: `startfs daemon --mount ~/StartFS`.

### macOS (untested)

The macOS mount adapter is written but has **never been run**; no Mac was available.
It needs FUSE-T (preferred) or macFUSE and a `whois_socket` setting for Tailscale.
See `dist/README.md` and `dist/com.startfs.daemon.plist`. Treat it as unsupported
until someone verifies it.

## Quickstart

On the first machine:

```sh
startfs daemon --mount ~/StartFS &       # or enable the service unit
startfs create --name home --copies 2    # prints a join token
```

On each further machine, with the token from above:

```sh
startfs daemon --mount ~/StartFS &       # or enable the service unit
startfs join <token> --offer 100G        # --offer: space this machine contributes
```

Use `~/StartFS` like any directory. `startfs token` prints a fresh token,
`startfs revoke <token>` cancels one (tokens also expire after 24h).

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
| `write_commit_delay_ms` | 1000 | how long a file must go unwritten before the background tick commits its buffered writes |
| `write_buffer_bytes` | 8 MiB | most of one file's writes held in memory before a batch is committed (never below 8 MiB) |
| `token_ttl_secs` | 86400 | join token lifetime |
| `repair_grace_secs` | 1800 | on `create`: how long a member may be unseen before re-replication |
| `gc_retention_secs` | 604800 | on `create`: how long deleted chunks are kept |
| `repair_bytes_per_sec` | 8 MiB | repair rate limit |
| `scrub_period_secs` | 30 days | full integrity pass period |
| `mountpoint` | unset | mount here when `--mount` is not given |
| `allowed_tags` | `[]` | Tailscale ACL tags a peer must carry (empty: any tailnet machine) |
| `whois_socket` | platform default | `tailscaled` socket |

## Known limits and non-goals

Limits found while validating v1 (details and numbers in `docs/validation.md`):

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
control, hardlinks, ACLs, cross-node locks, pinning, snapshots, a web UI, filesystem
auto-discovery, and more than about 20 nodes. The tailnet is fully trusted: every
member can read everything.
