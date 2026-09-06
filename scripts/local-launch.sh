#!/usr/bin/env bash
#
# app:local:launch — build and start holler-server in the background.
#
# Usage: ./scripts/local-launch.sh
#        HOLLER_STATE_DIR=/custom/path ./scripts/local-launch.sh
#
# Same setup as app:local:run (scripts/local-run.sh), but backgrounded and
# logging to $HOLLER_STATE_DIR/server.log, then prints the commands you'll
# want next (mint a token, check the roster).

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

export HOLLER_STATE_DIR="${HOLLER_STATE_DIR:-$HOME/holler-state}"
mkdir -p "$HOLLER_STATE_DIR"

if [ ! -f "$HOLLER_STATE_DIR/.pepper" ]; then
  openssl rand -hex 32 > "$HOLLER_STATE_DIR/.pepper"
  chmod 600 "$HOLLER_STATE_DIR/.pepper"
  echo "generated new pepper at $HOLLER_STATE_DIR/.pepper"
fi
export HOLLER_SERVER_PEPPER
HOLLER_SERVER_PEPPER="$(cat "$HOLLER_STATE_DIR/.pepper")"

if pgrep -f "target/release/holler serve" > /dev/null 2>&1; then
  echo "holler-server: already running (pgrep -f 'target/release/holler serve' to find it)" >&2
  exit 1
fi

cargo build --release
nohup ./target/release/holler serve --listen 127.0.0.1:41807 --advertise 127.0.0.1:41807 \
  > "$HOLLER_STATE_DIR/server.log" 2>&1 &
disown

sleep 1
if ! pgrep -f "target/release/holler serve" > /dev/null 2>&1; then
  echo "holler-server: failed to start — check $HOLLER_STATE_DIR/server.log" >&2
  exit 1
fi

echo "holler-server: running in background, logging to $HOLLER_STATE_DIR/server.log"
echo
echo "Next:"
echo "  export HOLLER_STATE_DIR=$HOLLER_STATE_DIR"
echo "  export HOLLER_SERVER_PEPPER=\"\$(cat $HOLLER_STATE_DIR/.pepper)\""
echo "  ./target/release/holler token mint --label <name>"
echo "  ./target/release/holler roster"
