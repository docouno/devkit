#!/usr/bin/env python3
"""Fresh-process timing and independent host/Docker traces for prepared products."""
from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import signal
import stat
import statistics
import subprocess
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def snapshot(root):
    """All source/config/locks, including untracked files, link targets and modes."""
    root = Path(root)
    components = json.loads((root / ".devkit/manifest.json").read_text())["components"]
    excluded = {".git", ".devkit/state", ".devkit/cache", "node_modules"}
    excluded.update(f"{component}/{name}" for component in components
                    for name in ("target", "dist", "release", "node_modules"))
    records = {}
    for directory, directories, files in os.walk(root, followlinks=False):
        for name in sorted(directories + files):
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            if relative in excluded:
                if name in directories:
                    directories.remove(name)
                continue
            mode = stat.S_IMODE(path.lstat().st_mode)
            if path.is_symlink():
                records[relative] = ["link", mode, os.readlink(path)]
                if name in directories:
                    directories.remove(name)
            elif path.is_dir():
                records[relative] = ["directory", mode]
            else:
                require(path.is_file(), f"Unsupported input type: {path}")
                records[relative] = ["file", mode, sha256(path), path.stat().st_size]
    return dict(sorted(records.items()))


def execute(argv, cwd, env, timeout=120):
    started = time.perf_counter()
    process = subprocess.Popen([str(arg) for arg in argv], cwd=cwd, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.communicate(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()
        raise
    return {"seconds": time.perf_counter() - started, "exitCode": process.returncode,
            "stdout": stdout, "stderr": stderr}


def summarize(values):
    require(values and all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0
                           for v in values), "Invalid timing samples")
    ordered = sorted(values)
    return {"sampleCount": len(values), "medianSeconds": statistics.median(values),
            "p95Seconds": ordered[math.ceil(0.95 * len(values)) - 1],
            "minSeconds": ordered[0], "maxSeconds": ordered[-1]}


def paired(cases, samples, warmups, run, persist):
    """Alternate order each round to keep old/new samples close in host time."""
    require(samples >= 30 and warmups >= 2, "At least 30 samples and two warmups are required")
    require(cases and len({case["name"] for case in cases}) == len(cases),
            "Benchmark cases must have unique names")
    for case in cases:
        require(isinstance(case.get("argv"), list) and case["argv"]
                and all(isinstance(s, str) and s for s in case["argv"]),
                "Command must be a nonempty string argv")
        require(Path(case["root"]).is_dir(), "Missing benchmark product")
        case.update(samplesSeconds=[], warmupSeconds=[], sampleOrder=[])
    for index in range(warmups + samples):
        for case in cases if index % 2 == 0 else reversed(cases):
            result = run(case, index)
            require(result["exitCode"] == 0,
                    f"Benchmark failed: {case['name']}: {result['stderr'][-4000:]}")
            case["warmupSeconds" if index < warmups else "samplesSeconds"].append(result["seconds"])
            case["sampleOrder"].append(index)
        persist()
    for case in cases:
        case.update(summarize(case["samplesSeconds"]))


def docker_kind(argv):
    """Logger observes full Docker argv; classify creates, never inspect calls."""
    if argv[:1] == ["run"] or argv[:2] == ["container", "run"]:
        return "tool" if "/devkit/devkit" in argv and "rust" in argv else "other"
    if argv[:1] == ["compose"] and "run" in argv:
        arguments = argv[argv.index("run") + 1:]
        values = {"--name", "--label", "--pull", "--user", "--entrypoint", "-e", "--env", "-v", "--volume"}
        while arguments and arguments[0].startswith("-"):
            option = arguments.pop(0)
            if option in values:
                require(arguments, "Incomplete Compose run option in trace")
                arguments.pop(0)
        require(arguments, "Missing Compose service in trace")
        return "control" if arguments[0] == "control" else "tool" if arguments[0] == "rust" else "other"
    return None


def exec_counts(summary):
    attempts = errors = rows = 0
    for line in summary.splitlines():
        columns = line.split()
        if columns and columns[-1] in ("execve", "execveat"):
            require(len(columns) in (5, 6), f"Unrecognized strace summary: {line}")
            attempts += int(columns[3])
            errors += int(columns[4]) if len(columns) == 6 else 0
            rows += 1
    require(rows and attempts > 0, "strace did not report host exec syscalls")
    return {"execSyscallAttempts": attempts, "execSyscallErrors": errors,
            "successfulExecs": attempts - errors}


def docker_logger(path, real_docker):
    """An author-only wrapper, used exclusively in the separate Docker trace."""
    code = '''#!/usr/bin/python3
import json,os,subprocess,sys,time
argv=sys.argv[1:]
started=time.perf_counter()
process=subprocess.run([REAL,*argv])
elapsed=time.perf_counter()-started
with open(os.environ['DEVKIT_PERF_DOCKER_LOG'],'a') as stream:
    stream.write(json.dumps({'argv':argv,'seconds':elapsed,'exitCode':process.returncode})+'\\n')
sys.exit(process.returncode)
'''
    # repr emits a Python literal; this value never enters a shell command.
    path.write_text(code.replace("REAL", repr(str(real_docker))))
    path.chmod(0o700)
