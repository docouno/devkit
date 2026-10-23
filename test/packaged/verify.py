#!/usr/bin/env python3
"""Black-box verification of independent, unpublished Devkit release consumers.

Only Python's standard library is used on the host. All language tooling runs in
the release's containers. The disposable release and consumers live outside the
author checkout; logs and a structured receipt remain in .artifacts/packaged.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


REPO = Path(__file__).resolve().parents[2]
OWNERSHIP = "io.docouno.devkit.packaged-test"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def verify_creation_hint(output):
    hints = [line for line in output.splitlines()
             if re.search(r"\bmake\b", line) and re.search(r"\bsetup\b", line)]
    require(hints, "Successful init did not explain how to set up the project")
    require(all("/workspace/" not in line for line in hints),
            "Creation instructions expose a container path that is invalid on the host")
    require(any(re.search(r"\bmake\s+setup\b", line) for line in hints),
            "Successful init did not provide a make setup hint")


def source_snapshot(root):
    """Include untracked inputs; ignore only native outputs and operational state."""
    records = {}
    for directory, names, files in os.walk(root, followlinks=False):
        relative = Path(directory).relative_to(root)
        names[:] = sorted(name for name in names if name not in (".git", "node_modules")
                          and not (len(relative.parts) == 2 and relative.parts[0] == "apps"
                                   and name in ("dist", "target"))
                          and not (relative == Path(".devkit") and name in ("state", "cache")))
        for name in sorted(files + [name for name in names if (Path(directory) / name).is_symlink()]):
            path = Path(directory) / name
            value = os.readlink(path) if path.is_symlink() else sha256(path.read_bytes())
            records[str(path.relative_to(root))] = [stat.S_IMODE(path.lstat().st_mode), value]
    return records


@contextmanager
def changed_files(changes):
    originals = {path: path.read_bytes() if path.exists() else None for path in changes}
    try:
        for path, contents in changes.items():
            path.write_text(contents)
        yield
    finally:
        for path, contents in originals.items():
            if contents is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(contents)


class Verification:
    def __init__(self, release, next_release=None):
        self.token = uuid.uuid4().hex
        self.receipt = REPO / ".artifacts/packaged" / self.token
        self.receipt.mkdir(parents=True)
        self.scratch = Path(tempfile.mkdtemp(prefix="devkit-packaged-"))
        self.release = self.copy_release(release, "release")
        self.next_release = self.copy_release(next_release, "next-release") if next_release else None
        self.version = json.loads((self.release / "release.json").read_text())["version"]
        self.results = []
        self.projects = []
        self.containers = []
        self.forbidden_log = self.scratch / "host-sdk-attempts"
        blockers = self.scratch / "without-host-sdks"
        blockers.mkdir()
        for name in ("node", "npm", "npx", "cargo", "rustc", "rustup", "tsc", "eslint"):
            path = blockers / name
            path.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$DEVKIT_FORBIDDEN_HOST_SDK_LOG"\n'
                            'printf "Host language SDK use is forbidden in this test\\n" >&2\nexit 97\n')
            path.chmod(0o755)
        self.environment = dict(os.environ)
        for key in list(self.environment):
            if key.startswith(("COMPOSE_", "DEVKIT_", "npm_", "NPM_")) or key in (
                "MAKEFLAGS", "MFLAGS", "MAKELEVEL", "NODE_OPTIONS", "NODE_PATH", "NODE_TEST_CONTEXT"
            ):
                self.environment.pop(key)
        self.environment["PATH"] = str(blockers) + os.pathsep + os.environ["PATH"]
        self.environment["DEVKIT_FORBIDDEN_HOST_SDK_LOG"] = str(self.forbidden_log)

    def copy_release(self, source, name):
        source = Path(source).resolve()
        require((source / "release.json").is_file(), f"Not a built release: {source}")
        destination = self.scratch / name
        shutil.copytree(source, destination)
        manifest = json.loads((destination / "release.json").read_text())
        actual = source_snapshot(destination)
        expected = manifest["files"]
        require(set(actual) == set(expected) | {"release.json"}, "Unexpected release payload file set")
        for path, identity in expected.items():
            require(actual[path] == [identity["mode"], identity["sha256"]], f"Release mismatch: {path}")
        require((destination / "runtime/LICENSE").read_bytes() == (destination / "LICENSE").read_bytes(),
                "Vendored runtime must retain the MIT notice")
        for path in actual:
            require(Path(path).name not in ("AGENTS.md", "CLAUDE.md"), f"Agent file shipped: {path}")
        return destination

    def run(self, label, argv, *, expected=None, timeout=600, cwd=None, environment=None):
        sequence = len(self.results) + 1
        logfile = self.receipt / f"{sequence:03d}-{label}.log"
        started = time.monotonic()
        process = subprocess.Popen([str(arg) for arg in argv], cwd=cwd, env=environment or self.environment,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                   start_new_session=True)
        try:
            output, _ = process.communicate(timeout=timeout)
        except BaseException as error:
            # Reap the Docker creator before the outer exact-resource cleanup.
            # In particular, Ctrl-C must not leave an isolated child process
            # creating a container after the cleanup pass has already finished.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                output, _ = process.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                output, _ = process.communicate()
            logfile.write_text(output)
            if isinstance(error, subprocess.TimeoutExpired):
                raise RuntimeError(f"Timed out: {label}; see {logfile}") from error
            raise
        logfile.write_text(output)
        duration = round(time.monotonic() - started, 3)
        passed = process.returncode == 0 if expected is None else (
            process.returncode != 0 and expected in output
        )
        self.results.append({"label": label, "argv": [str(arg) for arg in argv],
                             "exitCode": process.returncode, "durationSeconds": duration,
                             "expectedFailure": expected, "passed": passed, "log": logfile.name})
        print(f"{'PASS' if passed else 'FAIL'} {label} ({duration:.1f}s)", flush=True)
        require(passed, f"{label}: exit={process.returncode}; expected={expected!r}; see {logfile}\n{output[-6000:]}")
        return output

    def make(self, root, *targets, label=None, **kwargs):
        return self.run(label or (root.parent.name + "-" + "-".join(targets)),
                        ["make", "--no-print-directory", "-C", root, *targets], **kwargs)

    def inspect(self, kind, name):
        result = subprocess.run(["docker", kind, "inspect", name], env=self.environment,
                                capture_output=True, text=True, timeout=20)
        return json.loads(result.stdout)[0] if result.returncode == 0 else None

    def project_name(self, root):
        return "devkit-" + sha256(str(root.resolve()).encode())[:16]

    def service(self, root, service):
        project = self.project_name(root)
        output = self.run(f"{root.parent.name}-{service}-identity", ["docker", "ps", "--all", "--filter",
                          f"label=com.docker.compose.project={project}", "--filter",
                          f"label=com.docker.compose.service={service}", "--format", "{{.ID}}"])
        identifiers = output.split()
        require(len(identifiers) == 1, f"Expected one {service} container in {project}")
        return identifiers[0]

    def port(self, container, internal):
        found = self.inspect("container", container)
        require(found is not None, f"Container disappeared: {container}")
        bindings = found["NetworkSettings"]["Ports"][f"{internal}/tcp"]
        require(len(bindings) == 1 and bindings[0]["HostIp"] == "127.0.0.1", "Expected one loopback port")
        return int(bindings[0]["HostPort"])

    def http(self, port, path, expected, *, timeout=30):
        deadline = time.monotonic() + timeout
        failure = None
        while time.monotonic() < deadline:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=2) as response:
                    body = response.read().decode()
                    require(response.status == 200, f"Unexpected HTTP status: {response.status}")
                    if isinstance(expected, dict):
                        require(json.loads(body) == expected, f"Unexpected JSON: {body}")
                    else:
                        require(expected in body, f"Expected {expected!r} in HTTP response")
                    return body
            except (OSError, urllib.error.URLError, RuntimeError) as error:
                failure = error
                time.sleep(0.2)
        raise RuntimeError(f"HTTP readiness failed at port {port}, {path}: {failure}")

    def create(self, kind, parent):
        root = self.scratch / "work trees" / parent / "product"
        root.parent.mkdir(parents=True)
        output = self.run(f"{kind}-init", [self.release / "devkit", "init", root, "--template", kind,
                          "--package-source", "local", "--name", f"verification-{kind}"])
        verify_creation_hint(output)
        self.projects.append(root)
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["version"] == self.version and state["packageMode"] == "local", "Wrong accepted release")
        for name, app in state["apps"].items():
            if app["kind"] == "node":
                package = json.loads((root / "apps" / name / "package.json").read_text())
                require(package["devDependencies"]["@docouno/env"].startswith("file:../../.devkit/npm/"),
                        "Local init did not consume the copied package archive")
                (root / "apps" / name / "src/consumer-marker.ts").write_text(
                    "export const consumerMarker = 'untracked source input';\n")
        (root / ".env").write_text("PRODUCT_SECRET=consumer-owned-fixture-value\n")
        return root

    def healthy(self, root, kind):
        before = source_snapshot(root)
        self.make(root, "setup", label=f"{kind}-setup")
        self.make(root, "doctor", label=f"{kind}-doctor")
        self.make(root, "check", "test", "build", label=f"{kind}-check-test-build")
        require(source_snapshot(root) == before, f"{kind}: setup/check/test/build mutated source inputs")
        self.results.append({"label": f"{kind}-source-purity", "passed": True,
                             "inputFileCount": len(before), "snapshot": before})

    def node_negative(self, root):
        source = root / "apps/main/src"
        probe = source / "quality-probe.ts"
        with changed_files({probe: 'export const formattingProbe={value:"bad"}\n'}):
            self.make(root, "check", expected="Code style issues found", label="node-format-rejected")
            bad = probe.read_bytes()
            self.make(root, "fix", label="node-explicit-fix")
            require(probe.read_bytes() != bad, "Explicit fix did not format source")
            self.make(root, "check", label="node-after-fix")
        with changed_files({probe: "export const typeProbe: string = 42;\n"}):
            self.make(root, "check", expected="TS2322", label="node-type-rejected")
        with changed_files({probe: "export const unsafeProbe: string = JSON.parse('\"text\"');\n"}):
            self.make(root, "check", expected="no-unsafe-assignment", label="node-typed-lint-rejected")
        config = root / "apps/main/eslint.config.mjs"
        with changed_files({
            probe: "export const ignoredProbe = 1;\n",
            config: "import { nodeConfig } from '@docouno/env/eslint';\n"
                    "export default [...nodeConfig(import.meta.dirname), { ignores: ['src/quality-probe.ts'] }];\n",
        }):
            self.make(root, "check", expected="Declared source is ignored by ESLint", label="node-ignored-source-rejected")
        tests = list(source.glob("*.test.ts"))
        require(len(tests) > 0, "Node template has no native test files")
        with changed_files({path: "import test from 'node:test';\nawait test.skip('skipped', () => {});\n" for path in tests}):
            self.make(root, "test", expected="No runnable node:test", label="node-skipped-tests-rejected")
        with changed_files({path: "" for path in tests}):
            self.make(root, "test", expected="No runnable node:test", label="node-empty-tests-rejected")

    def react_negative(self, root):
        tests = list((root / "apps/web/src").glob("*.test.*"))
        require(len(tests) > 0, "React template has no native tests")
        with changed_files({path: "import { test } from 'vitest';\ntest.skip('skipped', () => {});\n" for path in tests}):
            self.make(root, "test", expected="No runnable Vitest", label="react-skipped-tests-rejected")

    def rust_negative(self, root):
        source = root / "apps/api/src/lib.rs"
        original = source.read_text()
        with changed_files({source: original + "\npub fn quality_probe() -> i32 {\n    let unused_value = 1;\n    2\n}\n"}):
            self.make(root, "check", expected="unused variable", label="rust-warning-rejected")
        files = [source, root / "apps/api/tests/api.rs"]
        skipped = {}
        for path in files:
            contents = path.read_text().replace("#[test]", "#[test]\n    #[ignore]")
            contents = contents.replace("#[tokio::test]", "#[tokio::test]\n#[ignore]")
            contents = contents.replace('/// ```\n/// assert_eq!', '/// ```ignore\n/// assert_eq!', 1)
            skipped[path] = contents
        with changed_files(skipped):
            self.make(root, "test", expected="no non-ignored native Rust test cases", label="rust-skipped-tests-rejected")

    def emitted(self, root, kind):
        settings = dict(line.split("=", 1) for line in (root / ".devkit/runtime/toolchains.env").read_text().splitlines() if line and not line.startswith("#"))
        name = f"devkit-packaged-{self.token}-{kind}"
        self.containers.append(name)
        image = settings["DEVKIT_NODE_IMAGE"] if kind == "node" else (root / ".devkit/state/rust-image").read_text().strip()
        command = ["node", "/workspace/apps/main/dist/index.js"] if kind == "node" else ["/workspace/apps/api/target/debug/devkit-api"]
        self.run(f"{kind}-emitted-start", ["docker", "run", "--detach", "--pull", "never", "--name", name,
                 "--label", f"{OWNERSHIP}={self.token}", "--user", f"{os.getuid()}:{os.getgid()}",
                 "--read-only", "--publish", "127.0.0.1::8080", "--volume", f"{root}:/workspace:ro,z", image, *command])
        port = self.port(name, 8080)
        self.http(port, "/health", {"status": "ok"})
        self.http(port, "/api/greeting?name=%20Ada%20%26%20Bob%20", {"message": "Hello, Ada & Bob!"})
        if kind == "node":
            # Use the literal origin-form target; URL clients may normalize '//'.
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
            try:
                connection.request("GET", "//")
                response = connection.getresponse()
                body = response.read()
                require(response.status == 400, f"Malformed request was not rejected: {response.status}, {body!r}")
            finally:
                connection.close()
            self.http(port, "/health", {"status": "ok"})
            self.results.append({"label": "node-malformed-request-survival", "passed": True})
        self.run(f"{kind}-emitted-stop", ["docker", "stop", "--time", "5", name], timeout=15)
        found = self.inspect("container", name)
        require(found and found["State"]["ExitCode"] == 0, f"{kind}: emitted app did not shut down cleanly")
        self.run(f"{kind}-emitted-remove", ["docker", "rm", name])

    def mixed_development(self, mixed, node):
        require(mixed.name == node.name and self.project_name(mixed) != self.project_name(node), "Invalid same-name isolation fixture")
        started = time.time()
        self.make(node, "dev", label="node-development-start")
        self.make(mixed, "dev", label="mixed-development-start")
        web = self.service(mixed, "web")
        api = self.service(mixed, "api")
        node_container = self.service(node, "main")
        web_port, api_port, node_port = self.port(web, 5173), self.port(api, 8080), self.port(node_container, 8080)
        require(len({web_port, api_port, node_port}) == 3, "Development stands share a port")
        self.http(web_port, "/", '<div id="root"></div>')
        self.http(web_port, "/src/main.tsx", "apiEnabled")
        self.http(api_port, "/health", {"status": "ok"})
        self.http(web_port, "/api/greeting?name=%E4%B8%96%E7%95%8C", {"message": "Hello, 世界!"})
        self.http(node_port, "/health", {"status": "ok"})
        self.make(mixed, "status", label="mixed-status")
        self.make(mixed, "logs", "tail=10", label="mixed-finite-logs", timeout=30)
        self.make(node, "down", label="node-development-down")
        require(self.inspect("container", node_container) is None, "Down left this checkout's service")
        self.http(web_port, "/api/greeting?name=isolated", {"message": "Hello, isolated!"})
        require(self.inspect("container", api)["State"]["Running"], "Other checkout stopped")
        self.make(mixed, "down", label="mixed-development-down")
        require(self.inspect("container", web) is None and self.inspect("container", api) is None, "Mixed services survived down")
        events = self.run("mixed-rust-stop-events", ["docker", "events", "--since", f"{started:.9f}",
                          "--until", f"{time.time():.9f}", "--filter", f"container={api}",
                          "--filter", "event=die", "--format", "{{json .}}"], timeout=10)
        exits = [json.loads(line)["Actor"]["Attributes"]["exitCode"] for line in events.splitlines()]
        require(len(exits) == 1 and exits[0] != "137", "Rust development service required a forced kill or has unknown stop status")
        self.results.append({"label": "mixed-http-and-same-name-isolation", "passed": True,
                             "ports": {"web": web_port, "rustApi": api_port, "nodeApi": node_port},
                             "rustDevelopmentExitCode": int(exits[0])})

    def upgrade(self, root):
        if self.next_release is None:
            return
        next_version = json.loads((self.next_release / "release.json").read_text())["version"]
        require(next_version != self.version, "Upgrade proof requires a distinct release version")
        package_path = root / "apps/main/package.json"
        package = json.loads(package_path.read_text())
        package["scripts"]["consumer:example"] = "node --version"
        package["description"] = "Consumer-owned metadata survives a toolkit update"
        package_path.write_text(json.dumps(package, indent=2) + "\n")
        source = root / "apps/main/src/consumer-marker.ts"
        source.write_text("export const consumerMarker = 'consumer edit retained across an update';\n")
        preserved = {path: path.read_bytes() for path in (source, root / ".env", root / "Makefile", root / "compose.yaml")}
        old_state = (root / ".devkit/manifest.json").read_bytes()
        runtime = root / ".devkit/runtime/devkit.mk"
        with changed_files({runtime: runtime.read_text() + "# consumer-conflict\n"}):
            before = source_snapshot(root)
            self.run("upgrade-managed-conflict", [self.next_release / "devkit", "update", root],
                     expected="Managed file conflict")
            require(source_snapshot(root) == before, "Conflicting update wrote project files")
        self.run("node-upgrade", [self.next_release / "devkit", "update", root])
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["version"] == next_version and state["packageMode"] == "local", "Update changed package mode or wrong version")
        require((root / ".devkit/manifest.json").read_bytes() != old_state, "Update retained old accepted state")
        for path, contents in preserved.items():
            require(path.read_bytes() == contents, f"Update overwrote {path}")
        current = json.loads(package_path.read_text())
        require(current["scripts"]["consumer:example"] == package["scripts"]["consumer:example"] and
                current["dependencies"] == package["dependencies"] and current["description"] == package["description"],
                "Update changed consumer scripts/dependencies/metadata")
        self.make(root, "check", expected="dependencies are absent/stale", label="upgrade-requires-setup")
        self.healthy(root, "upgraded-node")
        before = source_snapshot(root)
        self.run("upgrade-identical-noop", [self.next_release / "devkit", "update", root])
        require(source_snapshot(root) == before, "Identical release update rewrote source or generation")
        self.run("recover-without-pending", [self.next_release / "devkit", "recover", root])

    def cleanup(self):
        errors = []
        for root in reversed(self.projects):
            project = self.project_name(root)
            result = subprocess.run(["docker", "ps", "--all", "--filter", f"label=com.docker.compose.project={project}", "--format", "{{.ID}}"],
                                    env=self.environment, capture_output=True, text=True, timeout=20)
            if result.returncode:
                errors.append(f"Could not inspect project {project}: {result.stderr}")
                continue
            for identifier in result.stdout.split():
                found = self.inspect("container", identifier)
                if found and found["Config"].get("Labels", {}).get("com.docker.compose.project") == project:
                    removed = subprocess.run(["docker", "rm", "--force", identifier], env=self.environment,
                                             capture_output=True, text=True, timeout=20)
                    if removed.returncode:
                        errors.append(f"Could not remove owned project container {identifier}: {removed.stderr}")
            network = project + "_default"
            found = self.inspect("network", network)
            if found and found.get("Labels", {}).get("com.docker.compose.project") == project:
                removed = subprocess.run(["docker", "network", "rm", network], env=self.environment,
                                         capture_output=True, text=True, timeout=20)
                if removed.returncode:
                    errors.append(f"Could not remove owned network {network}: {removed.stderr}")
        for name in self.containers:
            found = self.inspect("container", name)
            if found:
                if found["Config"].get("Labels", {}).get(OWNERSHIP) != self.token:
                    errors.append(f"Container ownership mismatch: {name}")
                else:
                    result = subprocess.run(["docker", "rm", "--force", name], env=self.environment,
                                            capture_output=True, text=True, timeout=20)
                    if result.returncode:
                        errors.append(f"Could not remove owned container {name}: {result.stderr}")
        require(not errors, "\n".join(errors))

    def verify(self):
        failure = None
        try:
            node = self.create("node", "a-node-Проект")
            self.make(node, "check", expected="dependencies are absent/stale", label="node-setup-required")
            self.healthy(node, "node")
            self.node_negative(node)
            self.emitted(node, "node")
            react = self.create("react", "b-react")
            self.healthy(react, "react")
            self.react_negative(react)
            rust = self.create("rust", "c-rust")
            self.healthy(rust, "rust")
            self.rust_negative(rust)
            self.emitted(rust, "rust")
            mixed = self.create("mixed", "d-mixed")
            # An ordinary Make edge must build the web output before the Rust API.
            with (mixed / "Makefile").open("a") as target:
                target.write("\nbuild-api: build-web\n")
            self.healthy(mixed, "mixed")
            edge = self.make(mixed, "build", "app=api", label="mixed-explicit-build-edge")
            require("vite v" in edge and "Finished" in edge and edge.index("vite v") < edge.index("Finished"),
                    "Explicit Make edge did not build web before the selected Rust API")
            self.mixed_development(mixed, node)
            self.upgrade(node)
            require(not self.forbidden_log.exists(), "A command tried to use a host language SDK")
        except BaseException as error:
            failure = error
        finally:
            try:
                self.cleanup()
            except BaseException as error:
                failure = RuntimeError(f"{failure or ''}\nCleanup: {error}")
            summary = {"schemaVersion": 1, "version": self.version, "token": self.token,
                       "standaloneRelease": str(self.release), "scratch": str(self.scratch),
                       "passed": failure is None, "error": str(failure) if failure else None,
                       "hostLanguageSDKAttempted": self.forbidden_log.exists(),
                       "nextVersionTested": bool(self.next_release), "results": self.results,
                       "limits": ["HTTP smoke does not execute a real browser", "Registry consumption and crash recovery are separate proofs"]}
            (self.receipt / "receipt.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
            print(f"Receipt: {self.receipt / 'receipt.json'}", flush=True)
            if failure is None and os.environ.get("DEVKIT_VERIFY_KEEP") != "1":
                shutil.rmtree(self.scratch)
            else:
                print(f"Preserved scratch: {self.scratch}", flush=True)
        if failure:
            raise failure


def main():
    def interrupted(signum, _frame):
        raise InterruptedError(f"Packaged verification interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("release", type=Path)
    parser.add_argument("--next-release", type=Path,
                        default=os.environ.get("DEVKIT_VERIFY_NEXT_RELEASE"))
    args = parser.parse_args()
    Verification(args.release, args.next_release).verify()


if __name__ == "__main__":
    main()
