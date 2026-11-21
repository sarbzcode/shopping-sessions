#!/usr/bin/env python3
"""Run the image the way the scorer does: docker_run.py DATA_DIR OUTPUT_DIR

Cross-platform twin of docker_run.sh (works from Windows too). Same flags as the scorer:
read-only root, 64 MB /tmp tmpfs, no network, all capabilities dropped, no-new-privileges,
4096 pids, uid 1000, /data read-only, /output and /scratch writable. CPU and memory limits
come from CPUS / MEM (the scorer uses 8 and 24g; a laptop may not have that many).
Prints the wall-clock time of the container run to stderr.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time

image = os.environ.get("IMAGE", "shopping-sessions:latest")
cpus = os.environ.get("CPUS", "8")
mem = os.environ.get("MEM", "24g")
platform = os.environ.get("PLATFORM", "linux/amd64")

data = os.path.abspath(sys.argv[1])
out = os.path.abspath(sys.argv[2])
os.makedirs(out, exist_ok=True)
scratch = tempfile.mkdtemp(prefix="ss-scratch-")
for path in (out, scratch):
    try:
        os.chmod(path, 0o777)
    except OSError:
        pass

cmd = [
    "docker", "run", "--rm", "--platform", platform,
    "--network", "none", "--read-only", "--tmpfs", "/tmp:rw,size=64m,mode=1777",
    "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
    "--pids-limit", "4096", "--cpus", cpus, "--memory", mem, "--memory-swap", mem,
    "--user", "1000:1000",
    "--mount", f"type=bind,source={data},target=/data,readonly",
    "--mount", f"type=bind,source={out},target=/output",
    "--mount", f"type=bind,source={scratch},target=/scratch",
    "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/scratch", "-e", "HOME=/scratch",
    "-e", f"NCPU={cpus.split('.')[0]}", "-e", f"MEM_GB={mem.rstrip('gG')}",
    image,
]
start = time.perf_counter()
code = subprocess.call(cmd)
elapsed = time.perf_counter() - start
print(f"docker_run: exit {code}, wall {elapsed:.3f} s", file=sys.stderr)
shutil.rmtree(scratch, ignore_errors=True)
sys.exit(code)
