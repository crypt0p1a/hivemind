#!/bin/sh
# Regenerates docs/dashboard.svg: runs the demo company and captures the dashboard while the
# developer builds the page (the CEO waiting on its sub-tasks, the CTO's answer done).
set -eu
cd "$(dirname "$0")/.."
cargo build --quiet
HIVE="$PWD/target/debug/hive"
DIR="$(mktemp -d)/demo"
export HIVE_HOME="$DIR"
"$HIVE" demo --no-dash --dir "$DIR" >/dev/null 2>&1 &
DEMO=$!
trap 'kill -INT $DEMO 2>/dev/null || true' EXIT
for _ in $(seq 1 200); do
  if "$HIVE" status 2>/dev/null | grep -q "dev  *busy (#4)" && "$HIVE" tasks -a 2>/dev/null | grep -q "#5 \[done\]"; then
    break
  fi
  sleep 0.25
done
sleep 2
"$HIVE" dash --svg docs/dashboard.svg --size 132x38
