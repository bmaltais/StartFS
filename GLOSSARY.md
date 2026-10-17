# StartFS

A filesystem replicated across a group of machines (members) that each hold its metadata and a share of its data.

## Language

**Rename cycle**:
Two or more directories that, through concurrent renames on different members, would each end up inside the other.
_Avoid_: Rename loop, circular move

**Lost and found**:
The root directory (`lost+found`) that holds a directory whose rename lost in a rename cycle, so nothing becomes unreachable.
_Avoid_: Orphan directory, trash
