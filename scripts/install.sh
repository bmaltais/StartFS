#!/bin/sh
# Install the latest prebuilt startfs release for this machine's OS and CPU.
#   curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | sh
# Environment: STARTFS_VERSION=v1.0.1 pins a release; INSTALL_DIR overrides ~/.local/bin;
# STARTFS_NO_SERVICE=1 installs the systemd unit without enabling or starting it.
set -eu

repo=bmaltais/StartFS
install_dir=${INSTALL_DIR:-$HOME/.local/bin}

die() { echo "install: $*" >&2; exit 1; }

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

version=${STARTFS_VERSION:-}
if [ -z "$version" ]; then
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
    die "cannot look up the latest release"
  version=${url##*/}
fi
case "$version" in v[0-9]*) ;; *) die "no release found (got '$version')" ;; esac

name=startfs-$version-$target
base=https://github.com/$repo/releases/download/$version
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $name"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" || die "no $target build in release $version"
curl -fsSL "$base/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256" || die "missing checksum for $name"
(
  cd "$tmp"
  if command -v sha256sum >/dev/null; then sha256sum -c "$name.tar.gz.sha256" >/dev/null
  else shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null; fi
) || die "checksum mismatch"
tar xzf "$tmp/$name.tar.gz" -C "$tmp"

mkdir -p "$install_dir"
install -m755 "$tmp/$name/startfs" "$install_dir/startfs"
echo "Installed $("$install_dir/startfs" --version) to $install_dir/startfs"
case ":$PATH:" in *":$install_dir:"*) ;; *) echo "Note: $install_dir is not on your PATH." ;; esac

if [ "$os" = linux ]; then
  unit_dir=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
  mkdir -p "$unit_dir"
  install -m644 "$tmp/$name/startfs.service" "$unit_dir/startfs.service"
  command -v fusermount3 >/dev/null || echo "Warning: fuse3 is not installed; the mount will fail until it is."
  command -v tailscale >/dev/null || echo "Warning: Tailscale is not installed; see the README."
  if [ -n "${STARTFS_NO_SERVICE:-}" ]; then
    echo "Service file installed but not started (STARTFS_NO_SERVICE is set)."
  elif systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable --now startfs.service || die "could not start startfs.service (see: journalctl --user -u startfs)"
    loginctl enable-linger "$USER" 2>/dev/null ||
      echo "Note: could not enable linger; run 'sudo loginctl enable-linger $USER' to start at boot."
    echo "The startfs service is running and mounts ~/StartFS."
  else
    echo "No systemd user session; start it by hand: startfs daemon --mount ~/StartFS"
  fi
else
  cat <<NEXT

Needs FUSE-T (brew install --cask fuse-t) and Tailscale. The macOS adapter is untested;
see the README for the whois_socket setting and the launchd agent (com.startfs.daemon.plist
is in the release tarball).
NEXT
fi
