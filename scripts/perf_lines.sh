#!/bin/bash
# Hottest source lines of a full run (development only): perf_lines.sh DATA_DIR [NCPU] [TOP]
set -eu
DATA=$(cd "$1" && pwd)
THREADS="${2:-4}"
TOP="${3:-45}"
OUT="$HOME/ss/perf"
mkdir -p "$OUT"
docker run --rm --privileged -v "$DATA":/data:ro -v "$OUT":/perf ss-perf bash -c "
  echo -1 > /proc/sys/kernel/perf_event_paranoid 2>/dev/null || true
  echo 0 > /proc/sys/kernel/kptr_restrict 2>/dev/null || true
  rm -rf /tmp/out
  env DATA_DIR=/data OUTPUT_DIR=/tmp/out NCPU=$THREADS perf record -F 2500 -o /perf/full.data ss 2>/perf/full-run.txt >/dev/null
  tail -3 /perf/full-run.txt
  perf report -i /perf/full.data --no-children --stdio --sort srcline 2>/dev/null | grep -v '^#' | grep -v '^$' | head -$TOP
"
