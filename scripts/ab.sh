#!/bin/bash
# A/B CPU time of env-var variants (development only): ab.sh DATA_DIR ROUNDS "ENV=.. ENV=.." "ENV=.." ...
# Interleaves the variants each round; prints wall and CPU per run, plus profile lines matching
# $SHOW (a grep -E pattern) if set.
set -eu
DATA="$1"; ROUNDS="$2"; shift 2
for r in $(seq 1 "$ROUNDS"); do
  for v in "$@"; do
    out=$(env $v SS_PROFILE=1 NCPU=4 DATA_DIR="$DATA" OUTPUT_DIR="$HOME/ss/out-ab" "$HOME/ss/bin/ss" 2>&1)
    printf "%-26s %s\n" "$v" "$(echo "$out" | grep -E '\] done' | cut -c1-80)"
    if [ -n "${SHOW:-}" ]; then echo "$out" | grep -E "$SHOW" | sed 's/^/      /'; fi
  done
done
