# Hardlinks: an object has a set of names

## Status

Accepted (#31).

## Context

The merge registers gave every object one place in the tree (a last-writer-wins
location), and `Delete` killed the object. A hardlink is a second name for the
same object, so "where does it live" becomes "which names does it have", and
`Unlink` has to remove one name rather than the object.

## Decision

- **Name**: an object holds a set of names. Each name is identified by the
  stamp of the op that created it (`Create` or `Link`), and carries its own
  last-writer-wins location (moved by `Rename`) and an optional removal stamp
  (set by `Unlink`). A name is live when its location is newer than its removal.
  Identifying a name by its origin stamp, not by `(parent, name)`, lets a rename
  of one name stay a rename of that name and keeps concurrent renames of the same
  name converging on one location, as before.
- **Ops**: `Create` and `Link` add a name; `Rename` and `Unlink` act on a named
  one. `Delete` is gone: removing the last name is an `Unlink`.
- **Liveness**: an object is alive while it has a live name. If it has none, a
  write newer than every removal revives it under the name removed last, as a
  write into a deleted file always has.
- **Last name against a new name (the real decision)**: an `Unlink` of the last
  name racing a `Link` of a new one resolves as **link wins**. The new name is
  live, so the object survives and no data is lost to a race. This is add-wins,
  and it falls out of per-name removal: an `Unlink` only ever removes the name it
  saw.

| Concurrent ops | Outcome |
|---|---|
| `Unlink` of the last name vs `Link` of a new name | Link wins; the object survives under the new name. |
| `Unlink` of name A vs `Unlink` of name B (the last two) | Both apply; the object dies. |
| `Rename` of name A vs `Unlink` of name A | The later stamp wins, per name. |
| `Link` into a directory vs `rmdir` of it | The path is recreated for the live name. |
| `Link` vs `Rename` of the only name | Both apply: the renamed name and the new one. |
| Two `Link`s onto the same name | The existing name-collision rule: the object with the newer content version keeps the name; the other is shown as a conflict copy. |

## Names are not `(parent, name)` pairs

The first proposal keyed `Unlink` and `Rename` by `(parent, name)`. Identifying a
name by the stamp of its creating op instead keeps a rename a rename of that
name (concurrent renames of one name still converge on one location) and lets
`Unlink` remove exactly the name it saw. `Rename` and `Unlink` therefore carry
the name's stamp, not its old `(parent, name)`.

Two names of one file can land on the same string (a `Link` onto a name while
another name is renamed onto it). They show as one entry, and `unlink` and
`rename` of that entry act on every name it stands for, so it goes or moves as a
whole.

## Consequences

- `nlink` is the number of live names, computed in `reconcile` and kept in step
  by the local `link`/`unlink` path.
- A hardlinked file's conflict copies are separate objects, as before; a
  conflict copy itself cannot be hardlinked (`EPERM`) until it is resolved.
- Concurrent writes through different names still conflict like writes through
  one name: versions belong to the object.
- State transfer carries the names with the object. `purge_settled` drops
  removed names once every member holds the whole log.
- The record format changes: `RECORD_VERSION` is bumped so an older store fails
  loudly.
