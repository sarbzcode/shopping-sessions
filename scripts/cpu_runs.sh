#!/bin/bash
# CPU-time A/B helper (development only): cpu_runs.sh DATA_DIR [RUNS] [NCPU] [BIN]
# Prints wall and CPU seconds per run; CPU time is far less sensitive to other load than wall time.
set -eu
DATA="$1"; RUNS="${2:-3}"; THREADS="${3:-4}"; BIN="${4:-$HOME/ss/bin/ss}"
for i in $(seq 1 "$RUNS"); do
  line=$(NCPU="$THREADS" DATA_DIR="$DATA" OUTPUT_DIR="$HOME/ss/out-cpu" "$BIN" 2>&1 | grep -E '\] done')
  echo "run $i: $line"
done
