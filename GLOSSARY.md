# StartFS

A filesystem replicated across a group of machines (members) that each hold its metadata and a share of its data.

## Language

**Logical size**:
The bytes the files in the visible tree add up to, what the user thinks they stored. `df` reports it as used.
_Avoid_: Apparent size

**Referenced bytes**:
The plaintext bytes of the distinct chunks the tree references; logical size against this is the dedup ratio.

**Usable size**:
What the cluster can hold: the space members offer, divided by the number of copies. `df` reports it as the size.

**State transfer**:
The sender's whole merge state, sent in bounded pieces to a member whose missing log entries were compacted away.
_Avoid_: Snapshot, full sync

**State chunk**:
One bounded piece of a state transfer.

**Rename cycle**:
Two or more directories that, through concurrent renames on different members, would each end up inside the other.
_Avoid_: Rename loop, circular move

**Lost and found**:
The root directory (`lost+found`) that holds a directory whose rename lost in a rename cycle, so nothing becomes unreachable.
_Avoid_: Orphan directory, trash

**Id range**:
The share of the member id space that one member may hand out. Admitting a node takes its id from the admitter's range and gives it a slice of that range, so members admitting at the same time never assign the same id.

**Name**:
One directory entry of an object: a parent directory and a name. A file or symlink has one or more; a directory has exactly one. Each name has its own identity and location, so renaming or removing one leaves the others alone.
_Avoid_: Path, alias

**Link**:
An additional name for an existing file or symlink (hardlink). The object lives on until its last name is removed. Directories cannot be linked.
_Avoid_: Reference, alias

**Protocol version**:
The number two members must share before they exchange messages. It changes when messages change shape, and is distinct from the release version.
_Avoid_: Wire version, Version skew

**Incompatible member**:
A member whose protocol version differs from this node's, so the two refuse each other. `status` lists it as unreachable and says which version it speaks.

**Stand-in copy**:
An extra copy of a chunk made on an awake member to cover for a member that is unseen, so the chunk keeps N reachable copies. Kept for `standin_days` after the covered member was last seen asleep, then trimmed.
_Avoid_: Temporary replica, backup copy

**Asleep**:
A member unseen for 5 minutes but not yet lost. Others cover for it with stand-in copies. A member unseen past the repair grace period is lost instead, and its chunks are re-replicated.
_Avoid_: Offline, down (down means unseen for three hello intervals)

**Re-checking**:
A chunk held here whose placement this node has asked other members about (after a restart or a change of membership) and whose answers have not all come in. It is not counted as short of copies meanwhile, so a restart does not turn the verdict to `DEGRADED` by itself; `status` shows how many chunks this node is re-checking. A peer that stays silent ends it: the chunk is then short of copies.
_Avoid_: Rescan (that is every chunk, which only a restarted daemon still does)

**Collective memory**:
What the members remember about each other that none of them stores on disk: when a member was last seen, when it was last seen asleep, and what a member that is not up last reported holding alone. Each daemon keeps it in memory and gossips it in `Health`, so a restarted daemon relearns it from its peers as it reconnects. It is lost only if every daemon that knows it stops at once (ADR 0003).
_Avoid_: Hive mind, shared state

**Stranded chunks**:
Chunks a member that is not up last reported as a single copy. Nobody else knows they exist, so they may exist only on that member. `status` lists them and holds the verdict at risk until the member returns.

**Activity**:
The daemon's own recent rates and timings: traffic per peer path, how long each tick phase takes, and what waits in the replication queue. It describes how the daemon is working now, where `status` describes the state of the cluster.
_Avoid_: Metrics, telemetry, stats (`stats` is the storage figures)

**Behind**:
Up but backed up: a member whose last tick began 30 s or more ago, or whose inbox holds `behind_inbox_messages` or more. It is gossiped as its `Load`. `status` says how far behind it is, and other members send it nothing new until it catches up, with no change to placement (ADR 0004).
_Avoid_: Slow, lagging, down (down means unseen)

**Weight**:
The share of new copies a member takes, in the units of its offer; placement uses it in place of the offer, and 0 means the offer. A large but slow disk offers capacity with a small weight.
_Avoid_: Priority, load factor

**Purge**:
Asking every member to reclaim the chunks no file uses now instead of after the retention period. The reclaim gate still applies, so a member that has not caught up holds it back until it has. Not undoable.
_Avoid_: Cleanup, force GC

**Peer path**:
The traffic between this node and one other member, counted at each end. Two ends that disagree about the same path show loss or a stall on it.
_Avoid_: Link (a hardlink), connection, route

**Chunk cache**:
Chunks fetched from other members, kept on this node's disk up to `cache_bytes`, least recently used first. Distinct from the read cache.

**Read cache**:
Recently read chunks kept decoded in memory up to `read_cache_bytes`, so reading a file in small pieces does not decode a chunk for each piece. Unless set, its size is derived from the machine's total memory. Distinct from the chunk cache.

**Site**:
A label a member declares for where it physically lives (home, cloud). It is never detected. Placement spreads a chunk's copies across sites before it weighs members by share. A member with no label is its own site. Distinct from a member, a peer path and weight.
_Avoid_: Zone, region, location

**Staged release**:
A release a member has fetched and checked but not yet swapped in, so it is ready to take over on the word to go. Nothing restarts while a release is only staged.
_Avoid_: Pending update, downloaded update

**Rolling upgrade**:
An upgrade to a release with the same protocol version, taken one member at a time while the others keep working. Distinct from a lockstep upgrade.

**Lockstep upgrade**:
An upgrade to a release with a different protocol version. Every reachable member stages the release first, then all swap together, so the cluster cannot sync only for the restart itself. Distinct from a rolling upgrade (ADR 0002).

**Upgrade notice**:
A message in a fixed form that every release understands whatever its protocol version, saying which release the cluster is on. A member that was away and finds itself incompatible learns from it which release to catch up to. It names a release and never carries a binary.
_Avoid_: Update command, upgrade order

**Upgrade pause**:
An announced absence of a member that is about to restart for an upgrade. The others send it nothing new, make no stand-in copies for it and keep the verdict as it was, until it returns or the asleep threshold passes, when it is simply asleep.
_Avoid_: Contributing pause, maintenance mode, drain
