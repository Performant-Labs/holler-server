#!/usr/bin/env bash
#
# app:local:status — show the local holler-server's roster and status.
#
# Usage: ./scripts/local-status.sh
#        HOLLER_STATE_DIR=/custom/path ./scripts/local-status.sh

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

export HOLLER_STATE_DIR="${HOLLER_STATE_DIR:-$HOME/holler-state}"

if [ ! -f "$HOLLER_STATE_DIR/.pepper" ]; then
  echo "holler-server: no pepper at $HOLLER_STATE_DIR/.pepper — has it been launched yet? (app:local:run / app:local:launch)" >&2
  exit 1
fi
export HOLLER_SERVER_PEPPER
HOLLER_SERVER_PEPPER="$(cat "$HOLLER_STATE_DIR/.pepper")"

echo "--- roster ---"
./target/release/holler roster
echo
echo "--- status ---"
./target/release/holler status
