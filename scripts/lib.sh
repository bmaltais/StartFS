# Shared helpers for the validation scripts. Source this file; do not run it.
# Runs real `startfs` daemons on loopback, each with its own data dir, a fake
# tailscaled whois (so peer identification passes without a tailnet) and a
# `tailscale` shim (so daemons bind 127.0.0.1 instead of a real tailnet address).

SF_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$SF_ROOT/target/release/startfs}"
mkdir -p "$HOME/.cache"
WORK="${WORK:-$(mktemp -d -p "$HOME/.cache" sfv.XXXXXX)}"
PIDS=()

build() { (cd "$SF_ROOT" && cargo build --release -q -p startfs); }

now() { date +%s.%N; }
elapsed() { python3 -c "print(f'{$(now) - $1:.3f}')"; }

# The shim directory goes first on PATH so `tailscale ip -4` answers 127.0.0.1.
setup_env() {
  mkdir -p "$WORK/shim"
  printf '#!/bin/sh\necho 127.0.0.1\n' > "$WORK/shim/tailscale"
  chmod +x "$WORK/shim/tailscale"
  export PATH="$WORK/shim:$PATH"
  cat > "$WORK/whois.py" <<'PY'
import os, socketserver, sys
path = sys.argv[1]
body = b'{"Node":{"Name":"peer.ts.net.","Tags":["tag:startfs"]}}'
class H(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.recv(4096)
        self.request.sendall(b"HTTP/1.0 200 OK\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
if os.path.exists(path): os.unlink(path)
socketserver.ThreadingUnixStreamServer(path, H).serve_forever()
PY
  python3 "$WORK/whois.py" "$WORK/whois.sock" & PIDS+=($!)
  while [ ! -S "$WORK/whois.sock" ]; do sleep 0.05; done
}

# start_node NAME [MOUNT] [extra config lines]; sets PID_NAME. PORT_NAME=n fixes its port.
start_node() {
  local name=$1 mount=${2:-} extra=${3:-}
  local d="$WORK/$name" port
  eval "port=\${PORT_$name:-0}" # a restarted node must keep its port: peers hold its address
  mkdir -p "$d"
  printf 'port = '"$port"'\nwhois_socket = "%s"\n%s\n' "$WORK/whois.sock" "$extra" > "$d/config.toml"
  local args=(--data-dir "$d/data" --config "$d/config.toml" daemon)
  [ -n "$mount" ] && args+=(--mount "$mount")
  "$BIN" "${args[@]}" >"$d/log" 2>&1 &
  local pid=$!
  PIDS+=($pid)
  eval "PID_$name=$pid"
  while [ ! -S "$d/data/control.sock" ]; do
    kill -0 "$pid" 2>/dev/null || { echo "daemon $name died:"; cat "$d/log"; exit 1; }
    sleep 0.02
  done
}

sf() { local name=$1; shift; "$BIN" --data-dir "$WORK/$name/data" "$@"; }

cleanup() {
  set +e
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  sleep 0.3
  for d in "$WORK"/*; do fusermount3 -uz "$d/mnt" 2>/dev/null || true; done
  [ -z "${KEEP_WORK:-}" ] && rm -rf "$WORK"
  return 0
}
trap cleanup EXIT

# /proc helpers. rss_kb PID; cpu_ticks PID (utime+stime, 100 ticks/s).
rss_kb() { awk '/^VmRSS/{print $2}' "/proc/$1/status"; }
hwm_kb() { awk '/^VmHWM/{print $2}' "/proc/$1/status"; }
cpu_ticks() { awk '{print $14+$15}' "/proc/$1/stat"; }
