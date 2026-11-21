#!/bin/bash
# Build a pushed commit from a fresh clone, as the scorer does, and run it on a dataset with the
# scorer's flags: verify_commit.sh SHA DATA_DIR OUT_DIR [CPUS] [MEM]
set -eu
SHA="$1"; DATA="$2"; OUT="$3"; CPUS="${4:-4}"; MEM="${5:-7g}"
DIR=$(mktemp -d)
git clone -q https://github.com/sarbzcode/shopping-sessions "$DIR"
cd "$DIR"
git checkout -q "$SHA"
echo "commit $(git rev-parse HEAD), $(git ls-files | wc -l) files, repo $(du -sh .git | cut -f1)"
docker build -q --platform linux/amd64 -t "ss-verify:$SHA" . >/dev/null
docker images "ss-verify:$SHA" --format 'image {{.Size}}'
rm -rf "$OUT"
IMAGE="ss-verify:$SHA" CPUS="$CPUS" MEM="$MEM" sh scripts/docker_run.sh "$DATA" "$OUT" 2>&1 | tail -2
rm -rf "$DIR"
