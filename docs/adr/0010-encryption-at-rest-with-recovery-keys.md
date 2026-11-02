# Encryption at rest, unlocked by recovery keys

## Status

Accepted (#184). Storage-only members without the key: not built yet (see Consequences).

## Context

Chunks and the metadata database sat on disk in the clear. A copy of a member's data directory (a
discarded disk, a backup, a replica host's operator) was a copy of every file and every file name.

## Decision

**One random filesystem key, made at `create`.** It is never typed or shown. Chunks are sealed with
XChaCha20-Poly1305 (cipher id 1 in the chunk envelope), after dedup and compression, with the
envelope header and the chunk id as associated data so a body cannot be moved to another chunk's
file. The metadata database file is encrypted sector by sector (AES-256-XTS, 512-byte sectors) by a
redb storage backend, which also hides file names that are table keys. Chunk ids stay the BLAKE3 hash
of the plaintext, so dedup and compression ratios are unchanged; the cost is 24 + 16 bytes per chunk.

**Recovery keys wrap the filesystem key.** A recovery key is a random 256-bit secret, shown once
(`sfrk1-...`). The filesystem key sealed under it (a *wrap*) is public state: it is in the admission
and gossiped to every member. A joining or rebuilt machine sends a *proof* derived from the recovery
key; the admitting member checks it against the wrap before it says anything, and the joiner opens
the wrap locally.

**Last resort: a new key, not a copy.** The original recovery key is never stored, so it cannot be
shown again. A running member holds the filesystem key and `startfs recovery-key` wraps it under a
new recovery key and gossips the wrap. Nothing is re-encrypted and the older keys keep working. This
is how someone on a running machine adds a node with no stored key to look up.

**Each keyed member keeps `fs.key` (mode 0600) in its data directory**, because daemons restart on
their own (lockstep updates). The file protects every copy of the chunks and metadata that lacks it
(backups, a disk image of the stores, a replica copied elsewhere); it does not protect against someone
who controls a running member's whole data directory. Keeping the key in a keyring or TPM is future work.

**Existing filesystems stay as they are.** A chunk with cipher 0 still reads; only filesystems created
after this are encrypted, and only on a fresh node (the metadata database is not migrated in place).
`create --no-encrypt` makes a plain one. The wire carries chunk bytes as before (inside the tailnet's
WireGuard). Protocol 19 (ADR 0002): wraps, a key proof in the join request, `encrypted` on the filesystem.

## Considered options

- **Derive the filesystem key from the recovery key.** Rejected: a new recovery key could not be made
  without re-encrypting everything.
- **Store the recovery key on members so it can be shown again.** Rejected: defeats "shown once", and
  one member's disk would expose the key that opens everything.
- **Encrypt rows, not the file.** Rejected: names are table keys, there are about 130 call sites, and
  the key arrives after the database is open.
- **A keyed chunk id.** Rejected for now: chunk ids are plaintext hashes, so someone who can read
  ids and already holds a file can confirm it is stored. Keyed ids change every wire message.

## Consequences

- XTS has no authentication: a writer to the metadata file can scramble sectors (the database's
  checksums notice) or restore an older sector. Chunks are authenticated.
- Storage-only members hold the key for now, so joining one to an encrypted filesystem is refused. The
  intent (#184 discussion) is for them to hold sealed chunks without the key: chunks sent sealed,
  checked by a hash of the sealed bytes, and told which chunks are referenced by a plaintext list of
  chunk ids, since they cannot read the metadata log.
- A node that holds files cannot start encrypting; create on a fresh node.
