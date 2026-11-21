#!/bin/bash
# Process wall time (bash `time`) vs the binary's own "done" timestamp: the gap is process
# start + exit teardown (development only). exit_cost.sh DATA_DIR [RUNS] [BIN]
set -u
DATA="$1"; RUNS="${2:-3}"; BIN="${3:-$HOME/ss/bin/ss}"
TIMEFORMAT='%R'
for i in $(seq 1 "$RUNS"); do
  rm -rf "$HOME/ss/out-exit"
  real=$( { time NCPU=4 DATA_DIR="$DATA" OUTPUT_DIR="$HOME/ss/out-exit" "$BIN" 2> "$HOME/ss/exit.log" ; } 2>&1 )
  done_ms=$(grep -E '\] done' "$HOME/ss/exit.log" | sed -E 's/\[ *([0-9.]+) ms\].*/\1/')
  echo "run $i: process wall ${real}s, done logged at ${done_ms} ms"
done
