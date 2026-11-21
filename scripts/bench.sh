#!/bin/sh
# Time the image the way the scorer runs it, several times: bench.sh DATA_DIR [RUNS]
# Same flags as docker_run.sh. Prints each run's wall time (docker run start to exit) and the
# container's own stderr from the last run.
set -eu
IMAGE="${IMAGE:-shopping-sessions:latest}"
CPUS="${CPUS:-8}"
MEM="${MEM:-24g}"
DATA=$(cd "$1" && pwd)
RUNS="${2:-3}"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK" 2>/dev/null || true' EXIT
i=0
while [ "$i" -lt "$RUNS" ]; do
  rm -rf "$WORK/out" "$WORK/scratch"; mkdir -p "$WORK/out" "$WORK/scratch"; chmod 777 "$WORK/out" "$WORK/scratch"
  start=$(date +%s%N)
  docker run --rm --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
    --cap-drop ALL --security-opt no-new-privileges \
    --pids-limit 4096 --cpus "$CPUS" --memory "$MEM" --memory-swap "$MEM" \
    --user 1000:1000 \
    -v "$DATA":/data:ro -v "$WORK/out":/output -v "$WORK/scratch":/scratch \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
    -e NCPU="${CPUS%.*}" -e MEM_GB="${MEM%g}" \
    "$IMAGE" 2> "$WORK/stderr"
  end=$(date +%s%N)
  echo "run $i: $(( (end - start) / 1000000 )) ms"
  i=$((i + 1))
done
cat "$WORK/stderr"
if [ -n "${KEEP:-}" ]; then rm -rf "$KEEP"; cp -r "$WORK/out" "$KEEP"; fi
