#!/bin/bash
# Exercises scripts/install.sh against fake releases and a fake systemctl: fresh install,
# upgrade, up to date, protocol bump, rollback and a stopped service. Needs no network.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export HOME=$work/home XDG_CONFIG_HOME=$work/home/.config
mkdir -p "$HOME" "$work/bin" "$work/state"

# A release is a tarball holding a fake `startfs` that reports a version and protocol, and
# answers `status` unless the release is marked broken.
make_release() { # tag version protocol [broken]
  local name=startfs-$1-$(uname -m | sed 's/amd64/x86_64/;s/arm64/aarch64/')-linux d=$work/rel/$1
  mkdir -p "$d" "$work/pkg/$name"
  cat >"$work/pkg/$name/startfs" <<SH
#!/bin/sh
case "\$1" in
  --version) echo "startfs $2 (protocol $3)" ;;
  status) [ "${4:-}" != broken ] ;;
esac
SH
  chmod +x "$work/pkg/$name/startfs"
  echo "[Service]" >"$work/pkg/$name/startfs.service"
  tar czf "$d/$name.tar.gz" -C "$work/pkg" "$name"
  (cd "$d" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
}
make_release v1.0.0 1.0.0 4
make_release v1.0.1 1.0.1 4
make_release v1.0.2 1.0.2 5
make_release v1.0.3 1.0.3 5 broken

# Fake systemctl: tracks "active" in a file and logs every call. When state/interrupt_on
# names the subcommand being run, it acts as Ctrl-C and signals the installer (its parent)
# once. SIGTERM stands in for SIGINT: a shell started with SIGINT ignored cannot trap that.
cat >"$work/bin/systemctl" <<SH
#!/bin/sh
echo "\$*" >>"$work/state/calls"
if [ "\$2" = "\$(cat "$work/state/interrupt_on" 2>/dev/null)" ]; then
  rm -f "$work/state/interrupt_on"
  [ "\$2" != stop ] || rm -f "$work/state/active" # a stop completes; a start is cut short
  kill -TERM \$PPID
  sleep 0.2 # the installer handles the signal once this returns, as for a real Ctrl-C
  exit 0
fi
case "\$2" in
  daemon-reload) ;;
  is-active) [ -e "$work/state/active" ] ;;
  stop) rm -f "$work/state/active" ;;
  enable|start) touch "$work/state/active" ;;
esac
SH
printf '#!/bin/sh\necho "loginctl $*" >>"%s/state/calls"\n' "$work" >"$work/bin/loginctl"
for t in fusermount3 tailscale; do printf '#!/bin/sh\n' >"$work/bin/$t"; done
chmod +x "$work/bin/"*
export PATH=$work/bin:$PATH

install_version() { # tag -> runs the installer, output in $work/out, status in $?
  : >"$work/state/calls"
  STARTFS_BASE_URL=file://$work/rel STARTFS_VERSION=$1 STARTFS_VERIFY_SECS=1 \
    sh "$here/install.sh" >"$work/out" 2>&1
}
fail() { echo "FAIL: $*"; cat "$work/out" "$work/state/calls"; exit 1; }
expect() { grep -q -- "$1" "$2" || fail "expected '$1' in $2"; }
refute() { ! grep -q -- "$1" "$2" || fail "did not expect '$1' in $2"; }
bin=$HOME/.local/bin/startfs

install_version v1.0.0 || fail "fresh install"
expect "enable --now startfs.service" "$work/state/calls"
expect "loginctl enable-linger" "$work/state/calls"
expect "startfs service is running" "$work/out"
echo "ok   fresh install enables and starts the service"
# The last lines are what a user reads before pasting the next command, so the PATH
# fix has to be there, not buried above the service messages.
tail -n 3 "$work/out" | grep -q 'export PATH="'"$HOME"'/.local/bin:\$PATH"' || fail "no PATH fix at the end"
echo "ok   a missing PATH entry comes with the command that fixes it, as the last thing printed"

