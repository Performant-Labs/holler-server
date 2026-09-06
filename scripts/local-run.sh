#!/usr/bin/env bash
#
# app:local:run — build and start holler-server in the foreground.
#
# Usage: ./scripts/local-run.sh
#        HOLLER_STATE_DIR=/custom/path ./scripts/local-run.sh
#
# Creates HOLLER_STATE_DIR if missing, generates a pepper if one isn't
# already there (reuses it otherwise), then builds and runs `holler serve`
# bound to loopback in the foreground. Ctrl-C to stop.

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

echo "holler-server: state dir = $HOLLER_STATE_DIR"
cargo build --release
exec ./target/release/holler-server serve --listen 127.0.0.1:41807 --advertise 127.0.0.1:41807
