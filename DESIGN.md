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
3. **Redundancy:** plain N-copy replication of whole chunks. N is set per filesystem at creation and can be changed later with `config --copies` (a gossiped, versioned setting; on a tie the higher count wins). Raising it is repair; lowering waits for a healthy cluster, then trims surplus copies lazily.
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
   (e.g. 50GB laptop, 2TB server) and placement (weighted rendezvous hashing) is proportional to it,
   or to a separate weight (`config --weight`, 0 = the offer) for a large disk that is slow.
   A weight only scales a share of an offer: a member offering 0 takes no copies whatever its weight,
   and `status` does not count it toward the copies the cluster can hold.
   Each member may also declare a site (`join --site`, `config --site`; ADR 0005): placement then
   spreads a chunk's copies over `min(copies, eligible sites)` sites, and a member with no label is
   its own site. Sites are declared, never detected, and a missing site never refuses a write.
   Each member gossips its backlog in `Health` (age of its last tick, inbox messages and bytes);
   `status` shows a member that is up but behind (by the same verdict the replicator acts on, hysteresis included), and the replicator sends it nothing new until it
   catches up, without changing placement or the epoch (ADR 0004).
   Usable size is roughly total offered space divided by N. An offer of 0 makes a node a pure client
   (mounts and caches, stores no replicas), which covers the "client-only" role without a separate concept.

8. **Failure handling:** timeout with automatic repair. A node unseen for a grace period (default
   30 minutes, configurable per filesystem) is treated as down and
   chunks with fewer than N reachable copies are re-replicated, rate-limited. If the node returns,
   its copies count again and surplus copies are trimmed lazily. A chunk with fewer than 2
   reachable copies does not wait for the grace period: once a member counts as down (unseen for
   three hello intervals), stand-in copies on other members bring the chunk back to 2, and stop there.
   A member unseen for 5 minutes counts as asleep: stand-in copies on awake members bring each of
   its chunks back to N reachable copies, so sleeping machines do not leave the filesystem degraded.
   The stand-ins stay for `standin_days` (default 3, fixed for the filesystem, 0 = off) after the
   member was last seen asleep, so a machine that sleeps again finds them there and nothing is
   copied twice; a member that keeps sleeping keeps them. Each daemon remembers when it last saw
   each member asleep in memory only, so a restarted daemon trims the extras it holds at once
   when the covered member is back. A member unseen past the grace period is lost as before.
   A change in who is reachable, asleep or lost, or in the members' offers, makes each node check
   again only the chunks it is still working on and those it holds that the change touches (their
   targets moved, they need stand-ins, or this node is not a target), not every chunk it holds;
   a restarted daemon, which has learned nothing yet, checks all of them. A held chunk that is
   only waiting for peers to say who else holds it is *re-checking*, not short of copies: it is
   left out of the shortfall and repair counts and `status` shows how many (this node's own
   figure, not gossiped), unless the peer asked has been silent for more than two retries.

