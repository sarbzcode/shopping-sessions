#!/bin/bash
# Profile the binary on a dataset (development only): perf.sh DATA_DIR [NCPU] [extra env, e.g. SS_BENCH=1]
# Runs inside the ss-perf image (scripts/Dockerfile.perf) with the privileges perf needs.
set -eu
DATA=$(cd "$1" && pwd)
THREADS="${2:-4}"
EXTRA="${3:-}"
OUT="${PERF_OUT:-$HOME/ss/perf}"
mkdir -p "$OUT"
docker run --rm --privileged -v "$DATA":/data:ro -v "$OUT":/perf ss-perf bash -c "
  set -e
  echo 0 > /proc/sys/kernel/kptr_restrict 2>/dev/null || true
  echo -1 > /proc/sys/kernel/perf_event_paranoid 2>/dev/null || true
  rm -rf /tmp/out
  env DATA_DIR=/data OUTPUT_DIR=/tmp/out NCPU=$THREADS $EXTRA perf record -F 2000 -g -o /perf/perf.data ss 2>/perf/run.txt || { cat /perf/run.txt; exit 1; }
  cat /perf/run.txt
  perf report -i /perf/perf.data --no-children --percent-limit 0.7 --stdio --sort symbol -g none 2>/dev/null | grep -v '^#' | grep -v '^$' | head -45
"
