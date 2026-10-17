# StartFS

A filesystem replicated across a group of machines (members) that each hold its metadata and a share of its data.

## Language

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
