# Storage-only is a declared role, and costs a protocol bump

## Status

Accepted (#142).

## Context

A member that never mounts already works: the daemon runs without a mount when no `mountpoint`
is set. But nothing made that a stated role. `status` did not say which members were not meant
to be mounted, nothing stopped someone mounting on one by mistake and creating conflict copies,
and there was no way to say "this member exists to hold copies" (an always-on box, a cloud node).

## Decision

**Declare.** Each member record has a `storage_only` flag, set at `join --storage-only` and
changed with `config --storage-only` / `--no-storage-only`, like `site` (ADR 0005). Nothing is
detected: an offer of 0 means the opposite role (a pure client, which mounts and stores nothing).

**Guard.** A storage-only member's filesystem layer refuses every operation with a distinct
error, so no path can write through it; the daemon refuses `--mount` and a configured
`mountpoint` on one. `stats` stays open: it describes the chunk store `status` reports, not files.
The role cannot be set while the node has the filesystem mounted, and a storage-only member must
offer space (joining or setting the role with an offer of 0, or dropping the offer to 0 later, is
refused).

**Otherwise ordinary.** The role does not enter placement, repair, stand-ins, GC or the reclaim
gate: a storage-only member takes its share by offer, weight and site like any member.

**Protocol.** The field is on the gossiped member record and the join request, so the protocol
version is bumped (ADR 0002) and every member is upgraded together.

## Considered options

- **Infer it from "has never mounted".** Rejected: a mount that has not happened yet is not a
  decision, and nothing would stop the first mount.
- **Treat offer 0 and storage-only as one setting.** Rejected: they are opposites (store nothing
  and mount, versus store and never mount).
- **Skip the caches on a storage-only member.** Left for later; leaving them is harmless.

## Consequences

A cluster of one mounting member and N storage-only members is "a single-node filesystem
replicated across other nodes". Retiring a storage-only member re-replicates its chunks like any
member. Mounting on it needs `config --no-storage-only` first.
