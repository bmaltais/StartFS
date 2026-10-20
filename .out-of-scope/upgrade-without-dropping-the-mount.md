# Upgrading the daemon without dropping the FUSE mount

StartFS does not keep the mount alive across a daemon upgrade. The mount is released when the
daemon stops, and processes using it (shells with a cwd there, editors, open files) have to
`cd` out and back in after the new daemon is up.

## Why this is out of scope

The kernel keeps a FUSE mount as long as the `/dev/fuse` fd stays open, so handing that fd to
the new daemon is possible in principle. Doing it here costs much more than it gives:

- **The FUSE library cannot resume a session.** `fuser`'s `Session::from_fd` wraps an existing
  fd but always waits for the kernel's `FUSE_INIT`, which an already-initialised connection
  never sends again. Taking over needs a patched `fuser` or an upstream change, and the new
  daemon must negotiate the same ABI version and capabilities as the old one.
- **It is Linux only.** macOS uses FUSE-T, which has no `/dev/fuse` fd to pass, so there would
  be two upgrade paths to build and test.
- **It needs handoff plumbing in two places.** Either the daemon and installer pass the fd over
  a unix socket, or the service file keeps it with systemd's `FileDescriptorStore`, which
  helps only systemd installs. Buffered writes must be flushed before the old process exits.
- **It does not remove the downtime that matters.** Upgrades are lockstep (ADR 0002), so a
  release that changes the protocol restarts every member together anyway. Only the local
  mount would survive.
- **Upgrades are rare** in a cluster of at most about 20 machines, and the installer already
  stops the daemon quickly, warns before dropping the mount and rolls back if the new daemon
  does not answer (#36).

What is not a problem, for anyone who reconsiders: inode numbers come from the metadata
database and file handles are always 0, so neither needs to survive a restart.

## Prior requests

- #45: "Upgrade the daemon without dropping the FUSE mount"
