#!/usr/bin/env python3
"""Differential test: generated datasets, DuckDB oracle vs native binary, exact comparison.

Usage:
  python scripts/difftest.py --gen PATH_TO_generate --native PATH_TO_BINARY [--seeds 1,2,5-9]
                             [--events 200000] [--clicks 50000] [--work datasets/diff]
  python scripts/difftest.py --find-windows   # seeds whose window holds 2028-02-29 or crosses a year

Each seed's dataset and oracle output are cached under --work, so re-runs only rerun the native side.
"""
import argparse
import datetime as dt
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_fixtures import SCHEMAS, load_actual  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
MASK = (1 << 64) - 1


def _mix(v):
    v = ((v ^ (v >> 30)) * 0xBF58476D1CE4E5B9) & MASK
    v = ((v ^ (v >> 27)) * 0x94D049BB133111EB) & MASK
    return v ^ (v >> 31)


def window_start(seed):
    """The generator's window_start for a seed (plan.rs: first draw of Stream::Dataset)."""
    state = _mix(_mix(seed ^ 0x5D588B656C078965) ^ _mix(0 + 1) ^ 0)
    state = (state + 0x9E3779B97F4A7C15) & MASK
    draw = _mix(state)
    first = dt.date(2025, 1, 1)
    last = dt.date(2028, 12, 31) - dt.timedelta(days=14)
    span = (last - first).days
    return first + dt.timedelta(days=(draw * span) >> 64)


def parse_seeds(text):
    out = []
    for part in text.split(","):
        if "-" in part:
            a, b = part.split("-")
            out.extend(range(int(a), int(b) + 1))
        elif part:
            out.append(int(part))
    return out


def tables_equal(a_dir, b_dir):
    ok = True
    for table, schema in SCHEMAS.items():
        a = load_actual(a_dir / table, schema)
        b = load_actual(b_dir / table, schema)
        only_a = a.join(b, on=list(schema), how="anti", nulls_equal=True).height
        only_b = b.join(a, on=list(schema), how="anti", nulls_equal=True).height
        if a.height != b.height or only_a or only_b:
            ok = False
            print(f"    {table}: rows {a.height} vs {b.height}, only native {only_a}, only oracle {only_b}")
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gen")
    ap.add_argument("--native", nargs="+")
    ap.add_argument("--seeds", default="1-10")
    ap.add_argument("--events", type=int, default=200_000)
    ap.add_argument("--clicks", type=int, default=50_000)
    ap.add_argument("--work", default=str(ROOT / "datasets" / "diff"))
    ap.add_argument("--find-windows", action="store_true")
    args = ap.parse_args()

    if args.find_windows:
        leap = dt.date(2028, 2, 29)
        for seed in range(1, 5000):
            ws = window_start(seed)
            we = ws + dt.timedelta(days=14)
            tags = []
            if ws <= leap < we:
                tags.append("holds 2028-02-29")
            if ws.year != (we - dt.timedelta(days=1)).year:
                tags.append("crosses a year")
            if tags:
                print(seed, ws, ", ".join(tags))
        return 0

    work = Path(args.work)
    failures = []
    for seed in parse_seeds(args.seeds):
        tag = f"s{seed}-e{args.events}-c{args.clicks}"
        data, oracle, native = work / tag / "data", work / tag / "oracle", work / tag / "native"
        if not (data / "manifest.json").is_file():
            subprocess.run([args.gen, "--seed", str(seed), "--events", str(args.events), "--clicks",
                            str(args.clicks), "--output", str(data)], check=True, capture_output=True)
        env = dict(os.environ, DATA_DIR=str(data), SCRATCH_DIR=str(work / "scratch"), MEM_GB="6")
        if not (oracle / "campaign_daily").is_dir():
            t = time.time()
            p = subprocess.run([sys.executable, str(ROOT / "pipeline" / "pipeline.py")],
                               env=dict(env, OUTPUT_DIR=str(oracle)), capture_output=True, text=True)
            if p.returncode != 0:
                print(f"{tag}: ORACLE FAILED\n{p.stderr[-2000:]}")
                failures.append(seed)
                continue
            oracle_s = time.time() - t
        else:
            oracle_s = 0.0
        if native.exists():
            import shutil
            shutil.rmtree(native)
        t = time.time()
        p = subprocess.run(args.native, env=dict(env, OUTPUT_DIR=str(native)), capture_output=True, text=True)
        native_s = time.time() - t
        if p.returncode != 0:
            print(f"{tag}: NATIVE FAILED (exit {p.returncode})\n{p.stderr[-2000:]}")
            failures.append(seed)
            continue
        ok = tables_equal(native, oracle)
        print(f"{tag} window {window_start(seed)}: {'MATCH' if ok else 'DIFFER'} "
              f"(oracle {oracle_s:.1f}s, native {native_s:.2f}s)", flush=True)
        if not ok:
            failures.append(seed)
    print(f"\n{len(parse_seeds(args.seeds)) - len(failures)}/{len(parse_seeds(args.seeds))} seeds match"
          + (f"; failing: {failures}" if failures else ""))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
