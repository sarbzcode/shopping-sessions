#!/bin/bash
# Repeated timing of the dev binary (development only): runs.sh DATA_DIR [RUNS] [NCPU]
# Prints each run's total and stage marks; use the minimum, this laptop is noisy.
set -eu
DATA="$1"; RUNS="${2:-5}"; THREADS="${3:-4}"
BIN="$HOME/ss/bin/ss"
for i in $(seq 1 "$RUNS"); do
  out=$(SS_PROFILE=1 NCPU="$THREADS" DATA_DIR="$DATA" OUTPUT_DIR="$HOME/ss/out-runs" "$BIN" 2>&1)
  total=$(echo "$out" | grep -E '\] done' | sed -E 's/\[ *([0-9.]+) ms\].*/\1/')
  s1=$(echo "$out" | grep -E '\] stage 1:' | sed -E 's/\[ *([0-9.]+) ms\].*/\1/')
  s2=$(echo "$out" | grep -E '\] stage 2:' | sed -E 's/\[ *([0-9.]+) ms\].*/\1/')
  s3=$(echo "$out" | grep -E '\] stage 3:' | sed -E 's/\[ *([0-9.]+) ms\].*/\1/')
  uf=$(echo "$out" | grep -E 'union-find:' | sed -E 's/\[ *([0-9.]+) ms\].*/\1/')
  printf "run %d: total %8s ms | uf done %7s | s1 %7s | s2 %7s | s3 %7s\n" "$i" "$total" "$uf" "$s1" "$s2" "$s3"
done
echo "$out" | grep -E '^  stage'
