#!/usr/bin/env bash
# Integration test for src/daemon/cinnamon_bg.rs: runs a fake
# org.Cinnamon.Background D-Bus-activatable service on an isolated session
# bus (dbus-run-session) and drives the real restack()/ensure_daemon_running()
# code through the `cinnamon_restack_probe` example binary.
#
# Everything below runs inside a SINGLE `dbus-run-session` invocation — each
# call to `dbus-run-session` starts its own private bus, so splitting the
# steps across several would tear the fake daemon down between them.
#
# Run directly:
#   bash tests/cinnamon_bg_fake_daemon.sh
# or via the ignored Rust test:
#   cargo test --features daemon --locked -- --ignored --nocapture \
#     daemon::cinnamon_bg::tests::restack_against_fake_daemon
set -euo pipefail

need() { command -v "$1" >/dev/null 2>&1 || { echo "SKIP: '$1' not found"; exit 0; }; }
need dbus-run-session
need python3
python3 -c 'import gi; gi.require_version("Gio","2.0")' 2>/dev/null \
  || { echo "SKIP: python3-gi (PyGObject) not available"; exit 0; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

STATE_FILE="$TMP/state.json"
SERVICE_DIR="$TMP/share/dbus-1/services"
mkdir -p "$SERVICE_DIR"
cat > "$SERVICE_DIR/org.Cinnamon.Background.service" <<EOF
[D-BUS Service]
Name=org.Cinnamon.Background
Exec=$(command -v python3) $ROOT/tests/fixtures/fake_cinnamon_background.py $STATE_FILE
EOF

echo "building cinnamon_restack_probe example..."
cargo build --quiet --example cinnamon_restack_probe --features daemon --locked
PROBE="$ROOT/target/debug/examples/cinnamon_restack_probe"

export XDG_DATA_DIRS="$TMP/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export STATE_FILE PROBE

# Everything after this point shares one private session bus.
dbus-run-session -- bash -c '
set -e
echo "-- activating the fake daemon (as Cinnamon would at login) --"
gdbus call --session --dest org.Cinnamon.Background \
  --object-path /org/Cinnamon/Background --method org.Cinnamon.Background.Start >/dev/null

echo "-- waiting for State == READY (1) --"
for i in $(seq 1 30); do
  st=$(gdbus call --session --dest org.Cinnamon.Background \
    --object-path /org/Cinnamon/Background \
    --method org.freedesktop.DBus.Properties.Get org.Cinnamon.Background State)
  case "$st" in *"uint32 1"*) break ;; esac
  sleep 0.1
  if [ "$i" -eq 30 ]; then echo "fake daemon never reached READY" >&2; exit 1; fi
done

echo "-- running restack() via the probe binary --"
"$PROBE" restack

echo "-- running ensure_daemon_running() (should be a no-op) --"
"$PROBE" ensure
'

echo "-- checking results --"
starts=$(python3 -c "import json; print(len(json.load(open('$STATE_FILE'))['starts']))")
pids=$(python3 -c "import json; print(' '.join(str(s['pid']) for s in json.load(open('$STATE_FILE'))['starts']))")
echo "total starts: $starts (pids: $pids)"

if [ "$starts" -ne 2 ]; then
  echo "FAIL: expected exactly 2 starts (initial + restack; ensure_daemon_running must not add a 3rd), got $starts" >&2
  exit 1
fi

old_pid=$(echo "$pids" | awk '{print $1}')
new_pid=$(echo "$pids" | awk '{print $2}')
if [ "$old_pid" = "$new_pid" ]; then
  echo "FAIL: restack() did not actually replace the daemon process" >&2
  exit 1
fi
if kill -0 "$old_pid" 2>/dev/null; then
  echo "FAIL: old daemon pid $old_pid is still alive after restack() (should have received SIGTERM and exited)" >&2
  exit 1
fi

echo "PASS: old pid $old_pid was terminated, new pid $new_pid was activated, ensure_daemon_running() added no extra start"
