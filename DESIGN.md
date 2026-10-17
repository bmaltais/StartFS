# StartFS design notes

Running design doc, to be fed to a spec agent.

## Vision

A lightweight, decentralised, low-maintenance filesystem in Rust that runs on a Tailscale tailnet.
Machines install the app, mount the filesystem, and the system deduplicates, compresses and keeps
N copies of data across member machines. When a machine disappears, the system re-replicates to
restore the configured redundancy.

Scopes: tailnet-wide (v1), group of machines (future), single machine (future). All three are the
same code with a different membership scope.

## Decisions

1. **Consistency:** last-writer-wins with conflict copies. Writes always succeed locally, including
   offline, and merge on reconnect. Collisions keep the newest version and save the other as a
   conflict copy. No quorum or Raft.
2. **Workload:** mixed, general-purpose. Content-defined chunking (FastCDC, roughly 1-4MB chunks)
   with a small-file fast path (single chunk under a threshold, around 64KB; tiny files inlined in
   metadata).
3. **Redundancy:** plain N-copy replication of whole chunks. N is set per filesystem at creation.
   No erasure coding in v1.

4. **Platforms:** Linux and macOS for v1. Linux uses FUSE (`fuser`). macOS uses FUSE-T (preferred,
   userspace, no kernel extension) or macFUSE. The core (chunk store, metadata, replication) is
   platform-independent; only the mount layer sits behind a trait. Windows (WinFsp) is deferred.
   Risk: macOS FUSE is a third-party install, which cuts against the "easy" goal.

5. **Scale:** up to about 20 nodes per filesystem. Every node knows all members and holds the full
   metadata replica. Simple membership; no sharded metadata. Larger scale is out of scope.

6. **Trust:** fully trusted tailnet. Every member can read everything. Tailscale provides transport
   encryption and machine authentication; membership can be limited with Tailscale ACL tags. No
   encryption at rest in v1. The on-disk chunk format should stay encryption-ready so a per-filesystem
   key can be added later without redesign.

7. **Capacity and placement:** weighted by contributed space. Each node declares a space offer
   (e.g. 50GB laptop, 2TB server) and placement (weighted rendezvous hashing) is proportional to it.
   Usable size is roughly total offered space divided by N. An offer of 0 makes a node a pure client
   (mounts and caches, stores no replicas), which covers the "client-only" role without a separate concept.

8. **Failure handling:** timeout with automatic repair. A node unseen for a grace period (default
   30 minutes, configurable per filesystem) is treated as down and
   chunks with fewer than N reachable copies are re-replicated, rate-limited. If the node returns,
   its copies count again and surplus copies are trimmed lazily.

