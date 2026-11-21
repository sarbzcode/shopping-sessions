#!/bin/bash
# Scale check (development only): run the dev binary on DATA, sampling its memory mappings and
# peak RSS: scale_check.sh DATA_DIR [NCPU]
set -u
DATA="$1"; THREADS="${2:-4}"
OUT="$HOME/ss/out-scale"
rm -rf "$OUT"
SS_PROFILE=1 NCPU="$THREADS" DATA_DIR="$DATA" OUTPUT_DIR="$OUT" "$HOME/ss/bin/ss" 2> "$HOME/ss/scale.log" &
pid=$!
max_maps=0
while kill -0 "$pid" 2>/dev/null; do
  n=$(wc -l < "/proc/$pid/maps" 2>/dev/null || echo 0)
  [ "$n" -gt "$max_maps" ] && max_maps=$n
  hwm=$(grep VmHWM "/proc/$pid/status" 2>/dev/null | awk '{print $2}')
  [ -n "$hwm" ] && last_hwm=$hwm
  sleep 0.05
done
wait "$pid"; code=$?
echo "exit $code, max mappings $max_maps, peak RSS $(( ${last_hwm:-0} / 1024 )) MB"
grep -E '\] (window|union|stage [123]:|campaign|done)' "$HOME/ss/scale.log"
ls "$OUT/user_daily" | wc -l | xargs echo "user_daily files:"
