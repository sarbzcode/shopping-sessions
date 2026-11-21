#!/bin/bash
# Annotated hot instructions of one function during SS_BENCH (development only):
# perf_annotate.sh DATA_DIR SYMBOL_SUBSTRING
set -eu
DATA=$(cd "$1" && pwd)
SYM="$2"
OUT="$HOME/ss/perf"
mkdir -p "$OUT"
docker run --rm --privileged -v "$DATA":/data:ro -v "$OUT":/perf ss-perf bash -c "
  echo -1 > /proc/sys/kernel/perf_event_paranoid 2>/dev/null || true
  echo 0 > /proc/sys/kernel/kptr_restrict 2>/dev/null || true
  env SS_BENCH=1 DATA_DIR=/data perf record -F 5000 -o /perf/perf.data ss >/dev/null 2>/perf/run.txt
  perf report -i /perf/perf.data --no-children --percent-limit 2 --stdio --sort symbol 2>/dev/null | grep -v '^#' | grep -v '^$' | head -12
  sym=\$(perf report -i /perf/perf.data --stdio --sort symbol 2>/dev/null | grep -o '[a-z_:]*$SYM[a-z_:]*' | head -1)
  echo \"== annotate \$sym\"
  perf annotate -i /perf/perf.data --stdio -s \"\$sym\" 2>/dev/null | awk '\$1+0 >= 0.8' | head -60
"
