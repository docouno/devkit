#!/usr/bin/env python3
"""Gate a frozen Linux release with paired public-0.2.1/native warm benchmarks.

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
                     paired, require, sha256, snapshot)


REPO = Path(__file__).resolve().parents[2]
BASELINE_VERSION = "0.2.1"
BASELINE_MANIFEST_SHA = "7f9dea1455027e0a0637077c7b040dbd7420a7fbc554d83d4d80ca47e9e849e0"
BASELINE_ARCHIVE_SHA = "882abf32da67bf3c6a99f0a3333413b2b0023d8e830c2792564875853f97a8fb"
BASELINE_URL = "https://github.com/docouno/devkit/releases/download/v0.2.1/devkit-0.2.1.tar.gz"
LARGE_CRATES = 80
SHAPES = (("one", 1, 0), ("five", 5, 0), ("twenty", 20, 0), ("large-closure", 1, LARGE_CRATES))


def atomic_json(path, value):
    temporary = path.with_name(path.name + ".writing")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def clean_environment(cache):
    environment = dict(os.environ)
    for key in list(environment):
        if key.startswith(("COMPOSE_", "DEVKIT_", "npm_", "NPM_")) or key in (
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
                "containers": "Separate Docker PATH logger run: Docker CLI invocations and successful one-shot run creates classified by control or Rust tool. No container-internal process census.",
                "tooling": "Separate Docker trace records wall time of the real Rust tool container invocations including their mandatory Rust gate. Its instrumented time is excluded from plain latency; residual is a diagnostic, not a compiler-speed claim.",
                "cold": "Not measured here. This receipt covers installed warm execution. Actual cold public npx/curl transport belongs to the public verification receipt and is required separately before promotion.",
                "inputs": "Every source/config/lock and untracked file, directories/modes/link targets. Excludes only Git, .devkit state/cache and registered component native outputs/dependency directories.",
                "fixture": "Both real release factories create matched Rust products. Product code is replaced with identical valid std-only Rust and actual Cargo-generated locks to expose controller costs. Large closure is an identical chain of 80 local crates on both sides.",
            },
            "limits": {
                "minimumSamples": 30, "warmups": args.warmups, "p95": "nearest-rank ceil(0.95*n)",
                "requireFasterPairedMedian": ["doctor", "build", "status", "large-closure:describe-vs-doctor"],
                "requireNativeControlContainers": 0, "requireNativeDescribeDockerCalls": 0,
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

    def save(self):
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
            source = self.scratch / "devkit-0.2.1.tar.gz"
            with urllib.request.urlopen(BASELINE_URL, timeout=60) as response, source.open("wb") as output:
                require(response.status == 200, "Public baseline download failed")
                total = 0
                while chunk := response.read(1024 * 1024):
                    total += len(chunk)
                    require(total <= 32 * 1024**2, "Public baseline exceeds budget")
                    output.write(chunk)
        if source.is_file():
            require(sha256(source) == BASELINE_ARCHIVE_SHA, "Public 0.2.1 archive identity mismatch")
            release = extract_release(source, self.scratch / "baseline release")
        else:
            verify_payload(source)
            release = self.scratch / "baseline release"
            shutil.copytree(source, release)
        require(sha256(release / "release.json") == BASELINE_MANIFEST_SHA,
                "Baseline must be the exact immutable public 0.2.1 payload")
        self.receipt["baseline"] = {"version": BASELINE_VERSION, "source": str(source),
                                    "publicArchiveUrl": BASELINE_URL,
                                    "archiveSha256": BASELINE_ARCHIVE_SHA,
                                    "releaseIdentity": BASELINE_MANIFEST_SHA}
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
            docker, "run", "--rm", "--pull", "never", "--network", "none", "--memory", "256m",
            "--cpus", "1", self.sdk, "sh", "-c", "rustc --version; cargo --version; rustfmt --version; cargo clippy --version"])["stdout"].splitlines()
        # Extract the trusted image's strace and its actual dynamic dependencies.
        # Explicit loader arguments affect only strace, never the traced child env.
        ldd = self.run("trace-ldd", [docker, "run", "--rm", "--pull", "never", "--network", "none",
                                    "--memory", "256m", "--cpus", "1", "--entrypoint", "/usr/bin/ldd",
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
            shutil.rmtree(app / "src")
            shutil.rmtree(app / "tests")
            (app / "src").mkdir()
            dependencies = ""
            if closure:
                for index in range(closure):
                    crate = app / "vendor" / f"dep{index:03}"
                    (crate / "src").mkdir(parents=True)
                    declaration = f'dep{index-1:03} = {{ path = "../dep{index-1:03}" }}\n' if index else ""
                    (crate / "Cargo.toml").write_text(
                        f'[package]\nname = "dep{index:03}"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n{declaration}')
                    expression = f"dep{index-1:03}::value() + 1" if index else "1"
                    (crate / "src/lib.rs").write_text(f"pub fn value() -> usize {{\n    {expression}\n}}\n")
                dependencies = f'dep{closure-1:03} = {{ path = "vendor/dep{closure-1:03}" }}\n'
            expression = f"dep{closure-1:03}::value()" if closure else "1"
            (app / "Cargo.toml").write_text(
                f'[package]\nname = "bench-app{number:03}"\nversion = "0.1.0"\nedition = "2024"\nrust-version = "1.99.1"\npublish = false\n\n[dependencies]\n{dependencies}')
            (app / "src/lib.rs").write_text(
                f"pub fn value() -> usize {{\n    {expression}\n}}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn value_is_expected() {{\n        assert_eq!(super::value(), {closure or 1});\n    }}\n}}\n")
            (app / "Cargo.lock").unlink()

    def product_inputs(self, root):
        return {name: value for name, value in snapshot(root).items() if name.startswith("apps/")}

    def make(self, root, target, *, label=None, expected=0):
        return self.run(label or target, ["make", "--no-print-directory", target], cwd=root,
                        expected=expected, timeout=600)

    def create(self, release, side, shape, count, closure):
        root = self.scratch / "products with spaces" / shape / side / "product"
        root.parent.mkdir(parents=True)
        argv = [release / "devkit"]
        if side == "native":
            argv += ["--non-interactive"]
        argv += ["init", root, "--name", "devkit-performance", "--package-source", "local"]
        for number in range(1, count + 1):
            argv += ["--app", f"app{number:03}=rust-api"]
        if side == "native":
            argv += ["--to", self.receipt["version"], "--release-root", release]
        self.run(f"init-{shape}-{side}", argv, timeout=600)
        self.edit_product(root, count, closure)
        # Cargo itself resolves every lock. No fabricated accepted state or lock.
        script = "set -eu\n" + "\n".join(
            f"cd /workspace/apps/app{number:03}; cargo generate-lockfile --offline"
            for number in range(1, count + 1))
        self.run(f"resolve-{shape}-{side}", [
            self.docker, "run", "--rm", "--pull", "never", "--network", "none", "--cpus", "2",
            "--memory", "2g", "--pids-limit", "256", "--user", f"{os.getuid()}:{os.getgid()}",
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
        case["trace"] = {
            "dockerCliCalls": len(records), "oneShotContainers": sum(counts.values()),
            "controlContainers": counts["control"], "toolContainers": counts["tool"],
            "otherContainers": counts["other"], "dockerLog": str(logfile),
            "hostExecSummary": str(summary_file), **exec_counts(summary_file.read_text()),
            "instrumentedTotalSeconds": result["seconds"],
            "instrumentedToolContainerSeconds": tool_seconds,
            "instrumentedOutsideToolSeconds": result["seconds"] - tool_seconds,
        }
        if case["side"] == "native":
            require(counts["control"] == 0, "Native path started a metadata control container")
            require(counts["other"] == 0, "Native path started an unexpected one-shot container")
            if case["operation"] != "build":
                require(sum(counts.values()) == 0, "Native metadata/service path started a one-shot container")
            if case["operation"] == "describe":
                require(not records, "Native describe called Docker")
        elif case["operation"] != "help":
            require(counts["control"] > 0, "Baseline trace failed to observe its real public metadata controller")
        if case["operation"] == "build":
            require(counts["tool"] == case["componentCount"] and counts["other"] == 0,
                    "Build did not execute exactly one Rust tool container per component")

    def benchmark(self, shape, roots, snapshots, count):
        operations = ["doctor", "build"]
        if shape == "one":
            operations = ["help", "doctor", "status", "build"]
        for operation in operations:
            cases = []
            for side in ("baseline", "native"):
                cases.append({"name": f"{shape}-{operation}-{side}", "shape": shape, "side": side,
                              "operation": operation, "componentCount": count, "root": str(roots[side]),
                              "argv": ["make", "--no-print-directory", operation]})
            self.receipt["measurements"].extend(cases)
            paired(cases, self.args.samples, self.args.warmups,
                   lambda case, index: self.run(case["name"] + f"-{index:02d}", case["argv"],
                                                cwd=case["root"], timeout=600), self.save)
            for case in cases:
                self.trace(case)
                require(snapshot(roots[case["side"]]) == snapshots[case["side"]],
                        f"Read-only sample changed product inputs: {case['name']}")
                case["inputsBytesAndModesUnchanged"] = True
                print(json.dumps({"name": case["name"], "median": case["medianSeconds"],
                                  "p95": case["p95Seconds"], "trace": case["trace"]}), flush=True)
            ratio = cases[1]["medianSeconds"] / cases[0]["medianSeconds"]
            self.receipt["comparisons"].append({"shape": shape, "operation": operation,
                                                "nativeToBaselineMedianRatio": ratio,
                                                "nativeFaster": ratio < 1})
            # Help has no admission work and an added verified ELF loader; no
            # unsupported claim of being faster than Bash printing a constant.
            if operation != "help":
                require(ratio < 1, f"Native median regressed on matched {shape}/{operation}: ratio {ratio:.3f}")
            self.save()
        native = {"name": f"{shape}-describe-native", "shape": shape, "side": "native", "operation": "describe",
                  "componentCount": count, "root": str(roots["native"]),
                  "argv": [str(self.native_release / "devkit"), "--json", "describe", "--root", str(roots["native"])]}
        self.receipt["measurements"].append(native)
        # Describe has no old public CLI equivalent. Compare separately with
        # baseline doctor, and retain the explicit semantic difference.
        paired([native], self.args.samples, self.args.warmups,
               lambda case, index: self.run(case["name"] + f"-{index:02d}", case["argv"], cwd=case["root"]), self.save)
        self.trace(native)
        require(snapshot(roots["native"]) == snapshots["native"], "Describe changed product inputs")
        native["inputsBytesAndModesUnchanged"] = True
        baseline = next(case for case in self.receipt["measurements"]
                        if case["shape"] == shape and case["side"] == "baseline" and case["operation"] == "doctor")
        ratio = native["medianSeconds"] / baseline["medianSeconds"]
        self.receipt["comparisons"].append({"shape": shape, "operation": "describe-vs-doctor",
                                           "scopeDifference": "Native describe admits the full graph without SDK readiness; old doctor includes Docker and image readiness.",
                                           "nativeToBaselineMedianRatio": ratio, "nativeFaster": ratio < 1})
        require(ratio < 1, "Native metadata admission failed to beat baseline doctor")
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
        baseline = self.baseline()
        self.tracing()
        for shape, count, closure in SHAPES:
            roots, snapshots = {}, {}
            for side, release in (("baseline", baseline), ("native", self.native_release)):
                roots[side], snapshots[side] = self.create(release, side, shape, count, closure)
            require(self.product_inputs(roots["baseline"]) == self.product_inputs(roots["native"]),
                    f"Paired product source/locks/config/modes differ: {shape}")
            fixture_difference = sorted(name for name in set(snapshots["baseline"]) | set(snapshots["native"])
                                        if snapshots["baseline"].get(name) != snapshots["native"].get(name))
            self.receipt.setdefault("fixtureDifferences", {})[shape] = {
                "identicalProductSourceAndNativeLocks": True,
                "releaseManagedAndRootFilesDifferent": fixture_difference,
                "reason": "Actual public 0.2.1 and indexed native factories intentionally ship different controller/runtime/version files."}
            self.benchmark(shape, roots, snapshots, count)
            if closure:
                self.admission(roots["native"], snapshots["native"])
        # Detect mutation of candidate artifacts while this longer gate ran.
        _, after_index_sha = validate_build_set(self.args.build_set)
        require(after_index_sha == index_sha, "Build index changed during performance gate")
        self.receipt["host"]["loadAverageAtEnd"] = os.getloadavg()
        self.receipt["host"]["meminfoAtEnd"] = self.meminfo()
        self.receipt["success"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-set", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-release", type=Path, help="Exact public 0.2.1 extracted directory or archive")
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--warmups", type=int, default=2)
    args = parser.parse_args()
    if args.samples < 30 or args.warmups < 2:
        parser.error("Release gate requires at least 30 samples and two explicit warmups")
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
    print(f"Performance gate passed: {args.output}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
