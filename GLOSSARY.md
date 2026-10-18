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

**Collective memory**:
What the members remember about each other that none of them stores on disk: when a member was last seen, when it was last seen asleep, and what a member that is not up last reported holding alone. Each daemon keeps it in memory and gossips it in `Health`, so a restarted daemon relearns it from its peers as it reconnects. It is lost only if every daemon that knows it stops at once (ADR 0003).
_Avoid_: Hive mind, shared state

**Stranded chunks**:
Chunks a member that is not up last reported as a single copy. Nobody else knows they exist, so they may exist only on that member. `status` lists them and holds the verdict at risk until the member returns.
