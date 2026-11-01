# Versions are derived from the log and kept for a window

## Status

Accepted (#183).

## Context

Last-writer-wins replicates a bad write or an `rm -rf` to every member at once, and garbage
collection kept the old chunks for a while without any way to get back to them. Users need to
bring back what a path held a few days ago. The merge registers already keep every state a file
has had until the log is settled, then throw away all but the newest.

## Decision

Every member keeps the states that a file's newer state or its deletion replaced (its *versions*)
for a filesystem-wide window, `keep_versions_secs` (7 days by default, `config --keep-versions`,
gossiped like the number of copies: the higher revision wins, a tie goes to the longer window).
The window counts from when a version was replaced, not from when it was written.

- Versions are derived from log entries, as the merge registers are, so members holding the same
  entries hold the same versions. They live in their own table rather than inside the object's
  register: a file written every second would otherwise make every rebuild of the tree decode
  its history. State transfer sends them with their object.
- The chunks of a version count as referenced: the staging sweep keeps them, so they are replicated,
  and garbage collection does not reclaim them. A chunk becomes an orphan when the tree stops using
  it, as before, but is reclaimed only when no version within the window uses it either. A chunk is
  therefore held for the longer of the window and the GC retention, not their sum, and the defaults
  (both 7 days) cost no more disk than before.
- A state that lasted under 30 s is not a version, so the files a build writes and deletes within
  the replication hold still never leave the node (#188).
- A deleted object keeps its registers until its window has passed, so a deleted file can be found
  by its path, inside deleted directories too.
- A restore point is an instant. Restoring writes the version found there to whatever is at the path
  now (or creates the file), as an ordinary write, so it replicates and can itself be undone. For a
  directory it does so for every file last placed under it, and brings back the deleted directories
  they need, so an `rm -rf` is undone in one step.
- Versions begin when a database first runs a release that keeps them: a deletion stamped before
  that (on a database an earlier release wrote, from the 1.11.0 rollout) is not one, since its
  chunks may already be gone.
- `purge` does not touch versions: what they hold is released by shortening the window.

This changes the state transfer message, so it bumps the protocol version (ADR 0002).

## Considered options

- **Versions in each node's local view only**, recorded as its tree changes. Rejected: members that
  rebuilt their tree at different moments would keep different versions, and one could trim a chunk
  another still offers to restore.
- **Versions inside the object register.** Rejected for the rebuild cost above, and because the
  register's encoding has no version byte, so existing databases would need a migration.
- **Holding a version's chunks for the window on top of the GC retention.** Rejected: it would
  double the disk an overwrite costs with the defaults, for no protection the window does not give.
