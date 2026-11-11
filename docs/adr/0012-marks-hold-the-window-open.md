# Marks hold the version window open back to an instant

## Status

Accepted (#265).

## Context

Versions (ADR 0008) are kept for a rolling window, so a state a user cares about ("before the
migration") is gone a week later, and nothing lets them say "keep this". Other filesystems offer
named snapshots. The word is avoided here: *snapshot* is already the code's name for one state of
an object and the glossary's old name for state transfer. The feature is a **mark**.

## Decision

A mark is a name for an instant on the creating member's clock. `mark create` first commits the
creating node's buffered writes, so the mark includes what the user has already written.

**The window grows to the oldest mark.** The effective window is the longer of
`keep_versions_secs` and the age of the oldest live mark. A version is kept when it ended after the
mark's instant, which is exactly the set `restore` needs to put any path back to what it held at that
instant. The retention test (`kept`), garbage collection, the version bytes, compaction and
`history`/`restore` all read the one window, so none of them needed to learn about marks. Deleting
the oldest mark lets the window fall back, and the next GC pass releases what only it held. A
state that ended at the very instant of the mark is not the one the mark names, as with `restore`.

**Marks are a gossiped table with tombstones.** Each name has an entry `(at, by, rev)`. Creating
takes `rev` from nothing to 1, deleting adds 1, creating again adds 1, so a live mark has an odd
`rev` and a deleted one an even `rev`, and two members never disagree about whether an entry is
deleted at the same revision. Per name the higher `rev` wins, so a delete beats a create a member
missed. If two members create one name at once (equal `rev`), the earlier instant wins, which keeps
more, then the lower member id, so every member settles on the same entry. Deleted entries stay in
the table forever: dropping them would let a member that was away bring a deleted mark back. A
member makes at most 256 names (live and deleted), which bounds the message; names others made
are always taken in, so a cap never leaves members with different sets (a cluster that has made
256 distinct names can only reuse deleted ones). The whole table is gossiped
like `DirCopies`, in a new `Marks` message, so the protocol is bumped to 21 (ADR 0002).

**Naming.** A mark's name is 1 to 64 characters of letters, digits, `-`, `_` and `.`, and must not
read as a restore point, so `restore --at X` tells a point from a mark by form.

**Restore.** `restore PATH --at NAME` resolves the name to its instant on the member that receives
it and restores as before: an ordinary write that replicates and can itself be undone.

## Consequences

- Disk held only because of a mark is reported (`status`, `efficiency.mark_bytes`), and `df`
  counts it as used with the other version bytes.
- A mark protects what each member still holds when it learns of the mark. Every version it needs
  ended after the mark was made, so it is inside the window on every member that was up to see it,
  and a returning member receives versions with the log or the state transfer.
- **Limits.** Versions are per file, found by the path last named. A file renamed since the mark
  is not brought back to its old place, and directory states are not versions (ADR 0008, *Not
  done*). States shorter than 30 s are not kept. A browsable read-only view of a mark needs the tree
  as of an instant, which this does not give: that is #266.
- Marks never expire by themselves: a forgotten mark holds disk until deleted, which `status` shows.
- The clock is the creating member's. A member whose clock is far ahead or behind names an instant
  that is off by as much, like the restore points in `history`.

## Considered options

- **Pin the versions a mark needs one by one.** Rejected: it adds a second retention mechanism
  beside the window, and every reader of the window would need to know it. Extending the window
  reuses all of them.
- **A per-mark timestamp comparison in `kept`.** Equivalent in what it keeps, but it makes `kept`
  depend on gossiped state; the window is already a single number the filesystem holds.
- **Dropping tombstones after a while.** Rejected: a member away longer than that would revive the
  mark.
