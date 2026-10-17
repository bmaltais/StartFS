# Running StartFS at boot

`startfs daemon --mount DIR` runs the daemon and mounts the filesystem at DIR
(or set `mountpoint = "..."` in `config.toml`). SIGTERM/SIGINT unmount cleanly.

## Linux (systemd)

Install `fuse3` (provides `/dev/fuse` and `fusermount3`), then follow the
comments in `startfs.service`. `loginctl enable-linger` makes the user service
start at boot, so the mount returns after a reboot.

## macOS (launchd)

Status: written but **untested** (no Mac was available). The code is compiled
only under `target_os = "macos"`.

- **FUSE-T (preferred)**: userspace, no kernel extension. `brew install --cask fuse-t`
  (and `fuse-t-sshfs` is not needed). Building needs its libfuse-compatible
  headers/pkg-config file (`fuse-t.pc`) on `PKG_CONFIG_PATH`.
- **macFUSE (alternative)**: `brew install --cask macfuse`; needs the system
  extension approved in System Settings and a reboot. Same adapter, no code change.

Then follow the comments in `com.startfs.daemon.plist`.

## Locks

Advisory locks (`fcntl`, `flock`) are local to each machine. They work between processes
on one node but are not coordinated across nodes; the mount answers lock requests with
`ENOSYS`, so the kernel handles them locally.

## Windows

Out of scope for v1. A WinFsp adapter would implement `mount::Mounter`.
