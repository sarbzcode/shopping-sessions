#!/bin/bash
# Extract the static binary from an image and time it natively (no container): native_bench.sh IMAGE DATA_DIR [RUNS] [NCPU]
set -eu
IMAGE="$1"; DATA=$(cd "$2" && pwd); RUNS="${3:-3}"; THREADS="${4:-$(nproc)}"
WORK="${WORK:-$HOME/ss/work}"
mkdir -p "$WORK"
id=$(docker create "$IMAGE")
docker cp "$id:/shopping-sessions" "$WORK/bin" >/dev/null
docker rm "$id" >/dev/null
for i in $(seq 1 "$RUNS"); do
  rm -rf "$WORK/out"
  s=$(date +%s%N)
  DATA_DIR="$DATA" OUTPUT_DIR="$WORK/out" NCPU="$THREADS" "$WORK/bin" 2> "$WORK/stderr"
  e=$(date +%s%N)
  echo "native run $i: $(( (e - s) / 1000000 )) ms"
done
cat "$WORK/stderr"