9. **Create and join:** explicit join token. `startfs create --name home --copies 3` on the first
   machine prints a token (filesystem ID plus a bootstrap peer's tailnet address).
   `startfs join <token>` on another machine contacts the bootstrap peer, and existing members admit
   the new node. No auto-discovery in v1.

10. **Deletion and space reclaim:** delayed garbage collection. Deleting a file only updates
    metadata. A background GC removes chunks that are unreferenced and have been orphaned longer than
    a retention period (default 7 days, configurable). This protects
    against accidental deletes and against nodes returning from offline with stale references. No
    reference counting. Versioned snapshots are a possible future addition.

11. **Remote reads:** fetch on demand with a bounded local LRU cache (size configurable). The cache
    is separate from replica storage and does not count toward the N copies. Recently used files
    work offline. Pinning (`startfs pin <path>`) is deferred beyond v1.

12. **Write durability:** local first, background replication. A write is durable on the local node's
    disk and returns immediately; replication to the other N-1 nodes follows. There is a window with
    a single copy, and `startfs status` must show files not yet fully replicated. Nodes with offer 0
    need a local staging area for writes until replication completes.

13. **Metadata sync:** per-node operation log (create, write, rename, delete) stamped with hybrid
    logical clocks. Peers exchange missing entries: push when online, catch-up on reconnect. Applying
    in clock order gives last-writer-wins; collisions become conflict copies. The log is compacted
    periodically. A Merkle summary to speed up catch-up is an optional later addition.

14. **Operation model:** a single `startfs` binary acting as both daemon and CLI. The daemon (run
    under systemd or launchd) mounts the filesystem and performs replication, repair and GC. The CLI
    (`create`, `join`, `status`, `leave`, `config`) talks to the daemon over a local socket. Config is
    a small TOML file with sensible defaults. `startfs status` is designed to answer "is my data safe
    right now?". No web UI or tray app in v1.

15. **Transport:** QUIC (`quinn`) to peers' tailnet addresses. Streams are multiplexed so bulk
    chunk transfers don't block metadata sync. Tailscale provides encryption and identity, so QUIC
    uses a throwaway self-signed certificate. Peer identity comes from the local `tailscaled` `whois`
    API, not from certificates. The protocol is a few message types (metadata log exchange, chunk
    fetch, chunk push, heartbeat), and the transport layer stays replaceable.

16. **POSIX scope:** core subset. Supported: files, directories, rename, symlinks, permission bits,
    mtime, basic xattrs. Ownership is mapped to the local user (uid/gid differ across machines).
    Unsupported: hardlinks, ACLs, cross-node locks (advisory locks are local only). Conflict rules:
    concurrent rename/delete resolved by clock order; a write into a deleted directory recreates
    the directory path.

17. **Integrity:** BLAKE3 verification on every chunk read, plus a low-priority background scrub
    (default full pass every 30 days, configurable; pauses on battery power). A corrupt chunk is deleted and re-fetched from another replica via the normal repair path.

18. **Replica counting:** copies on distinct, currently reachable nodes. Health tiers per chunk:
    healthy (N copies), degraded (fewer than N), at risk (1). If fewer storage nodes exist than N,
    run best effort and say so in `status` (e.g. "3 wanted, 2 possible"). Full nodes are skipped in
    placement and `status` warns about capacity. Writes are refused only when the local node has no
    space left; never because N copies can't be placed.

19. **v1 scope and machine-loss recovery:** v1 is tailnet-wide only. A rebuilt machine recovers by
    joining again with a new token and pulling the metadata log and chunks (full metadata is on every
    node, chunks on N nodes). The dead node is removed and repair restores N copies.

## Future: personal (group-of-1) scope

Not in v1. Needs:
- Encryption at rest for chunks and metadata, so machines hosting replicas of a personal filesystem
  store opaque blobs they cannot read.
- A recovery key shown once at `create` time, from which the encryption key is derived.
- `startfs recover <recovery key>` on a rebuilt machine: find replicas on the tailnet, prove key
  possession, pull encrypted metadata, regain access.
- Replica hosting for non-members.

Group scope follows the same model with a shared group key. Chunk and metadata formats in v1 must be
encryption-ready.

## Non-goals for v1 (confirmed)

- Windows support
- Erasure coding
- Encryption at rest and per-user access control
- Hardlinks, ACLs, cross-node locks
- Pinning, snapshots and versioned history
- Web UI or tray app
- Auto-discovery of filesystems
- More than about 20 nodes
- Strong consistency

## Success criteria (confirmed)

- New filesystem plus one joined machine in under 5 minutes and about 3 commands.
- A node can disappear permanently and data returns to N copies with no user action.
- No data loss when any N-1 nodes fail, provided writes had finished replicating.
- Idle daemon under about 100MB memory and negligible CPU.
- Normal operations on cached data run at near-local-disk speed.

## Open items for the spec agent

These are intentionally left to the specification, with the constraints stated above
(lightweight, simple, low maintenance):

- Exact on-disk formats: chunk store layout, metadata store (e.g. redb), operation log record format
- Wire protocol message definitions and versioning
- Operation log compaction rules and tombstone handling
- Conflict-copy naming and how conflicts are surfaced to the user
- Node admission and removal flow details (token format, expiry, revocation, `leave` semantics)
- Repair rate limiting and prioritisation (at-risk chunks first)
- Local staging area sizing for offer-0 nodes
- Compression choice and level (zstd assumed), and per-chunk skip for incompressible data
- Defaults to treat as accepted: small-file threshold about 64KB, failure grace period 30 minutes,
  GC retention 7 days, scrub period 30 days
