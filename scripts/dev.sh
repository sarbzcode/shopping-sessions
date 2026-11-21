#!/bin/bash
# Development loop on Linux (inside the WSL2 VM, via Docker): dev.sh build | bench DATA | run DATA OUT | test
# Builds with the same flags as the image into a cached volume; binary at $HOME/ss/bin/ss.
set -eu
SRC=/mnt/e/shopping-sessions
BIN="$HOME/ss/bin"
mkdir -p "$BIN"
cargo() {
  docker run --rm -v "$SRC":/src:ro -v ss-cargo-target:/target -v ss-cargo-registry:/usr/local/cargo/registry \
    -v "$BIN":/out -w /src -e CARGO_TARGET_DIR=/target \
    -e RUSTFLAGS="-C target-cpu=x86-64-v2 -C target-feature=+crt-static" \
    rust:1.93-slim-bookworm bash -c "$1"
}
case "$1" in
  build) cargo "cargo build --release --target x86_64-unknown-linux-gnu 2>&1 | grep -E '^(error|warning: unused)|Finished' ; cp /target/x86_64-unknown-linux-gnu/release/shopping-sessions /out/ss" ;;
  test) cargo "cargo test --release --target x86_64-unknown-linux-gnu 2>&1 | grep -E '^test |test result|panicked|error'" ;;
  bench) for i in 1 2 3; do SS_BENCH=1 DATA_DIR="$2" "$BIN/ss"; done ;;
  run) rm -rf "$3"; DATA_DIR="$2" OUTPUT_DIR="$3" NCPU="${NCPU:-4}" "$BIN/ss" ;;
  *) echo "usage: dev.sh build|test|bench DATA|run DATA OUT"; exit 2 ;;
esac
