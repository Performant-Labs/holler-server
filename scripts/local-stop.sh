#!/usr/bin/env bash
#
# app:local:stop — stop a background holler-server started by app:local:launch.
#
# Usage: ./scripts/local-stop.sh

set -euo pipefail

pid="$(pgrep -f "target/release/holler serve" || true)"
if [ -z "$pid" ]; then
  echo "holler-server: not running"
  exit 0
fi

kill "$pid"
echo "holler-server: stopped (pid $pid)"
