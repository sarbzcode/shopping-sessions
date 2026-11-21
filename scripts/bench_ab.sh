#!/bin/bash
# Single-threaded parse benchmark A/B (development only): bench_ab.sh DATA_DIR ROUNDS BIN1 BIN2 ...
# Prints the "full parse_events" line of each binary per round; compare the minimums.
set -u
DATA="$1"; ROUNDS="$2"; shift 2
for r in $(seq 1 "$ROUNDS"); do
  for b in "$@"; do
    line=$(SS_BENCH=1 DATA_DIR="$DATA" "$b" 2>&1 | grep -E 'full parse_events')
    printf "%-12s %s\n" "$(basename "$b")" "$line"
  done
done