9. **Create and join:** explicit join token. `startfs create --name home --copies 3` on the first
   machine prints a token (filesystem ID plus a bootstrap peer's tailnet address: its MagicDNS name when it has one, else its IP).
   `startfs join <token>` on another machine contacts the bootstrap peer, and existing members admit
   the new node. No auto-discovery in v1. Member ids need no coordination: each member owns an
   id range and an admission takes the new id from it and gives the node a slice for its own
   admissions, so concurrent admissions through different members cannot collide.

10. **Deletion and space reclaim:** delayed garbage collection. Deleting a file only updates
    metadata. A background GC removes chunks that are unreferenced and have been orphaned longer than
    a retention period (default 7 days, configurable). This protects
    against accidental deletes and against nodes returning from offline with stale references. No
    reference counting. Versioned snapshots are a possible future addition. `startfs purge` ends the
    wait on request, for what is deleted by then: it refuses unless every member is up (a member that
    missed it would keep its copies and re-spread them when it returns), sends `Purge` to all, and
    each member records the orphans it knows and collects with a retention of zero for those first
    seen up to 30 s after the request (so a delete still arriving is included); later deletes keep
    their retention. The reclaim gate is kept. A purge cannot be undone.

11. **Remote reads:** fetch on demand with a bounded local LRU cache (size configurable). The cache
    is separate from replica storage and does not count toward the N copies. Recently used files
    work offline. A node asks peers for a chunk in order of the round-trip time it has measured to each
    (a local estimate, not shared; unmeasured peers first, peers that look down last). Placement
    ignores latency. Pinning (`startfs pin <path>`) is deferred beyond v1.

12. **Write durability:** local first, background replication. A write is durable on the local node's
    disk and returns immediately; replication to the other N-1 nodes follows. There is a window with
    a single copy, and `startfs status` must show files not yet fully replicated. Nodes with offer 0
    need a local staging area for writes until replication completes. "Durable" means after
    `fsync`, the periodic sync tick or shutdown; `close` alone does not wait for disk, as on a
    local filesystem. That holds for a file of any size that is only being added to. Deleting an
    inline file or an empty directory is lazy in the same way, so a crash may bring it back;
    deleting a chunk-backed file, or overwriting or truncating one so that chunks are dropped, is
    durable before it returns, since the dropped chunks may be collected. Chunks written without a
    sync are flagged on disk, and a store opened after a crash drops any of them that were cut
    short, so a later write of the same content stores it afresh.

13. **Metadata sync:** per-node operation log (create, write, rename, delete) stamped with hybrid
    logical clocks. Peers exchange missing entries: push when online, catch-up on reconnect. Applying
    in clock order gives last-writer-wins; collisions become conflict copies. The log is compacted
    periodically. A Merkle summary to speed up catch-up is an optional later addition.

14. **Operation model:** a single `startfs` binary acting as both daemon and CLI. The daemon (run
    under systemd or launchd) mounts the filesystem and performs replication, repair and GC. The CLI
    (`create`, `join`, `status`, `leave`, `config`) talks to the daemon over a local socket. Config is
    a small TOML file with sensible defaults. `startfs status` is designed to answer "is my data safe
    right now?" for the whole cluster: members gossip counts of their under-replicated chunks (by copies held) and repair progress, `status` merges the reports of reachable members, and lists the members it could not hear from (`unheard`) so a partial view is never shown as complete. A member that is not up still speaks through its last report: the chunks it then listed as a single copy are shown as `stranded` (they may exist only on it) and hold the verdict at risk until it returns. Such knowledge is the cluster's collective memory: kept in memory, relayed in `Health`, and relearned by a restarted daemon from its peers (ADR 0003). `status` also answers "where did my space go?" from this node's own view: logical size of the files, bytes after dedup (both cluster-wide, since every member holds the full metadata), and how this node's replicas are compressed (chunks, bytes saved, share of the data compressed) and what share of the expected total it holds. Other members' figures are not gossiped: `startfs stats` asks each member that is up for them on demand (`StatsRequest`/`StatsReply`), so nothing runs in the background and the numbers are current. `df` on the mount reports the logical used bytes against the usable size (offered space divided by N), not the node-local replica bytes. `status` also shows, for each member that is up, this node's round-trip time to it: the transport's own smoothed measurement (`Transport::rtt`, QUIC's connection RTT, kept fresh by keep-alives), falling back to the estimate learned from chunk fetches. `startfs activity` answers "where does the time go?" for this node only (nothing is sent to other members): a `CountingTransport` around the transport counts bytes and messages per peer path by message category, a tick timer splits each tick into phases, and the replicator reports its queue and send outcomes; all of it sits in a 60 s rolling window of one-second buckets read from the injected `Clock`, in memory, off with `activity = false`. The view also lists the in-memory structures a burst fills (transport inbox, unclaimed chunk replies, decoded-chunk cache, replication and staging indexes) beside the daemon's RSS, and the OS figures (`/proc`, PSI, cgroup), each shown as unavailable when the platform has no source. Memory is bounded by bytes on the chunk path: a receiver stops reading a chunk message while its inbox holds 64 MiB (QUIC flow control slows the sender; metadata and gossip are exempt), the replicator withholds a chunk push while the transport holds 32 MiB for that peer (`Transport::backlog`), unclaimed chunk replies (late or duplicate answers nobody fetched) are dropped after 60 s and capped at 16 MiB, oldest first, and the tick hands freed heap pages back to the OS every 2 s (`malloc_trim`) because glibc keeps a burst's memory in per-thread arenas. No web UI or tray app in v1.

15. **Transport:** QUIC (`quinn`) to peers' tailnet addresses. Streams are multiplexed so bulk
    chunk transfers don't block metadata sync. Tailscale provides encryption and identity, so QUIC
    uses a throwaway self-signed certificate. Peer identity comes from the local `tailscaled` `whois`
    API, not from certificates. The protocol is a few message types (metadata log exchange, chunk
    fetch, chunk push, heartbeat), and the transport layer stays replaceable.

16. **POSIX scope:** core subset. Supported: files, directories, rename, symlinks, hardlinks (files and
    symlinks, not directories), permission bits, mtime, basic xattrs. Ownership is mapped to the
    local user (uid/gid differ across machines). Unsupported: ACLs, cross-node locks (advisory
    locks are local only). Conflict rules: concurrent rename/delete resolved by clock order; a
    write into a deleted directory recreates the directory path; an unlink of a file's last name
    racing a link of a new one keeps the file (link wins, see `docs/adr/0001-hardlink-names.md`).

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

## Background maintenance (as built)

- **GC:** a pass every 10 minutes looks at what the visible tree references. A chunk (replica or
  staged) unreferenced for the retention period (`gc_retention_secs`, fixed per filesystem at
  creation, default 7 days) is removed. "Unreferenced since" is when this node first saw it so,
  persisted across restarts, so a node that was away re-waits a full period after catching up.
  An orphan past retention is reclaimed only through the **reclaim gate**: since it became eligible,
  this node must have received each current member's own `Hello` frontier (lost members included,
  removed ones excluded) and applied every log entry up to it. A frontier learned second-hand does
  not count. If a member cannot be reached the gate holds and orphans stay, so space is freed late,
  never wrongly; a member gone for good unblocks GC by being removed. `status` shows pending and
  reclaimed counts and the members the gate is waiting on. No extra messages or quorum are needed.
  A held pass runs again as soon as a peer syncs, rather than waiting for the next 10-minute pass.
  Accepted residual window: a member may write identical content again just after we synced with it
  and before we reclaim (about one GC pass, 10 minutes, and only for content last referenced over the
  retention period ago). Other copies and repair restore the count; a handshake would close it and is
  deliberately not built. A returning node re-waiting the retention period is intended, and a stale
  node re-pushing reclaimed chunks is harmless: nobody can still reference them, so they are simply
  collected again.
