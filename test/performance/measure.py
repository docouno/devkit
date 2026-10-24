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
        def signal_owned_group(value):
            try:
                os.killpg(process.pid, value)
            except ProcessLookupError:
                pass

        # communicate can time out after the leader exited because an owned
        # descendant inherited its pipes. Leader status says nothing about the
        # rest of this new session's process group.
        signal_owned_group(signal.SIGTERM)
        try:
            try:
                process.communicate(timeout=8)
            except subprocess.TimeoutExpired:
                signal_owned_group(signal.SIGKILL)
                try:
                    process.communicate(timeout=8)
                except subprocess.TimeoutExpired:
                    # An escaped process may retain a pipe. Do not wait for it
                    # without a deadline or extend ownership to unrelated PIDs.
                    pass
        finally:
            # A descendant can close its pipes but ignore TERM. Finish the
            # owned group even when draining the leader succeeded immediately.
            signal_owned_group(signal.SIGKILL)
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            process.wait(timeout=8)
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
    """Count supported single-container run requests; reject unknown mutations."""
    require(isinstance(argv, list) and argv and all(isinstance(arg, str) and arg for arg in argv),
            "Unsupported Docker trace argv")
    if argv[:1] == ["run"] or argv[:2] == ["container", "run"]:
        return "tool" if "/devkit/devkit" in argv and "rust" in argv else "other"

    def skip_options(arguments, values, switches):
        while arguments and arguments[0].startswith("-"):
            option = arguments.pop(0)
            if "=" in option and option.split("=", 1)[0] in values:
                continue
            require(option in values or option in switches,
                    f"Unsupported Docker Compose option in measured trace: {option}")
            if option in values:
                require(arguments, "Unsupported Docker trace: incomplete Compose option")
                arguments.pop(0)
        require(arguments, "Unsupported Docker trace: missing Compose operation/service")
        return arguments

    if argv[:1] == ["compose"]:
        arguments = skip_options(argv[1:], {
            "--project-directory", "--project-name", "-p", "--env-file", "--file", "-f",
            "--profile", "--parallel", "--ansi", "--progress"}, {"--compatibility"})
        operation = arguments.pop(0)
        if operation == "run":
            arguments = skip_options(arguments, {
                "--name", "--label", "-l", "--pull", "--user", "-u", "--entrypoint", "-e", "--env",
                "--env-from-file", "-v", "--volume", "--workdir", "-w", "--publish", "-p"},
                {"--rm", "-T", "--no-TTY", "--no-deps", "-d", "--detach", "--use-aliases",
                 "--quiet", "--quiet-pull", "--service-ports"})
            return "control" if arguments[0] == "control" else "tool" if arguments[0] == "rust" else "other"
        require(operation in {"version", "config", "ps", "logs", "ls", "images", "top", "port",
                              "down", "stop", "kill", "rm"},
                f"Unsupported Docker Compose operation in measured trace: {operation}")
        return None
    readonly_or_cleanup = {
        "container": {"inspect", "ls", "logs", "top", "port", "stats", "stop", "kill", "rm"},
        "image": {"inspect", "ls", "history"},
        "network": {"inspect", "ls"}, "volume": {"inspect", "ls"},
        "context": {"inspect", "ls", "show"}, "system": {"info", "df"},
    }
    if argv[0] in readonly_or_cleanup and len(argv) > 1 and argv[1] in readonly_or_cleanup[argv[0]]:
        return None
    if argv[0] in {"version", "info", "inspect", "ps", "logs", "top", "port", "stats", "stop", "kill", "rm"}:
        return None
    raise RuntimeError(f"Unsupported Docker operation in measured trace: {argv!r}")


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
