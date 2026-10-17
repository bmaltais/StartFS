#!/bin/sh
# Install or upgrade startfs from the prebuilt release for this machine's OS and CPU.
#   curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
# Run it again on every machine to upgrade; the README ("Upgrading") explains each step.
#
# Environment:
#   STARTFS_VERSION=v1.0.4   install this release instead of the latest (may be a downgrade)
#   INSTALL_DIR=DIR          where the binary goes (default ~/.local/bin)
#   STARTFS_NO_SERVICE=1     install files only; never stop, start or enable the service
#   STARTFS_BASE_URL=URL     download releases from URL/<tag>/ (testing)
#   STARTFS_VERIFY_SECS=15   how long to wait for the new daemon to answer before rolling back
set -eu

repo=bmaltais/StartFS
install_dir=${INSTALL_DIR:-$HOME/.local/bin}
base_url=${STARTFS_BASE_URL:-https://github.com/$repo/releases/download}
bin=$install_dir/startfs
label=com.startfs.daemon

die() { echo "install: $*" >&2; exit 1; }
say() { echo "install: $*"; }

# "startfs 1.0.1 (protocol 4)" -> 1.0.1 / 4 (older builds print no protocol).
version_of() { "$1" --version 2>/dev/null | awk '{print $2}'; }
protocol_of() { "$1" --version 2>/dev/null | sed -n 's/.*(protocol \([0-9][0-9]*\)).*/\1/p'; }
newest_of() { printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1; }

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) die "unsupported OS: $(uname -s) (Linux and macOS only)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac
target=$arch-$os
[ "$target" != x86_64-macos ] || die "no Intel Mac build; build from source"

command -v curl >/dev/null || die "curl is required"
command -v tar >/dev/null || die "tar is required"

# 1. Choose the release.
pinned=${STARTFS_VERSION:-}
version=$pinned
if [ -z "$version" ]; then
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
    die "cannot look up the latest release"
  version=${url##*/}
fi
case "$version" in v[0-9]*) ;; *) die "no release found (got '$version')" ;; esac

installed=
[ -x "$bin" ] && installed=$(version_of "$bin")
if [ -n "$installed" ]; then
  if [ "v$installed" = "$version" ]; then
    say "startfs $installed is already up to date"
    exit 0
  fi
  if [ "$(newest_of "v$installed" "$version")" = "v$installed" ]; then
    [ -n "$pinned" ] || die "installed startfs $installed is newer than latest release $version"
    say "Warning: downgrading $installed to $version; on-disk state written by a newer build may not read back."
  fi
fi

# 2. Download, verify and unpack.
name=startfs-$version-$target
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
say "Downloading $name"
curl -fsSL "$base_url/$version/$name.tar.gz" -o "$tmp/$name.tar.gz" || die "no $target build in release $version"
curl -fsSL "$base_url/$version/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256" || die "missing checksum for $name"
(
  cd "$tmp"
  if command -v sha256sum >/dev/null; then sha256sum -c "$name.tar.gz.sha256" >/dev/null
  else shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null; fi
) || die "checksum mismatch"
tar xzf "$tmp/$name.tar.gz" -C "$tmp"
new=$tmp/$name/startfs

# 3. Warn when members must be upgraded together.
old_protocol=
[ -n "$installed" ] && old_protocol=$(protocol_of "$bin")
new_protocol=$(protocol_of "$new")
if [ -n "$old_protocol" ] && [ -n "$new_protocol" ] && [ "$old_protocol" != "$new_protocol" ]; then
  say "WARNING: protocol $old_protocol -> $new_protocol. Members on different protocols cannot sync."
  say "         Upgrade every member; until they all match, 'startfs status' lists the others as unreachable."
fi

# 4. Service handling: what is managed, and is it running now?
manage=
was_running=
first_install=
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/startfs.service
if [ -z "${STARTFS_NO_SERVICE:-}" ]; then
  if [ "$os" = linux ]; then
    if systemctl --user daemon-reload 2>/dev/null; then
      manage=systemd
      [ -e "$unit" ] || first_install=1
      ! systemctl --user is-active --quiet startfs.service || was_running=1
    fi
  elif launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
    manage=launchd
    was_running=1
  fi
