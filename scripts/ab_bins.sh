#!/bin/bash
# A/B CPU time of binaries (development only): ab_bins.sh DATA_DIR ROUNDS BIN1 BIN2 ...
# Interleaves the binaries each round; SHOW (grep -E pattern) adds profile lines.
set -eu
DATA="$1"; ROUNDS="$2"; shift 2
for r in $(seq 1 "$ROUNDS"); do
  for b in "$@"; do
    out=$(SS_PROFILE=1 NCPU=4 DATA_DIR="$DATA" OUTPUT_DIR="$HOME/ss/out-ab" "$b" 2>&1)
    printf "%-12s %s\n" "$(basename "$b")" "$(echo "$out" | grep -E '\] done' | cut -c1-80)"
    if [ -n "${SHOW:-}" ]; then echo "$out" | grep -E "$SHOW" | sed 's/^/      /'; fi
  done
done