- **Scrub:** one pass per `scrub_period_secs` (default 30 days), a few replicas per daemon tick, only
  when the tick had no other work, paused on battery power. A corrupt replica is deleted and the
  node asks other members for the chunk until one supplies it (the replicator's heal path).
- **Log compaction:** hourly, entries every non-lost peer has itself reported holding are dropped.
  A peer that missed dropped entries is sent the merge registers instead, as a state transfer in
  bounded `StateChunk`s (`state_chunk_bytes`, 1 MiB by default), one per poll on the ordered metadata
  stream. The sender pins its frontier when it starts; the receiver joins each chunk's registers as it
  arrives (idempotent) and moves its frontier only with the last chunk, so a transfer that breaks off
  never claims history the receiver lacks. If the peer's next `Hello` after the last chunk still shows
  it behind, the transfer starts over. Superseded-version bookkeeping and tombstones are only purged
  when every member, lost ones included, has reported the same frontier; a member that is gone for good has to be
  removed to unblock that. A node offline longer than the retention period is not otherwise
  special: its state joins in like any other.

## Transport (as built)

`crates/startfs/src/quic.rs` implements the poll-based `Transport` over quinn on a private background
runtime; the core stays free of async. A node's transport endpoint (`NodeId`) is a stable hash of the
`ip:port` it listens on, so every member derives the same endpoint for a peer without coordination;
members' addresses reach the transport through `Transport::learn`.

- One QUIC connection per direction per peer. The dialer opens a handshake stream carrying a magic
  and the protocol version; the acceptor checks the version, looks the dialer up with `tailscaled`
  (whois) and the `allowed_tags` policy, and answers "ok" or the reason. The reason is kept
  (`Transport::last_error`) and shows up in e.g. `startfs join` failures. Dialing is vetted the same way.
- Metadata messages share one ordered unidirectional stream; messages carrying chunk bytes
  (`Message::is_bulk`) each get their own lower-priority stream, so a big transfer never delays them.
- Frames are a 4-byte length plus the message, capped at 256 MiB. Every encoded message starts with a
  protocol version byte (`PROTOCOL_VERSION`); a mismatch fails at the handshake and in `Message::decode`.
- Certificates are throwaway and unchecked; identity is whois only. The `whois_socket` config option
  points at a non-default `tailscaled` socket (macOS needs it set).

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
- ACLs, cross-node locks, directory hardlinks
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
- Compression level (zstd 3). Done: `compression = auto` (default) samples four 16 KiB spans of a chunk and compresses it only if they shrink by an eighth, keeping the result only if the whole chunk does; `off` and `always` are the alternatives. The codec is per chunk on disk, so the setting is per node and changing it needs no migration.
- Defaults to treat as accepted: small-file threshold about 64KB, failure grace period 30 minutes,
  GC retention 7 days, scrub period 30 days
