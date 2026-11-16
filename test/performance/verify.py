#!/usr/bin/env python3
"""Gate a frozen Linux release with paired public-0.4.1/native warm benchmarks.

Usage: python3 test/performance/verify.py --build-set DIR --output receipt.json
Requires Docker Compose and the built tooling/native.Dockerfile image in
DEVKIT_PERFORMANCE_TRACE_IMAGE. Products and private traces are retained outside
the source checkout. This harness never clears OS, Docker, or global SDK caches.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import sys
import tarfile
import tempfile
import time
import urllib.request
import uuid

from measure import (digest, docker_kind, docker_logger, exec_counts, execute,
                     paired, require, sha256, snapshot, summarize)


REPO = Path(__file__).resolve().parents[2]
if str(REPO / "test") not in sys.path:
    sys.path.insert(0, str(REPO / "test"))
from docker_flags import author_docker_flags  # noqa: E402
BASELINE_VERSION = "0.4.1"
BASELINE_RELEASE_IDENTITY = "0c2f111d8d53cd95aaa9dc7ed6e34e4c764d96ea4f537a7e758ee2f44e5b8a5a"
BASELINE_ARCHIVE_SHA256 = "4c76c7ffd0ab48c710512af9874f8a1bc189bb663b939daa6ddc76522511143a"
BASELINE_URL = "https://github.com/docouno/devkit/releases/download/v0.4.1/devkit-0.4.1.tar.gz"
LARGE_CRATES = 80
SHAPES = (("one", 1, 0), ("five", 5, 0), ("large-closure", 1, LARGE_CRATES))
RELATIVE_FACTOR = 1.20
JITTER_MARGIN_SECONDS = 0.020
HELP_MAX_MEDIAN_SECONDS = 0.050


def atomic_json(path, value):
    temporary = path.with_name(path.name + ".writing")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def clean_environment(cache):
    environment = dict(os.environ)
    for key in list(environment):
        if key.startswith(("COMPOSE_", "DEVKIT_", "npm_", "NPM_", "pnpm_", "PNPM_")) or key in (
                "MAKEFLAGS", "MFLAGS", "MAKELEVEL", "NODE_OPTIONS", "NODE_PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"):
            environment.pop(key)
    environment.update(CI="true", LC_ALL="C", DEVKIT_CACHE_DIR=str(cache))
    return environment


def extract_release(archive, destination):
    destination.mkdir()
    with tarfile.open(archive) as source:
        members = source.getmembers()
        require(len(members) <= 4096 and sum(m.size for m in members) <= 256 * 1024**2,
                "Release archive exceeds budget")
        seen = set()
        for member in members:
            path = Path(member.name)
            require(not path.is_absolute() and ".." not in path.parts and member.name not in seen,
                    "Unsafe or duplicate archive path")
            require(member.isdir() or member.isfile(), "Release links and special files are forbidden")
            seen.add(member.name)
        source.extractall(destination, filter="data")
    manifests = list(destination.glob("*/release.json"))
    if (destination / "release.json").is_file():
        manifests.append(destination / "release.json")
    require(len(manifests) == 1, "Expected exactly one release root")
    root = manifests[0].parent
    verify_payload(root)
    return root


def verify_payload(root):
    manifest = json.loads((root / "release.json").read_text())
    actual = {}
    for path in root.rglob("*"):
        require(not path.is_symlink(), "Release payload contains a symlink")
        relative = path.relative_to(root).as_posix()
        if path.is_file() and relative != "release.json":
            actual[relative] = {
                "sha256": sha256(path), "mode": stat.S_IMODE(path.stat().st_mode)}
    require(actual == manifest["files"], "Payload differs from its canonical release manifest")
    return manifest


def validate_build_set(directory):
    raw = directory / "release-index.json"
    index = json.loads(raw.read_text())
    require(index["schemaVersion"] == 1 and index["target"] == "x86_64-unknown-linux-musl",
            "Expected a schema-1 Linux x86_64 build set")
    require(re.fullmatch(r"\d+\.\d+\.\d+", index["version"])
            and all(re.fullmatch(r"[a-f0-9]{40}", index[key]) for key in ("sourceCommit", "sourceTree")),
            "Invalid release source identity")
    require(index.get("publicationEligible") is True, "Source-attributed public build set required")
    for name, identity in index["files"].items():
        require(re.fullmatch(r"[A-Za-z0-9._-]+", name) and name not in (".", ".."), "Unsafe asset name")
        path = directory / name
        require(path.is_file() and not path.is_symlink() and path.stat().st_size == identity["size"]
                and sha256(path) == identity["sha256"], f"Build asset identity mismatch: {name}")
    require(index["archive"] in index["files"] and index["engine"] in index["files"]
            and index["releaseIdentity"] == index["files"]["release.json"]["sha256"],
            "Build set is missing its declared payload")
    return index, sha256(raw)


class Gate:
    def __init__(self, args):
        self.args = args
        self.scratch = Path(tempfile.mkdtemp(prefix="devkit-performance-"))
        self.logs = self.scratch / "logs"
        self.logs.mkdir()
        self.environment = clean_environment(self.scratch / "native cache")
        self.serial = 0
        self.receipt = {
            "schemaVersion": 1, "success": False, "measurements": [], "comparisons": [],
            "fixtures": [], "admissionChecks": [], "commands": [], "scratch": str(self.scratch),
            "definitions": {
                "warm": "Fresh command process, prepared accepted images/dependencies/target caches, two explicit warmups, alternating old/new round order. No tracing or PATH logger in timed runs. OS page cache is not dropped.",
                "latency": "Wall time includes real Make DAG, admission, Docker CLI/daemon, required Rust gates and Cargo. median and nearest-rank p95; raw samples retained.",
                "hostProcesses": "Separate strace -f -c execve/execveat run: exact host-descendant exec syscall attempts/errors/successes. Container-daemon children are outside this scope. Successful execs are not unique PIDs.",
                "containers": "Separate Docker PATH logger run: Docker CLI invocations and successful single-container run requests classified by control or Rust tool. oneShotContainers counts these run requests, not daemon events or all container processes. Unsupported create/start/up and unknown mutating operations fail closed; explicit readonly queries and cleanup are allowed.",
                "tooling": "Separate Docker trace records wall time of the real Rust tool container invocations including their mandatory Rust gate. Its instrumented time is excluded from plain latency; residual is a diagnostic, not a compiler-speed claim.",
                "cold": "Not measured here. This receipt covers installed warm execution. Actual cold public npx/curl transport belongs to the public verification receipt and is required separately before promotion.",
                "inputs": "Every source/config/lock and untracked file, directories/modes/link targets. Excludes only Git, .devkit state/cache and registered component native outputs/dependency directories.",
                "fixture": "Both real release factories create matched Rust products. Product code, template README and actual Cargo-generated locks are normalized to identical std-only fixtures to expose controller costs. Large closure is an identical chain of 80 local crates on both sides.",
            },
            "limits": {
                "minimumSamples": 30, "warmups": args.warmups, "p95": "nearest-rank ceil(0.95*n)",
                "nonRegressionFormula": "native_median <= baseline_median * 1.20 + 0.020",
                "helpCeilingSeconds": HELP_MAX_MEDIAN_SECONDS,
                "requireControlContainers": 0, "requireOtherContainers": 0,
                "requireHelpAndDescribeDockerCalls": 0,
                "requireBuildToolContainersPerComponent": 1,
                "setupAndLockResolution": "Outside measurement; SDK containers are capped at two CPUs, 2 GiB and one Cargo job. Timed production commands use unchanged accepted runtime defaults; no source or gate instrumentation.",
                "support": "Linux x86_64 with local Docker Compose; no host language SDK needed",
            },
            "harness": {path.name: sha256(path) for path in Path(__file__).parent.glob("*.py")},
            "host": {"system": platform.system(), "machine": platform.machine(),
                     "kernel": platform.release(), "python": platform.python_version(),
                     "cpuCount": os.cpu_count(), "cpuModel": self.cpu_model(),
                     "meminfo": self.meminfo(), "loadAverageAtStart": os.getloadavg(),
                     "uid": os.getuid(), "gid": os.getgid()},
        }

    @staticmethod
    def cpu_model():
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
        return "unknown"

    @staticmethod
    def meminfo():
        return {line.split(":", 1)[0]: line.split(":", 1)[1].strip()
                for line in Path("/proc/meminfo").read_text().splitlines()
                if line.split(":", 1)[0] in ("MemTotal", "MemAvailable", "SwapTotal", "SwapFree")}

    def is_publication_sampled(self):
        return getattr(self.args, "samples", 30) >= 30 and getattr(self.args, "warmups", 2) >= 2

    def save(self):
        if not self.is_publication_sampled():
            self.receipt["success"] = False
            self.receipt["diagnosticUnderSampled"] = True
        atomic_json(self.args.output, self.receipt)

    def run(self, label, argv, *, cwd=None, env=None, expected=0, timeout=120):
        self.serial += 1
        logfile = self.logs / f"{self.serial:05d}-{label}.json"
        result = execute(argv, cwd or self.scratch, env or self.environment, timeout)
        atomic_json(logfile, {"argv": [str(a) for a in argv], **result})
        self.receipt["commands"].append({"label": label, "log": str(logfile),
                                        "exitCode": result["exitCode"], "seconds": result["seconds"]})
        require(result["exitCode"] == expected if expected is not None else result["exitCode"] != 0,
                f"{label}: exit {result['exitCode']}; {result['stderr'][-5000:]} (log {logfile})")
        return result

    def baseline(self):
        source = self.args.baseline_release
        if source is None:
            source = self.scratch / "devkit-0.4.1.tar.gz"
            with urllib.request.urlopen(BASELINE_URL, timeout=60) as response, source.open("wb") as output:
                require(response.status == 200, "Public baseline download failed")
                total = 0
                while chunk := response.read(1024 * 1024):
                    total += len(chunk)
                    require(total <= 128 * 1024**2, "Public baseline exceeds budget")
                    output.write(chunk)
        if source.is_file():
            require(sha256(source) == BASELINE_ARCHIVE_SHA256, "Public 0.4.1 archive identity mismatch")
            release = extract_release(source, self.scratch / "baseline release")
        else:
            verify_payload(source)
            release = self.scratch / "baseline release"
            shutil.copytree(source, release)
        require(sha256(release / "release.json") == BASELINE_RELEASE_IDENTITY,
                "Baseline must be the exact immutable public 0.4.1 payload")
        require((release / "devkit").read_bytes()[:4] == b"\x7fELF",
                "Baseline release must ship a native Linux ELF engine")
        self.receipt["baseline"] = {"version": BASELINE_VERSION, "source": str(source),
                                    "publicArchiveUrl": BASELINE_URL,
                                    "archiveSha256": BASELINE_ARCHIVE_SHA256,
                                    "releaseIdentity": BASELINE_RELEASE_IDENTITY}
        return release

    def tracing(self):
        docker = shutil.which("docker", path=self.environment["PATH"])
        require(docker, "Docker CLI is required")
        self.docker = Path(docker).resolve()
        self.receipt["tools"] = {
            "dockerPath": str(self.docker), "dockerSha256": sha256(self.docker),
            "dockerVersion": json.loads(self.run("docker-version", [docker, "version", "--format", "{{json .}}"])["stdout"]),
            "composeVersion": self.run("compose-version", [docker, "compose", "version"])["stdout"].strip(),
            "makeVersion": self.run("make-version", ["make", "--version"])["stdout"].splitlines()[0],
        }
        image = os.environ.get("DEVKIT_PERFORMANCE_TRACE_IMAGE")
        require(image, "Set DEVKIT_PERFORMANCE_TRACE_IMAGE to the built tooling/native.Dockerfile image")
        image_data = json.loads(self.run("trace-image", [docker, "image", "inspect", image])["stdout"])[0]
        self.sdk = image_data["Id"]
        self.receipt["tools"].update(traceImageId=self.sdk, traceImageRepoDigests=image_data["RepoDigests"],
                                      traceRecipeSha256=sha256(REPO / "tooling/native.Dockerfile"))
        self.receipt["tools"]["sdkVersions"] = self.run("sdk-versions", [
            docker, "run", "--rm", "--pull", "never", "--network", "none",
            *author_docker_flags(1, 256), self.sdk, "sh", "-c", "rustc --version; cargo --version; rustfmt --version; cargo clippy --version"])["stdout"].splitlines()
        # Extract the trusted image's strace and its actual dynamic dependencies.
        # Explicit loader arguments affect only strace, never the traced child env.
        ldd = self.run("trace-ldd", [docker, "run", "--rm", "--pull", "never", "--network", "none",
                                    *author_docker_flags(1, 256), "--entrypoint", "/usr/bin/ldd",
                                    self.sdk, "/usr/bin/strace"])["stdout"]
        libraries = sorted(set(re.findall(r"(/[\w/+.\-]+)\s+\(0x", ldd)))
        require(libraries and any("ld-linux" in name for name in libraries), "No strace dynamic loader")
        directory = self.scratch / "trace tools"
        directory.mkdir()
        name = "devkit-perf-extract-" + uuid.uuid4().hex
        container_id = self.run("trace-create", [docker, "create", "--name", name, self.sdk, "true"])["stdout"].strip()
        require(re.fullmatch(r"[a-f0-9]{64}", container_id), "Unexpected extractor container ID")
        try:
            for source in ["/usr/bin/strace", *libraries]:
                self.run("trace-copy", [docker, "cp", "-L", f"{container_id}:{source}", directory / Path(source).name])
        finally:
            self.run("trace-remove", [docker, "container", "rm", container_id])
        loader = next(path for path in directory.iterdir() if "ld-linux" in path.name)
        self.strace = [str(loader), "--library-path", str(directory), str(directory / "strace")]
        self.receipt["tools"]["straceFiles"] = {path.name: sha256(path) for path in directory.iterdir()}
        self.receipt["tools"]["straceVersion"] = self.run("trace-version", [*self.strace, "--version"])["stdout"].splitlines()[0]
        self.logger = self.scratch / "docker logger"
        self.logger.mkdir()
        docker_logger(self.logger / "docker", self.docker)

    def edit_product(self, root, count, closure):
        for number in range(1, count + 1):
            app = root / "apps" / f"app{number:03}"
            manifest = app / "Cargo.toml"
            manifest_mode = manifest.stat().st_mode & 0o777 if manifest.exists() else 0o644
            shutil.rmtree(app / "src")
            shutil.rmtree(app / "tests")
            (app / "src").mkdir()
            (app / "src").chmod(0o755)
            dependencies = ""
            if closure:
                vendor = app / "vendor"
                vendor.mkdir(exist_ok=True)
                vendor.chmod(0o755)
                for index in range(closure):
                    crate = vendor / f"dep{index:03}"
                    (crate / "src").mkdir(parents=True)
                    crate.chmod(0o755)
                    (crate / "src").chmod(0o755)
                    declaration = f'dep{index-1:03} = {{ path = "../dep{index-1:03}" }}\n' if index else ""
                    crate_manifest = crate / "Cargo.toml"
                    crate_manifest.write_text(
                        f'[package]\nname = "dep{index:03}"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n{declaration}')
                    crate_manifest.chmod(0o644)
                    expression = f"dep{index-1:03}::value() + 1" if index else "1"
                    crate_lib = crate / "src/lib.rs"
                    crate_lib.write_text(f"pub fn value() -> usize {{\n    {expression}\n}}\n")
                    crate_lib.chmod(0o644)
                dependencies = f'dep{closure-1:03} = {{ path = "vendor/dep{closure-1:03}" }}\n'
            expression = f"dep{closure-1:03}::value()" if closure else "1"
            manifest.write_text(
                f'[package]\nname = "bench-app{number:03}"\nversion = "0.1.0"\nedition = "2024"\nrust-version = "1.99.1"\npublish = false\n\n[dependencies]\n{dependencies}')
            manifest.chmod(manifest_mode)
            app_lib = app / "src/lib.rs"
            app_lib.write_text(
                f"pub fn value() -> usize {{\n    {expression}\n}}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn value_is_expected() {{\n        assert_eq!(super::value(), {closure or 1});\n    }}\n}}\n")
            app_lib.chmod(0o644)
            (app / "Cargo.lock").unlink()
            # Historical template prose differs; compare identical fixture inputs.
            readme = app / "README.md"
            readme.write_text("# Matched Rust performance fixture\n")
            readme.chmod(0o644)

    def product_inputs(self, root):
        return {name: value for name, value in snapshot(root).items() if name.startswith("apps/")}

    def make(self, root, target, *, label=None, expected=0):
        return self.run(label or target, ["make", "--no-print-directory", target], cwd=root,
                        expected=expected, timeout=600)

    def create(self, release, side, shape, count, closure):
        root = self.scratch / "products with spaces" / shape / side / "product"
        root.parent.mkdir(parents=True)
        expected_version = BASELINE_VERSION if side == "baseline" else self.receipt["version"]
        argv = [release / "devkit", "--non-interactive", "init", root,
                "--name", "devkit-performance", "--package-source", "local"]
        for number in range(1, count + 1):
            argv += ["--app", f"app{number:03}=rust-api"]
        argv += ["--to", expected_version, "--release-root", release]
        self.run(f"init-{shape}-{side}", argv, timeout=600)
        self.edit_product(root, count, closure)
        # Cargo itself resolves every lock. No fabricated accepted state or lock.
        script = "set -eu\n" + "\n".join(
            f"cd /workspace/apps/app{number:03}; cargo generate-lockfile --offline"
            for number in range(1, count + 1))
        self.run(f"resolve-{shape}-{side}", [
            self.docker, "run", "--rm", "--pull", "never", "--network", "none",
            *author_docker_flags(2, 2048, pids_limit=256), "--user", f"{os.getuid()}:{os.getgid()}",
            "--volume", f"{root}:/workspace", "--env", "HOME=/tmp", "--env", "CARGO_HOME=/tmp/cargo",
            "--env", "CARGO_BUILD_JOBS=1", "--env", "RUSTUP_AUTO_INSTALL=0", self.sdk,
            "sh", "-c", script], timeout=600)
        before = snapshot(root)
        self.make(root, "setup", label=f"setup-{shape}-{side}")
        self.make(root, "check", label=f"check-{shape}-{side}")
        self.make(root, "build", label=f"build-prime-{shape}-{side}")
        require(snapshot(root) == before, f"Fixture setup/check/build changed source: {shape}/{side}")
        state = json.loads((root / ".devkit/manifest.json").read_text())
        expected_version = BASELINE_VERSION if side == "baseline" else self.receipt["version"]
        require(state["version"] == expected_version and len(state["components"]) == count,
                "Factory did not create the intended accepted component matrix")
        item = {"shape": shape, "side": side, "root": str(root), "componentCount": count,
                "localTransitiveCrates": closure, "version": state["version"],
                "releaseIdentity": state["releaseIdentity"], "inputSnapshotSha256": digest(before),
                "inputFiles": sum(value[0] == "file" for value in before.values()),
                "inputBytes": sum(value[3] for value in before.values() if value[0] == "file"),
                "rustImageId": (root / ".devkit/state/rust-image").read_text().strip(),
                "productInputSha256": digest(self.product_inputs(root)),
                "sourceUnchanged": True}
        atomic_json(self.logs / f"inputs-{shape}-{side}.json", before)
        self.receipt["fixtures"].append(item)
        self.save()
        print(f"Prepared {shape}/{side}: {count} components, {closure} local crates", flush=True)
        return root, before

    def trace(self, case):
        root = Path(case["root"])
        logfile = self.logs / (case["name"] + "-docker.jsonl")
        logfile.write_text("")
        environment = dict(self.environment, PATH=str(self.logger) + os.pathsep + self.environment["PATH"],
                           DEVKIT_PERF_DOCKER_LOG=str(logfile))
        result = self.run(case["name"] + "-docker-trace", case["argv"], cwd=root, env=environment, timeout=600)
        records = [json.loads(line) for line in logfile.read_text().splitlines()]
        counts = {"control": 0, "tool": 0, "other": 0}
        tool_seconds = 0
        for record in records:
            kind = docker_kind(record["argv"])
            if kind:
                require(record["exitCode"] == 0, "Traced container creation failed")
                counts[kind] += 1
                if kind == "tool":
                    tool_seconds += record["seconds"]
        summary_file = self.logs / (case["name"] + "-host-exec.txt")
        self.run(case["name"] + "-host-trace",
                 [*self.strace, "-f", "-c", "-e", "trace=execve,execveat", "-o", summary_file,
                  "--", *case["argv"]], cwd=root, timeout=600)
        summary_text = summary_file.read_text()
        case["trace"] = {
            "dockerCliCalls": len(records), "oneShotContainers": sum(counts.values()),
            "controlContainers": counts["control"], "toolContainers": counts["tool"],
            "otherContainers": counts["other"], "dockerLog": str(logfile),
            "dockerRecords": records,
            "hostExecSummary": str(summary_file),
            "hostExecSummaryText": summary_text, **exec_counts(summary_text),
            "instrumentedTotalSeconds": result["seconds"],
            "instrumentedToolContainerSeconds": tool_seconds,
            "instrumentedOutsideToolSeconds": result["seconds"] - tool_seconds,
        }
        require(counts["control"] == 0, f"{case['side']} path started a metadata control container")
        require(counts["other"] == 0, f"{case['side']} path started an unexpected one-shot container")
        if case["operation"] in ("help", "describe"):
            require(not records and counts["tool"] == 0,
                    f"{case['side']} {case['operation']} called Docker or started a container")
        elif case["operation"] in ("doctor", "status"):
            require(counts["tool"] == 0, f"{case['side']} {case['operation']} started a tool container")
        elif case["operation"] == "build":
            require(counts["tool"] == case["componentCount"],
                    "Build did not execute exactly one Rust tool container per component")

    def run_samples(self, cases, runner):
        if self.is_publication_sampled():
            paired(cases, self.args.samples, self.args.warmups, runner, self.save)
            return
        require(self.args.samples >= 1 and self.args.warmups >= 0,
                "Diagnostic runs require at least 1 sample and non-negative warmups")
        for case in cases:
            require(isinstance(case.get("argv"), list) and case["argv"]
                    and all(isinstance(s, str) and s for s in case["argv"]),
                    "Command must be a nonempty string argv")
            require(Path(case["root"]).is_dir(), "Missing benchmark product")
            case.update(samplesSeconds=[], warmupSeconds=[], sampleOrder=[])
        for index in range(self.args.warmups + self.args.samples):
            for case in cases if index % 2 == 0 else reversed(cases):
                result = runner(case, index)
                require(result["exitCode"] == 0,
                        f"Benchmark failed: {case['name']}: {result['stderr'][-4000:]}")
                case["warmupSeconds" if index < self.args.warmups else "samplesSeconds"].append(result["seconds"])
                case["sampleOrder"].append(index)
            self.save()
        for case in cases:
            case.update(summarize(case["samplesSeconds"]))

    def benchmark(self, shape, roots, snapshots, count):
        operations = ("help", "doctor", "status", "describe", "build") if shape == "one" else ("doctor", "describe", "build")
        for operation in operations:
            cases = []
            for side in ("baseline", "native"):
                side_release = self.baseline_release if side == "baseline" else self.native_release
                argv = ([str(side_release / "devkit"), "--json", "describe", "--root", str(roots[side])]
                        if operation == "describe"
                        else ["make", "--no-print-directory", operation])
                cases.append({"name": f"{shape}-{operation}-{side}", "shape": shape, "side": side,
                              "operation": operation, "componentCount": count, "root": str(roots[side]),
                              "argv": argv})
            self.receipt["measurements"].extend(cases)
            self.run_samples(
                cases,
                lambda case, index: self.run(case["name"] + f"-{index:02d}", case["argv"],
                                             cwd=case["root"], timeout=600),
            )
            for case in cases:
                self.trace(case)
                require(snapshot(roots[case["side"]]) == snapshots[case["side"]],
                        f"Read-only sample changed product inputs: {case['name']}")
                case["inputsBytesAndModesUnchanged"] = True
                print(json.dumps({"name": case["name"], "median": case["medianSeconds"],
                                  "p95": case["p95Seconds"], "trace": case["trace"]}), flush=True)
            baseline_median = cases[0]["medianSeconds"]
            native_median = cases[1]["medianSeconds"]
            ratio = native_median / baseline_median if baseline_median > 0 else float("inf")
            if operation == "help":
                allowed_max = HELP_MAX_MEDIAN_SECONDS
                passed = native_median <= allowed_max
                self.receipt["comparisons"].append({
                    "shape": shape, "operation": operation,
                    "baselineMedianSeconds": baseline_median,
                    "nativeMedianSeconds": native_median,
                    "nativeToBaselineMedianRatio": ratio,
                    "allowedMaxSeconds": allowed_max,
                    "nonRegressionPassed": passed,
                })
                require(passed, f"Native help median {native_median:.4f}s exceeded ceiling {allowed_max:.3f}s")
            else:
                allowed_max = baseline_median * RELATIVE_FACTOR + JITTER_MARGIN_SECONDS
                passed = native_median <= allowed_max
                self.receipt["comparisons"].append({
                    "shape": shape, "operation": operation,
                    "baselineMedianSeconds": baseline_median,
                    "nativeMedianSeconds": native_median,
                    "nativeToBaselineMedianRatio": ratio,
                    "allowedMaxSeconds": allowed_max,
                    "nonRegressionPassed": passed,
                })
                require(
                    passed,
                    f"Native median regressed on matched {shape}/{operation}: "
                    f"native={native_median:.4f}s > allowed={allowed_max:.4f}s "
                    f"(baseline={baseline_median:.4f}s, ratio={ratio:.3f})",
                )
            self.save()

    def admission(self, root, before):
        lock = root / "apps/app001/Cargo.lock"
        original = lock.read_bytes()
        try:
            lock.write_text('version = 4\n\n[[package]]\nname = "wrong"\nversion = "0.0.0"\n')
            changed = snapshot(root)
            for operation, argv in (
                    ("describe", [self.native_release / "devkit", "--json", "describe", "--root", root]),
                    ("build", ["make", "--no-print-directory", "build"])):
                result = self.run("stale-lock-" + operation, argv, cwd=root, expected=None)
                require(snapshot(root) == changed, "Rejected stale lock sample mutated source")
                self.receipt["admissionChecks"].append({"case": "stale-local-closure-lock-" + operation,
                                                       "exitCode": result["exitCode"], "sourceUnchanged": True})
        finally:
            lock.write_bytes(original)
        require(snapshot(root) == before, "Admission probe did not restore product inputs")
        self.make(root, "doctor", label="admission-restored-doctor")

    def execute(self):
        require(platform.system() == "Linux" and platform.machine() == "x86_64", "Linux x86_64 required")
        require(not self.scratch.is_relative_to(REPO), "Products must live outside the author checkout")
        index, index_sha = validate_build_set(self.args.build_set)
        self.receipt.update({key: index[key] for key in ("version", "sourceCommit", "sourceTree", "buildRun", "releaseIdentity")})
        self.receipt.update(indexSha256=index_sha, buildSet=str(self.args.build_set))
        self.native_release = extract_release(self.args.build_set / index["archive"], self.scratch / "native release")
        require(sha256(self.native_release / "release.json") == index["releaseIdentity"]
                and sha256(self.native_release / "devkit") == index["files"][index["engine"]]["sha256"],
                "Archive payload is not the indexed engine/release")
        info = json.loads(self.run("engine-build-info", [self.native_release / "devkit", "internal", "build-info"])["stdout"])
        require(all(info[key] == index[key] for key in ("version", "sourceCommit", "sourceTree", "target")),
                "Embedded binary source identity differs from exact supplied index")
        self.receipt["engineBuildInfo"] = info
        self.baseline_release = self.baseline()
        self.tracing()
        for shape, count, closure in SHAPES:
            roots, snapshots = {}, {}
            for side, release in (("baseline", self.baseline_release), ("native", self.native_release)):
                roots[side], snapshots[side] = self.create(release, side, shape, count, closure)
            require(self.product_inputs(roots["baseline"]) == self.product_inputs(roots["native"]),
                    f"Paired product source/locks/config/modes differ: {shape}")
            fixture_difference = sorted(name for name in set(snapshots["baseline"]) | set(snapshots["native"])
                                        if snapshots["baseline"].get(name) != snapshots["native"].get(name))
            self.receipt.setdefault("fixtureDifferences", {})[shape] = {
                "identicalProductSourceAndNativeLocks": True,
                "releaseManagedAndRootFilesDifferent": fixture_difference,
                "reason": "Actual public 0.4.1 and indexed native factories intentionally ship different controller/runtime/version files."}
            self.benchmark(shape, roots, snapshots, count)
            if closure:
                self.admission(roots["native"], snapshots["native"])
        # Detect mutation of candidate artifacts while this longer gate ran.
        _, after_index_sha = validate_build_set(self.args.build_set)
        require(after_index_sha == index_sha, "Build index changed during performance gate")
        self.receipt["host"]["loadAverageAtEnd"] = os.getloadavg()
        self.receipt["host"]["meminfoAtEnd"] = self.meminfo()
        self.receipt["success"] = self.is_publication_sampled()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-set", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-release", type=Path, help="Exact public 0.4.1 extracted directory or archive")
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--warmups", type=int, default=2)
    args = parser.parse_args()
    if args.samples < 1 or args.warmups < 0:
        parser.error("Samples must be >= 1 and warmups must be >= 0")
    args.build_set = args.build_set.resolve(strict=True)
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    if args.baseline_release:
        args.baseline_release = args.baseline_release.resolve(strict=True)
    gate = Gate(args)
    started = time.monotonic()
    print(f"Performance scratch: {gate.scratch}", flush=True)
    try:
        gate.execute()
    except Exception as error:
        gate.receipt["error"] = str(error)
        print(f"Performance gate failed: {error}", file=sys.stderr, flush=True)
        return 1
    finally:
        gate.receipt["durationSeconds"] = time.monotonic() - started
        gate.save()
    if not gate.receipt["success"]:
        print(
            f"Performance diagnostic completed (success=false for samples={args.samples} < 30 or warmups={args.warmups} < 2): {args.output}",
            file=sys.stderr,
            flush=True,
        )
        return 1
    print(f"Performance gate passed: {args.output}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