# --fixpath writes the profile once and still prints the line for this shell.
fix() { STARTFS_BASE_URL=file://$work/rel STARTFS_VERSION=v1.0.0 sh "$here/install.sh" --fixpath >"$work/out" 2>&1; }
SHELL=/bin/bash fix || fail "--fixpath"
fix || fail "--fixpath again"
[ "$(grep -cF 'export PATH="'"$HOME"'/.local/bin:$PATH"' "$HOME/.profile")" = 1 ] || fail "profile line missing or duplicated"
expect "already added to PATH" "$work/out"
if sh "$here/install.sh" --bogus >"$work/out" 2>&1; then fail "unknown flag must fail"; fi
echo "ok   --fixpath adds the PATH line to the profile once; unknown flags are rejected"

PATH=$HOME/.local/bin:$PATH install_version v1.0.1 || fail "upgrade"
refute "not on your PATH" "$work/out"
expect "stop startfs.service" "$work/state/calls"
expect "enable --now startfs.service" "$work/state/calls"
expect "Upgraded 1.0.0 -> 1.0.1" "$work/out"
refute "WARNING" "$work/out"
refute "lose ~/StartFS" "$work/out"
[ "$("$bin" --version)" = "startfs 1.0.1 (protocol 4)" ] || fail "binary not replaced"
[ "$("$bin.prev" --version)" = "startfs 1.0.0 (protocol 4)" ] || fail "previous binary not kept"
echo "ok   upgrade stops, replaces, restarts and keeps the previous binary"

install_version v1.0.1 || fail "re-run"
expect "already up to date" "$work/out"
refute "stop" "$work/state/calls"
echo "ok   re-running the same version changes nothing"

install_version v1.0.2 || fail "protocol bump"
expect "WARNING: protocol 4 -> 5" "$work/out"
echo "ok   a protocol change is called out"

if install_version v1.0.3; then fail "a broken release must fail"; fi
expect "restoring the previous version" "$work/out"
[ "$("$bin" --version)" = "startfs 1.0.2 (protocol 5)" ] || fail "not rolled back"
echo "ok   a daemon that does not answer is rolled back, exit non-zero"

install_version v1.0.1 || fail "downgrade"
expect "Warning: downgrading 1.0.2 to v1.0.1" "$work/out"
rm -f "$work/state/active"
make_release v1.0.4 1.0.4 5
install_version v1.0.4 || fail "stopped service"
refute "enable --now" "$work/state/calls"
expect "left stopped" "$work/out"
echo "ok   an explicit downgrade warns; a stopped service stays stopped"

rm -rf "$HOME/.config" "$HOME/.local"
: >"$work/state/calls"
STARTFS_BASE_URL=file://$work/rel STARTFS_VERSION=v1.0.4 STARTFS_NO_SERVICE=1 \
  sh "$here/install.sh" >"$work/out" 2>&1 || fail "STARTFS_NO_SERVICE install"
[ -e "$HOME/.config/systemd/user/startfs.service" ] || fail "unit file not installed"
refute "stop\|enable\|start " "$work/state/calls"
echo "ok   STARTFS_NO_SERVICE installs the files and never touches the service"

# An interrupt after the service was stopped puts things back as they were.
rm -rf "$HOME/.config" "$HOME/.local"
install_version v1.0.0 || fail "fresh install for the interrupt tests"
echo stop >"$work/state/interrupt_on"
if install_version v1.0.1; then fail "an interrupted install must exit non-zero"; fi
expect "up to 90s" "$work/out"
expect "interrupted" "$work/out"
[ "$("$bin" --version)" = "startfs 1.0.0 (protocol 4)" ] || fail "binary changed by an interrupted stop"
[ -e "$work/state/active" ] || fail "service not running after an interrupt during the stop"
echo "ok   an interrupt during the stop restarts the service that was running"

echo enable >"$work/state/interrupt_on"
if install_version v1.0.1; then fail "an interrupted install must exit non-zero"; fi
[ "$("$bin" --version)" = "startfs 1.0.0 (protocol 4)" ] || fail "previous binary not restored"
[ -e "$work/state/active" ] || fail "service not running after an interrupt during the start"
echo "ok   an interrupt after the swap restores the previous binary and the service"

# Processes inside the mount are named before the daemon is stopped, and do not block it.
mkdir -p "$HOME/StartFS"
(cd "$HOME/StartFS" && exec sleep 600) &
holder=$!
trap 'kill $holder 2>/dev/null; rm -rf "$work"' EXIT
install_version v1.0.1 || fail "upgrade with a process inside the mount"
expect "sleep (pid $holder)" "$work/out"
expect "lose ~/StartFS" "$work/out"
[ "$("$bin" --version)" = "startfs 1.0.1 (protocol 4)" ] || fail "upgrade did not go ahead"
echo "ok   processes inside the mount are listed; with no terminal the upgrade goes ahead"
STARTFS_FORCE=1 install_version v1.0.2 || fail "forced upgrade with a process inside the mount"
expect "sleep (pid $holder)" "$work/out"
echo "ok   STARTFS_FORCE=1 lists them and goes ahead"
