#!/usr/bin/env bash
# Runs an isolated, headless patched-niri instance for testing (never touches the live
# session): its own D-Bus session, config and Wayland/IPC sockets.
#
#   scripts/test-niri.sh            # foreground; Ctrl-C stops it
#   . build/test-niri/env           # in another shell: NIRI_SOCKET, WAYLAND_DISPLAY, DBUS_SESSION_BUS_ADDRESS
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/build/test-niri"
NIRI="${NIRI:-$ROOT/build/niri-virtual/target/release/niri}"
mkdir -p "$DIR"
rm -f "$DIR/env"   # never leave a stale env from a previous run

cat > "$DIR/config.kdl" <<KDL
// Isolated UltraDisplay test instance.
screenshot-path "$DIR/shot-%H%M%S.png"
hotkey-overlay { skip-at-startup; }
// Pinned at the origin like a real setup's main monitor (output-placement presets).
output "HEADLESS-1" { position x=0 y=0; }
debug {
    dbus-interfaces-in-non-session-instances
}
KDL

if [ -z "${UD_TEST_NIRI_INNER:-}" ]; then
    exec env UD_TEST_NIRI_INNER=1 dbus-run-session -- "$0" "$@"
fi

unset WAYLAND_DISPLAY NIRI_SOCKET DISPLAY
export NIRI_BACKEND=headless
"$NIRI" -c "$DIR/config.kdl" > "$DIR/niri.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; rm -f "$DIR/env"' EXIT INT TERM

for _ in $(seq 50); do
    sock=$(ls "$XDG_RUNTIME_DIR"/niri.wayland-*."$pid".sock 2>/dev/null || true)
    [ -n "$sock" ] && break
    sleep 0.1
done
[ -n "$sock" ] || { echo "test niri failed to start, see $DIR/niri.log" >&2; exit 1; }
wl=$(basename "$sock" | sed -E 's/^niri\.(wayland-[0-9]+)\..*/\1/')
cat > "$DIR/env" <<ENV
export NIRI_SOCKET=$sock
export WAYLAND_DISPLAY=$wl
export DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS
ENV
echo "test niri pid $pid: $sock ($wl); source $DIR/env"
wait $pid