fi

stop_service() {
  [ -n "$was_running" ] || return 0
  say "Stopping the service (the daemon unmounts ~/StartFS cleanly)"
  case "$manage" in
    systemd) systemctl --user stop startfs.service ;;
    launchd) launchctl bootout "gui/$(id -u)/$label" ;;
  esac
}

start_service() {
  case "$manage" in
    systemd) systemctl --user enable --now startfs.service ;;
    launchd) launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$label.plist" ;;
  esac
}

# The new daemon is up once the CLI gets an answer from it.
daemon_answers() {
  n=0
  while [ "$n" -lt "${STARTFS_VERIFY_SECS:-15}" ]; do
    "$bin" status >/dev/null 2>&1 && return 0
    n=$((n + 1))
    sleep 1
  done
  return 1
}

# 5. Replace: stop, swap the binary (keeping the old one), refresh the unit, start, verify.
mkdir -p "$install_dir"
stop_service
had_binary=
if [ -e "$bin" ]; then
  had_binary=1
  cp -p "$bin" "$bin.prev"
fi
install -m755 "$new" "$bin.new"
mv -f "$bin.new" "$bin"
say "Installed $("$bin" --version) to $bin"
path_missing=
case ":$PATH:" in *":$install_dir:"*) ;; *) path_missing=1 ;; esac

if [ "$os" = linux ]; then
  mkdir -p "$(dirname "$unit")"
  install -m644 "$tmp/$name/startfs.service" "$unit"
  [ "$manage" != systemd ] || systemctl --user daemon-reload
  command -v fusermount3 >/dev/null || say "Warning: fuse3 is not installed; the mount will fail until it is."
  command -v tailscale >/dev/null || say "Warning: Tailscale is not installed; see the README."
else
  mkdir -p "$HOME/Library/LaunchAgents"
fi

rollback() {
  say "The new daemon did not come up; restoring the previous version."
  if [ -n "$had_binary" ]; then mv -f "$bin.prev" "$bin"; fi
  start_service || true
  die "upgrade to $version failed and was rolled back (see: journalctl --user -u startfs)"
}

if [ -n "$was_running" ] || [ -n "$first_install" ]; then
  start_service || { [ -n "$had_binary" ] && rollback; die "could not start the service (see: journalctl --user -u startfs)"; }
  if daemon_answers; then
    say "The startfs service is running and mounts ~/StartFS."
  elif [ -n "$had_binary" ]; then
    rollback
  else
    die "the service started but the daemon does not answer (see: journalctl --user -u startfs)"
  fi
  if [ -n "$first_install" ]; then
    loginctl enable-linger "$USER" 2>/dev/null ||
      say "Note: could not enable linger; run 'sudo loginctl enable-linger $USER' to start at boot."
  fi
elif [ -n "${STARTFS_NO_SERVICE:-}" ]; then
  say "Files installed; the service was not touched (STARTFS_NO_SERVICE is set)."
elif [ "$manage" ]; then
  say "The service was not running, so it was left stopped. Start it with: systemctl --user start startfs.service"
elif [ "$os" = linux ]; then
  say "No systemd user session; start it by hand: startfs daemon --mount ~/StartFS"
else
  say "macOS: FUSE-T (brew install --cask fuse-t) and Tailscale are required. The adapter is untested; the"
  say "launchd agent (com.startfs.daemon.plist) is in the release tarball and is not set up for you."
fi
[ -n "$installed" ] && say "Upgraded $installed -> ${version#v}. Check the cluster with: startfs status"
# Last, so it is the thing read before the next command (`startfs join ...`) is pasted.
if [ -n "$path_missing" ]; then
  say "Note: $install_dir is not on your PATH, so 'startfs' will not be found yet. In this shell, run:"
  say "  export PATH=\"$install_dir:\$PATH\""
  say "To keep it, add that line to ~/.profile (Ubuntu and Debian add ~/.local/bin by themselves at your next login)."
fi
exit 0
