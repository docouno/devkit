#!/usr/bin/env python3
"""Black-box verification of independent Devkit release consumers.

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
import sys
import tempfile
import tarfile
import time
import urllib.error
import urllib.request
import uuid


REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "test"))
from release_targets import DARWIN_ARM64, LINUX_ARM64, LINUX_X64, executable_header_matches, host_target, linux_arch, payload_engine, payload_name  # noqa: E402

OWNERSHIP = "io.docouno.devkit.packaged-test"
COMMAND_TIMEOUT_SECONDS = 600


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def native_release(release):
    with payload_engine(release).open("rb") as stream:
        return executable_header_matches(stream.read(64), host_target())


def semver_tuple(value):
    parts = value.split("-", 1)[0].split(".")
    require(len(parts) == 3 and all(part.isdigit() for part in parts), f"Invalid version: {value}")
    return tuple(int(part) for part in parts)


def verify_creation_hint(output):
    hints = [line for line in output.splitlines()
             if re.search(r"\bmake\b", line) and re.search(r"\bsetup\b", line)]
    require(hints, "Successful init did not explain how to set up the project")
    require(all("/workspace/" not in line for line in hints),
            "Creation instructions expose a container path that is invalid on the host")
    require(any(re.search(r"\bmake\s+setup\b", line) for line in hints),
            "Successful init did not provide a make setup hint")


def read_image_id(root, kind):
    receipt = root / ".devkit/state" / f"{kind}-image.json"
    if receipt.is_file():
        return json.loads(receipt.read_text())["imageId"]
    return (root / ".devkit/state" / f"{kind}-image").read_text().strip()


def node_workspace(root):
    # Generated membership is an explicit sequence of canonical component paths.
    text = (root / 'pnpm-workspace.yaml').read_text()
    return set(re.findall(r'^\s*-\s*[\'"]?((?:apps|packages)/[a-z][a-z0-9-]*)[\'"]?\s*$', text, re.M))


def assert_node_workspace(root, paths):
    require(node_workspace(root) == set(paths), 'Root workspace registration mismatch')
    require('workspaces' not in json.loads((root / 'package.json').read_text()), 'npm workspaces survived pnpm migration')
    require((root / 'pnpm-lock.yaml').is_file() and not (root / 'package-lock.json').exists(), 'Wrong pnpm lockfile set')


def source_snapshot(root):
    """Include untracked inputs; ignore only native outputs and operational state."""
    records = {}
    for directory, names, files in os.walk(root, followlinks=False):
        relative = Path(directory).relative_to(root)
        names[:] = sorted(name for name in names if name not in (".git", "node_modules")
                          and not (len(relative.parts) == 2 and relative.parts[0] in ("apps", "packages")
                                   and name in ("dist", "target", "release"))
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
    def __init__(self, release, next_release=None, *, package_source="local",
                 public_bootstrap=None, daemon_arch=None, scratch_parent=None, preview_index_sha256=None):
        self.token = uuid.uuid4().hex
        self.receipt = REPO / ".artifacts/packaged" / self.token
        self.receipt.mkdir(parents=True)
        self.scratch = Path(tempfile.mkdtemp(prefix="devkit-packaged-", dir=scratch_parent))
        self.daemon_arch = daemon_arch or linux_arch()
        self.linux_target = {"amd64": LINUX_X64, "arm64": LINUX_ARM64}[self.daemon_arch]
        self.release = (self.copy_preview(release, preview_index_sha256) if preview_index_sha256 is not None
                        else self.copy_release(release, "release"))
        self.next_release = self.copy_release(next_release, "next-release") if next_release else None
        self.version = json.loads((self.release / "release.json").read_text())["version"]
        self.release_identity = sha256((self.release / "release.json").read_bytes())
        self.next_release_identity = (
            sha256((self.next_release / "release.json").read_bytes()) if self.next_release else None
        )
        require(native_release(self.release) and semver_tuple(self.version) >= (0, 5, 0),
                "The baseline release must be a supported native release (>= 0.5.0)")
        if self.next_release is not None:
            next_version = json.loads((self.next_release / "release.json").read_text())["version"]
            require(self.version != next_version and self.release_identity != self.next_release_identity,
                    "Baseline and target releases must have distinct versions and release identities")
        require(package_source in ("local", "registry"), "Unknown package source")
        self.package_source = package_source
        self.public_bootstrap = None
        if public_bootstrap:
            self.public_bootstrap = self.scratch / "public-bootstrap.sh"
            shutil.copy2(public_bootstrap, self.public_bootstrap)
        self.manager = self.next_release or self.release
        require(native_release(self.manager),
                "The target release must contain the native CLI")
        self.results = []
        self.created_versions = {}
        self.harness_identity = {path.name: sha256(path.read_bytes())
                                 for path in Path(__file__).parent.iterdir()
                                 if path.is_file()}
        self.projects = []
        self.containers = []
        self.forbidden_log = self.scratch / "host-sdk-attempts"
        blockers = self.scratch / "without-host-sdks"
        blockers.mkdir()
        for name in ("node", "npm", "npx", "pnpm", "pnpx", "corepack", "cargo", "rustc", "rustup", "tsc", "eslint"):
            path = blockers / name
            path.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$DEVKIT_FORBIDDEN_HOST_SDK_LOG"\n'
                            'printf "Host language SDK use is forbidden in this test\\n" >&2\nexit 97\n')
            path.chmod(0o755)
        self.environment = dict(os.environ)
        for key in list(self.environment):
            if key.startswith(("COMPOSE_", "DEVKIT_", "npm_", "NPM_", "pnpm_", "PNPM_")) or key in (
                "MAKEFLAGS", "MFLAGS", "MAKELEVEL", "NODE_OPTIONS", "NODE_PATH", "NODE_TEST_CONTEXT"
            ):
                self.environment.pop(key)
        self.environment["PATH"] = str(blockers) + os.pathsep + os.environ["PATH"]
        self.environment["DEVKIT_FORBIDDEN_HOST_SDK_LOG"] = str(self.forbidden_log)
        self.environment["DEVKIT_CACHE_DIR"] = str(self.scratch / "native cache")
        self.environment["CI"] = "true"
        if host_target() != DARWIN_ARM64 and (self.manager / "runtime/pnpm.env").is_file():
            selected = subprocess.check_output([str(payload_engine(self.manager)), 'internal', 'ensure-pnpm', '--pin-file', str(self.manager / 'runtime/pnpm.env')], text=True).strip()
            self.environment['DEVKIT_PNPM_BIN_OVERRIDE'] = selected
        print(f"Receipt directory: {self.receipt}", flush=True)
        print(f"Consumer scratch: {self.scratch}", flush=True)

    def cli(self, release, operation, root, *arguments):
        version = json.loads((release / "release.json").read_text())["version"]
        # A genuine baseline project in offline packaged verification is created
        # directly by its actual released native binary; public_bootstrap exercises
        # cross-version init delegation through the target bootstrap script.
        if operation == "init" and release == self.release and not self.public_bootstrap:
            return [payload_engine(release), "--non-interactive", "init", root,
                    "--to", version, "--release-root", release, *arguments]
        command = (["sh", self.public_bootstrap] if self.public_bootstrap
                   else [payload_engine(self.manager)])
        command += ["--non-interactive", operation, root]
        if operation in ("init", "update"):
            command += ["--to", version]
        if not self.public_bootstrap and operation != "recover":
            command += ["--release-root", release]
        return command + list(arguments)

    def exec_cli(self, root, component, *argv, write=False):
        command = (["sh", self.public_bootstrap] if self.public_bootstrap
                   else [payload_engine(self.manager)])
        command += ["exec", "--root", root, "--component", component]
        if write:
            command.append("--write")
        return command + ["--", *argv]

    def copy_preview(self, build_set, expected_sha256):
        raw = (build_set / "release-index.json").read_bytes()
        require(sha256(raw) == expected_sha256, "Frozen preview index SHA-256 differs")
        index = json.loads(raw)
        targets = (LINUX_X64, LINUX_ARM64, DARWIN_ARM64)
        require(index["publicationEligible"] is False and
                index["engines"] == {target: "devkit-" + target for target in targets},
                "Expected an unpublished three-target preview")
        archive_name = index["archive"]
        require(archive_name == f'devkit-{index["version"]}.tar.gz' and Path(archive_name).name == archive_name,
                "Invalid preview archive name")
        # Artifact downloads do not retain executable modes or the payload directory.
        # Extract the same private archive copy that was verified against the frozen index.
        archive = self.scratch / archive_name
        shutil.copyfile(build_set / archive_name, archive)
        require(archive_name in index["files"], "Preview archive missing from the index")
        for name, identity in index["files"].items():
            require(Path(name).name == name and name not in ("", ".", ".."), "Invalid preview asset name")
            path = archive if name == archive_name else build_set / name
            with path.open("rb") as stream:
                require(os.fstat(stream.fileno()).st_size == identity["size"] and
                        hashlib.file_digest(stream, "sha256").hexdigest() == identity["sha256"],
                        f"Frozen preview asset differs: {name}")
        release = self.copy_release(archive, "release")
        manifest = (release / "release.json").read_bytes()
        require(sha256(manifest) == index["releaseIdentity"] == index["files"]["release.json"]["sha256"],
                "Preview payload differs from its frozen release identity")
        accepted = json.loads(manifest)
        require(accepted["version"] == index["version"], "Preview manifest version differs")
        for target, asset in index["engines"].items():
            member = payload_name(target)
            with (release / member).open("rb") as stream:
                require(executable_header_matches(stream.read(64), target) and
                        accepted["files"][member]["sha256"] == index["files"][asset]["sha256"],
                        f"Preview payload engine differs from {asset}")
        return release

    def copy_release(self, source, name):
        source = Path(source).resolve()
        if source.is_file():
            unpacked = self.scratch / (name + "-archive")
            unpacked.mkdir()
            with tarfile.open(source) as archive:
                for entry in archive.getmembers():
                    require(not entry.name.startswith("/") and ".." not in Path(entry.name).parts, "Unsafe release archive")
                archive.extractall(unpacked, filter="data")
            candidates = [unpacked] if (unpacked / "release.json").is_file() else list(unpacked.glob("*/release.json"))
            require(len(candidates) == 1, "Release archive must contain one release root")
            source = candidates[0] if candidates[0].is_dir() else candidates[0].parent
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
        def check_payload_path(path):
            for part in Path(path).parts:
                require(part != ".worktrees" and part.lower() not in ("agents.md", "claude.md", "gemini.md"),
                        f"Forbidden release payload path: {path}")
        for path in actual:
            check_payload_path(path)
        with tarfile.open(destination / manifest["environment"]["archive"]) as env_archive:
            for entry in env_archive:
                check_payload_path(entry.name)
        return destination

    def run(self, label, argv, *, expected=None, timeout=COMMAND_TIMEOUT_SECONDS, cwd=None, environment=None):
        sequence = len(self.results) + 1
        logfile = self.receipt / f"{sequence:03d}-{label}.log"
        started = time.monotonic()
        process = subprocess.Popen([str(arg) for arg in argv], cwd=cwd, env=environment or self.environment,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
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
        if result.returncode == 0:
            objects = json.loads(result.stdout)
            require(isinstance(objects, list) and len(objects) == 1,
                    f"Unexpected Docker inspect result: {kind} {name}")
            return objects[0]
        escaped = re.escape(str(name))
        absent = (re.search(rf"no such (?:object|container|network|image):\s*{escaped}(?:\s|$)",
                            result.stderr, re.I)
                  or re.search(rf"\b{re.escape(kind)}\s+{escaped}\s+not found\b", result.stderr, re.I))
        if absent:
            return None
        raise RuntimeError(f"Docker inspection failed: {kind} {name}: {result.stderr}")

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

    def create(self, kind, parent, release=None):
        root = self.scratch / "work trees" / parent / "product"
        root.parent.mkdir(parents=True)
        release = release or self.release
        selected_version = json.loads((release / "release.json").read_text())["version"]
        selections = {"node": ["--app", "main=node-api"], "react": ["--app", "web=react-web"],
                      "rust": ["--app", "api=rust-api"], "fullstack-web": ["--preset", "fullstack-web"], "fullstack": ["--preset", "fullstack-desktop"],
                      "tslib": ["--template", "ts-library"], "design-system": ["--preset", "design-system"]}
        arguments = [*selections[kind], "--name", f"verification-{kind}"]
        if not self.public_bootstrap or self.package_source != "registry":
            arguments += ["--package-source", self.package_source]
        output = self.run(f"{kind}-init", self.cli(release, "init", root, *arguments))
        verify_creation_hint(output)
        self.projects.append(root)
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["schemaVersion"] == 2 and state["version"] == selected_version and state["packageMode"] == self.package_source, "Wrong accepted v2 release")
        self.created_versions[kind] = state["version"]
        if self.package_source == "registry":
            require(not (root / ".devkit/npm").exists(), "Registry consumer copied a local environment archive")
        components = state["components"]
        require(all(path.startswith(("apps/", "packages/")) for path in components), "Invalid component paths")
        node_paths = [path for path, component in components.items() if component["toolchain"] == "node"]
        if node_paths:
            assert_node_workspace(root, node_paths)
        for path in node_paths:
            package = json.loads((root / path / "package.json").read_text())
            environment_spec = package["devDependencies"]["@docouno/env"]
            if self.package_source == "local":
                require(environment_spec.startswith("file:../../.devkit/npm/"), "Init did not use the release's local env archive")
            else:
                require(environment_spec == selected_version, "Registry consumer did not pin the published environment version")
            require(not (root / path / "package-lock.json").exists() and not (root / path / "pnpm-lock.yaml").exists(), "Leaf lock survived workspace creation")
            (root / path / "src/consumer-marker.ts").write_text("export const consumerMarker = 'untracked source input';\n")
        if kind == "react":
            require(set(components) == {"apps/web"}, "React selection implicitly created a backend")
        if kind == "fullstack":
            require(set(components) == {"apps/api", "apps/web", "apps/desktop", "packages/client"}, "Wrong preset composition")
            ui = [path for path in root.rglob("ClientApp.tsx") if "node_modules" not in path.parts]
            require(ui == [root / "packages/client/src/ClientApp.tsx"], "Shared preset contains duplicate UI")
            require(not any((root / path / "variants").exists() for path in node_paths), "Template overlays leaked into consumer")
            for path in ("apps/web", "apps/desktop"):
                require(components[path]["dependencies"] == ["packages/client"], "Shared dependency not derived")
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

    def readonly_mounts(self, root, kind):
        if kind == "node":
            probe = root / "apps/main/src/readonly-mount.test.ts"
            code = ("import assert from 'node:assert/strict';\n"
                    "import { closeSync, openSync } from 'node:fs';\n"
                    "import test from 'node:test';\n\n"
                    "test('source and native lock mounts reject writable opens', () => {\n"
                    "  for (const file of [new URL(import.meta.url), new URL('../../../pnpm-lock.yaml', import.meta.url), new URL('../../../pnpm-workspace.yaml', import.meta.url)]) {\n"
                    "    assert.throws(() => { const fd = openSync(file, 'r+'); closeSync(fd); }, { code: 'EROFS' });\n"
                    "  }\n"
                    "});\n")
        else:
            probe = root / "apps/api/tests/readonly_mount.rs"
            code = ('#[test]\nfn source_and_lock_mounts_reject_writable_opens() {\n'
                    '    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));\n'
                    '    for file in [root.join("Cargo.lock"), root.join("tests/readonly_mount.rs")] {\n'
                    '        let error = std::fs::OpenOptions::new().write(true).open(file).err();\n'
                    '        assert_eq!(error.and_then(|error| error.raw_os_error()), Some(30));\n'
                    '    }\n}\n')
        with changed_files({probe: code}):
            before = source_snapshot(root)
            self.make(root, "test", label=f"{kind}-source-lock-readonly")
            require(source_snapshot(root) == before, "Readonly mount probe changed project bytes")

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
            self.make(root, "test", "app=web", expected="No runnable Vitest", label="react-skipped-tests-rejected")

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
        image = settings["DEVKIT_NODE_IMAGE"] if kind == "node" else read_image_id(root, "rust")
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

    def electron_runtime(self, root):
        """Resolve the installed binary from the consuming workspace, never infer hoisting."""
        image = read_image_id(root, "desktop")
        name = f"devkit-packaged-{self.token}-electron-resolve"
        self.containers.append(name)
        output = self.run("consumer-electron-runtime", ["docker", "run", "--rm", "--pull", "never", "--name", name,
                          "--label", f"{OWNERSHIP}={self.token}", "--user", f"{os.getuid()}:{os.getgid()}",
                          "--network", "none", "--read-only", "--volume", f"{root}:/workspace:ro,z",
                          "--workdir", "/workspace/apps/desktop", image, "node", "-p", "require('electron')"]).strip()
        require(output.startswith("/workspace/") and ".." not in Path(output).parts,
                f"Electron resolved outside the consumer: {output}")
        binary = root / output.removeprefix("/workspace/")
        require(binary.is_file() and os.access(binary, os.X_OK), "Prepared Electron binary is absent")
        return binary.parent

    def helper(self, root, mode, network_container, heading, label, *, hmr=None, hmr_marker=None, packaged=None, electron_root=None):
        output = self.receipt / label
        output.mkdir()
        name = f"devkit-packaged-{self.token}-{label}"
        self.containers.append(name)
        runtime_root = electron_root or root
        image = read_image_id(runtime_root, "desktop")
        argv = ["docker", "run", "--detach", "--pull", "never", "--name", name,
                "--label", f"{OWNERSHIP}={self.token}", "--user", f"{os.getuid()}:{os.getgid()}",
                "--network", f"container:{network_container}", "--read-only", "--tmpfs", "/tmp:mode=1777",
                "--env", "HOME=/tmp/devkit-browser-home", "--cap-drop", "ALL",
                "--security-opt", "no-new-privileges:true", "--security-opt", f"seccomp={root}/.devkit/runtime/desktop-seccomp.json",
                "--shm-size", "256m", "--volume", f"{Path(__file__).parent}:/harness:ro,z",
                "--volume", f"{output}:/evidence:z", "--env", f"PROBE_MODE={mode}",
                "--env", f"PROBE_HEADING={heading}", "--env", f"PROBE_NAME={label}"]
        if hmr:
            argv += ["--env", f"PROBE_HMR_HEADING={hmr}"]
        if hmr_marker:
            argv += ["--env", f"PROBE_HMR_MARKER={hmr_marker}"]
        if mode == "packaged-desktop":
            argv += ["--volume", f"{packaged}:/app:ro,z", "--env", "PROBE_API_TARGET=http://127.0.0.1:8080"]
        elif mode != "development-desktop":
            argv += ["--volume", f"{self.electron_runtime(runtime_root)}:/electron:ro,z"]
            if mode == "production-web":
                argv += ["--volume", f"{root}/apps/web/dist:/web:ro,z", "--env", "PROBE_API_TARGET=http://127.0.0.1:8080"]
        argv += [image, "timeout", "65s", "node", "/harness/browser.mjs"]
        self.run(label + "-start", argv)
        return name, output

    def helper_ready(self, helper):
        name, output = helper
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if (output / "ready.json").is_file():
                return json.loads((output / "ready.json").read_text())
            found = self.inspect("container", name)
            if found and not found["State"]["Running"]:
                break
            time.sleep(0.1)
        log = self.run(name[-25:] + "-logs", ["docker", "logs", name])
        raise RuntimeError(f"Browser helper did not become ready: {name}: {log[-4000:]}")

    def helper_finish(self, helper, label):
        name, output = helper
        status = self.run(label + "-wait", ["docker", "wait", name], timeout=70).strip()
        self.run(label + "-logs", ["docker", "logs", name])
        require(status == "0", f"{label} failed with container exit {status}")
        result = json.loads((output / "result.json").read_text())
        require((output / "screenshot.png").is_file(), "Browser did not capture a screenshot")
        self.results.append({"label": label + "-browser-result", "passed": True, "result": result})
        self.run(label + "-remove", ["docker", "rm", name])
        return result

    def library_negative(self, root):
        source = root / "packages/client/src"
        probe = source / "quality-probe.ts"
        with changed_files({probe: "export const invalid: string = 42;\n"}):
            self.make(root, "check", "package=client", expected="TS2322", label="library-type-rejected")
        with changed_files({probe: "export const unsafe: string = JSON.parse('42');\n"}):
            self.make(root, "check", "package=client", expected="no-unsafe-assignment", label="library-typed-lint-rejected")
        tests = list(source.rglob("*.test.*"))
        require(tests, "Shared library has no native tests")
        with changed_files({path: "import { test } from 'vitest';\ntest.skip('skipped', () => {});\n" for path in tests}):
            self.make(root, "test", "package=client", expected="No runnable Vitest", label="library-skipped-tests-rejected")
        with changed_files({path: "" for path in tests}):
            self.make(root, "test", "package=client", expected="No test suite found", label="library-empty-tests-rejected")

    def starter_bind_options(self, root, kind):
        pins = dict(line.split("=", 1) for line in (root / ".devkit/runtime/toolchains.env").read_text().splitlines() if "=" in line)
        image = pins["DEVKIT_NODE_IMAGE"] if kind == "node" else read_image_id(root, "rust")
        command = ["node", "/workspace/apps/main/dist/index.js"] if kind == "node" else ["/workspace/apps/api/target/debug/devkit-api"]
        common = ["--pull", "never", "--label", f"{OWNERSHIP}={self.token}", "--user", f"{os.getuid()}:{os.getgid()}",
                  "--network", "none", "--read-only", "--volume", f"{root}:/workspace:ro,z"]
        name = f"devkit-packaged-{self.token}-{kind}-bind-zero"
        self.containers.append(name)
        self.run(kind + "-ephemeral-bind-start", ["docker", "run", "--detach", "--init", "--name", name, *common, "--env", "HOST=127.0.0.1", "--env", "PORT=0", image, *command])
        logs = ""
        for _ in range(50):
            logs = self.run(kind + "-ephemeral-bind-logs", ["docker", "logs", name])
            if re.search(r"127\.0\.0\.1:([1-9][0-9]*)", logs):
                break
            time.sleep(0.1)
        match = re.search(r"127\.0\.0\.1:([1-9][0-9]*)", logs)
        require(match and 0 < int(match.group(1)) <= 65535, "Starter did not log its actual ephemeral bind")
        self.run(kind + "-ephemeral-bind-stop", ["docker", "stop", "--time", "5", name])
        require(self.inspect("container", name)["State"]["ExitCode"] == 0, "Starter signal shutdown failed")
        self.run(kind + "-ephemeral-bind-remove", ["docker", "rm", name])
        for index, (key, value) in enumerate([("PORT", ""), ("PORT", "65536"), ("PORT", "invalid"), ("HOST", "")]):
            name = f"devkit-packaged-{self.token}-{kind}-invalid-bind-{index}"
            self.containers.append(name)
            self.run(kind + f"-invalid-bind-{index}", ["docker", "run", "--name", name, *common, "--env", key + "=" + value, image, *command], expected=key)
            self.run(kind + f"-invalid-bind-remove-{index}", ["docker", "rm", name])

    def fullstack_web(self):
        root = self.create("fullstack-web", "g-fullstack-web", release=self.manager)
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(set(state["components"]) == {"apps/api", "apps/web"}, "Web preset added desktop/library components")
        self.make(root, "setup", label="fullstack-web-setup")
        self.make(root, "up", "app=web", label="fullstack-web-selected-start")
        api, web = self.service(root, "api"), self.service(root, "web")
        # The first cargo run compiles the API before its HTTP listener opens.
        self.http(self.port(api, 8080), "/health", {"status": "ok"}, timeout=COMMAND_TIMEOUT_SECONDS)
        self.http(self.port(web, 5173), "/api/greeting?name=Connected", {"message": "Hello, Connected!"})
        self.preview_host_wire(root)
        self.make(root, "down", label="fullstack-web-stop")
        before = source_snapshot(root)
        self.run("invalid-api-connection", self.cli(self.manager, "add-app", root, "--name", "dashboard", "--template", "react-web", "--api", "web"), expected="Node or Rust API")
        require(source_snapshot(root) == before and not (root / "apps/dashboard").exists(), "Invalid API add changed product state")
        self.run("connected-api-add", self.cli(self.manager, "add-app", root, "--name", "dashboard", "--template", "react-web", "--api", "api"))
        self.make(root, "setup", label="connected-add-setup")
        self.make(root, "up", "app=dashboard", label="connected-add-selected-start")
        dashboard = self.service(root, "dashboard")
        self.http(self.port(dashboard, 5173), "/api/greeting?name=Dashboard", {"message": "Hello, Dashboard!"})
        self.starter_bind_options(root, "rust")
        self.make(root, "down", "cleanup=1", label="connected-add-cleanup")
        require(not (root / ".devkit/state/ports.json").exists(), "Connected cleanup retained port claims")
        self.results.append({"label": "fullstack-web-and-transactional-api-add", "passed": True})
        node = self.create("node", "h-new-node-bind", release=self.manager)
        self.make(node, "setup", "build", label="new-node-bind-build")
        self.starter_bind_options(node, "node")

    def preview_host_wire(self, root):
        """The native producer's literal FILE must start the released Vite plugin."""
        corpus = json.loads((Path(__file__).parents[1] / "fixtures/preview-hosts.json").read_text())
        rows = []
        for index, (raw, canonical) in enumerate(corpus["urls"]):
            template = raw.replace(":1234", ":{port}")
            self.make(root, "up", "app=web", "DEVKIT_PREVIEW_URL_TEMPLATE=" + template,
                      label=f"preview-canonical-start-{index}")
            web = self.service(root, "web")
            actual = self.inspect("container", web)
            environment = dict(item.split("=", 1) for item in actual["Config"]["Env"] if "=" in item)
            slot = environment["DEVKIT_PREVIEW_HOSTS_FILE"]
            mount = next(item for item in actual["Mounts"] if item["Destination"] == slot)
            data = Path(mount["Source"]).read_bytes()
            require(sha256(data) == environment["DEVKIT_PREVIEW_HOSTS_SHA256"], "Canonical host FILE digest differs")
            require(json.loads(data)["hosts"] == [canonical], "Native host differs from the browser corpus")
            authority = f"[{canonical}]" if ":" in canonical else canonical
            port = self.port(web, 5173)
            for host, expected in ((authority, 200), ("unrelated.preview.example.test", 403)):
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
                try:
                    connection.request("GET", "/", headers={"Host": host})
                    response = connection.getresponse()
                    response.read()
                    require(response.status == expected, "Canonical native FILE-to-Vite host policy differs")
                finally:
                    connection.close()
            rows.append({"template": template, "host": canonical, "passed": True})
        self.results.append({"label": "preview-native-host-canonicalization", "passed": True, "rows": rows})

    def preview_transport(self, root):
        """Consume the actual released plugin in separate server/browser namespaces."""
        output = self.receipt / "preview-transport"
        output.mkdir()
        server = f"devkit-packaged-{self.token}-preview-server"
        browser = f"devkit-packaged-{self.token}-preview-browser"
        self.containers.extend([server, browser])
        pins = dict(line.split("=", 1) for line in (root / ".devkit/runtime/toolchains.env").read_text().splitlines() if "=" in line)
        image = read_image_id(root, "desktop")
        certificate = f"devkit-packaged-{self.token}-preview-certificate"
        self.containers.append(certificate)
        self.run("preview-test-certificate", ["docker", "run", "--rm", "--pull", "never", "--name", certificate,
                 "--label", f"{OWNERSHIP}={self.token}", "--user", f"{os.getuid()}:{os.getgid()}",
                 "--network", "none", "--read-only", "--cpus", "1", "--memory", "128m", "--pids-limit", "64",
                 "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true", "--volume", f"{output}:/evidence:z",
                 read_image_id(root, "rust"), "openssl", "req", "-x509", "-newkey", "rsa:2048",
                 "-nodes", "-days", "1", "-subj", "/CN=preview.test", "-keyout", "/evidence/preview-key.pem", "-out", "/evidence/preview-cert.pem"])
        common = ["--detach", "--pull", "never", "--label", f"{OWNERSHIP}={self.token}",
                  "--user", f"{os.getuid()}:{os.getgid()}", "--read-only", "--tmpfs", "/tmp:rw,exec,mode=1777,size=512m",
                  "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
                  "--cpus", "1", "--memory", "768m", "--pids-limit", "256",
                  "--volume", f"{root}:/workspace:ro,z", "--volume", f"{Path(__file__).parent}:/harness:ro,z",
                  "--volume", f"{output}:/evidence:z", "--env", "HOME=/tmp/devkit-preview-home"]
        self.run("preview-server-start", ["docker", "run", "--name", server, *common, pins["DEVKIT_NODE_IMAGE"], "node", "/harness/preview-server.mjs"])
        deadline = time.monotonic() + 20
        while not (output / "server-ready.json").is_file() and time.monotonic() < deadline:
            time.sleep(0.1)
        if not (output / "server-ready.json").is_file():
            self.run("preview-server-logs", ["docker", "logs", server])
        require((output / "server-ready.json").is_file(), "Preview Vite server did not become ready")
        captured = self.inspect("container", server)
        address = captured["NetworkSettings"]["Networks"]["bridge"]["IPAddress"]
        require(address, "Preview server has no isolated namespace address")
        self.run("preview-browser-start", ["docker", "run", "--name", browser, *common,
                 "--add-host", f"preview.test:{address}", "--shm-size", "256m",
                 "--security-opt", f"seccomp={root}/.devkit/runtime/desktop-seccomp.json",
                 "--volume", f"{self.electron_runtime(root)}:/electron:ro,z",
                 "--env", f"PROBE_PROXY_ADDRESS={address}", image, "timeout", "120s", "node", "/harness/preview-browser.mjs"])
        result = self.run("preview-browser-wait", ["docker", "wait", browser], timeout=130).strip()
        self.run("preview-browser-logs", ["docker", "logs", browser])
        require(result == "0" and (output / "result.json").is_file(), "Preview browser transport failed")
        evidence = json.loads((output / "result.json").read_text())
        require(evidence["passed"] and len(evidence["results"]) == 18, "Incomplete preview browser matrix")
        self.results.append({"label": "preview-first-handshake-and-host-guard", "passed": True, "result": evidence})
        for name in (browser, server):
            self.run("preview-stop-" + name.rsplit("-", 1)[-1], ["docker", "rm", "--force", name])

    def fullstack(self, root, node):
        require(root.name == node.name and self.project_name(root) != self.project_name(node), "Same-name isolation fixture invalid")
        # Product-owned Compose override exposes no inspector on the host; the
        # helper joins only this service's network namespace.
        (root / "compose.yaml").write_text('services:\n  desktop:\n    environment:\n      REMOTE_DEBUGGING_PORT: "9222"\n')
        self.make(node, "dev", label="node-development-start")
        preview_template = "https://{port}-{service}.preview.example.test"
        args = ["dev", "headless=1", "DEVKIT_PREVIEW_URL_TEMPLATE=" + preview_template]
        self.make(root, *args, label="fullstack-development-start")
        web, api, desktop = (self.service(root, name) for name in ("web", "api", "desktop"))
        main = self.service(node, "main")
        self.http(self.port(api, 8080), "/health", {"status": "ok"})
        self.http(self.port(web, 5173), "/api/greeting?name=shared", {"message": "Hello, shared!"})
        actual = self.inspect("container", web)
        env = dict(item.split("=", 1) for item in actual["Config"]["Env"] if "=" in item)
        require(env.get("DEVKIT_PREVIEW_HOSTS_FILE") in ("/run/devkit/preview-hosts.json", "/devkit/preview-hosts.json"), "Managed Vite did not receive FILE carrier")
        require(env.get("DEVKIT_PREVIEW_HOSTS", "") == "", "Managed Vite retained CSV carrier")
        host = f"{self.port(web, 5173)}-web.preview.example.test"
        for authority, expected_status in [(host, 200), ("unrelated.preview.example.test", 403)]:
            connection = http.client.HTTPConnection("127.0.0.1", self.port(web, 5173), timeout=3)
            try:
                connection.request("GET", "/", headers={"Host": authority})
                response = connection.getresponse()
                response.read()
                require(response.status == expected_status, "Managed FILE-to-Vite host policy mismatch")
            finally:
                connection.close()
        self.results.append({"label": "managed-file-to-vite-host-policy", "passed": True, "file": env["DEVKIT_PREVIEW_HOSTS_FILE"]})
        self.http(self.port(main, 8080), "/health", {"status": "ok"})
        initial = "A small app, ready to change."
        marker = "Packaged harness shared UI"
        helpers = [self.helper(root, "development-web", web, initial, "dev-web", hmr=marker),
                   self.helper(root, "development-desktop", desktop, initial, "dev-desktop", hmr=marker)]
        for helper in helpers:
            self.helper_ready(helper)
        source = root / "packages/client/src/ClientApp.tsx"
        original = source.read_text()
        require(initial in original, "Shared UI template heading changed; update the named fixture")
        library_before = {str(path.relative_to(root)): sha256(path.read_bytes()) for path in (root / "packages/client/dist").rglob("*") if path.is_file()}
        with changed_files({source: original.replace(initial, marker)}):
            for helper, label in zip(helpers, ("dev-web", "dev-desktop")):
                self.helper_finish(helper, label)
            library_after = {str(path.relative_to(root)): sha256(path.read_bytes()) for path in (root / "packages/client/dist").rglob("*") if path.is_file()}
            require(library_before == library_after, "Dev HMR unexpectedly rebuilt the library")
            self.results.append({"label": "native-shared-hmr", "passed": True, "libraryDistUnchanged": True})
            self.make(root, "build", label="changed-shared-production-build")
            web_js = "\n".join(path.read_text() for path in (root / "apps/web/dist").rglob("*.js"))
            require(marker in web_js, "Shared change missing from production web bundle")
            self.make(root, "package", "app=desktop", label="changed-shared-desktop-package")
            archive = root / "apps/desktop/release/app-linux.tar.gz"
            require(archive.is_file(), "Missing packaged desktop archive")
            extracted = self.scratch / "extracted-desktop"
            extracted.mkdir()
            with tarfile.open(archive) as tar:
                for entry in tar.getmembers():
                    require(not entry.name.startswith("/") and ".." not in Path(entry.name).parts, "Unsafe archive member")
                tar.extractall(extracted, filter="data")
            with (extracted / "app").open("rb") as executable:
                require(executable_header_matches(executable.read(64), self.linux_target),
                        "Packaged Electron executable has the wrong Linux architecture")
            asar = extracted / "resources/app.asar"
            require(asar.is_file(), "Packaged app has no ASAR payload")
            require(not (extracted / "src").exists(), "Raw application source leaked outside ASAR")
            self.results.append({"label": "desktop-archive", "passed": True, "sha256": sha256(archive.read_bytes()), "bytes": archive.stat().st_size,
                                 "asarSha256": sha256(asar.read_bytes()), "executableType": "ELF"})
            for mode, label in (("production-web", "production-web"), ("packaged-desktop", "packaged-desktop")):
                helper = self.helper(root, mode, api, marker, label, packaged=extracted)
                self.helper_finish(helper, label)
        self.preview_transport(root)
        self.make(root, "status", label="fullstack-status")
        self.make(root, "logs", "tail=10", label="fullstack-finite-logs", timeout=30)
        self.make(node, "down", label="node-development-down")
        require(self.inspect("container", main) is None, "Node service survived down")
        self.http(self.port(api, 8080), "/api/greeting?name=isolated", {"message": "Hello, isolated!"})
        self.make(root, "down", label="fullstack-development-down")
        require(all(self.inspect("container", item) is None for item in (web, api, desktop)), "Fullstack services survived down")
        self.results.append({"label": "same-name-checkout-isolation", "passed": True})

    def add_app(self, root):
        source = root / "apps/main/src/consumer-marker.ts"
        source.write_text("export const consumerMarker = 'preserve edited app during add';\n")
        make = root / "Makefile"
        make.write_text(make.read_text() + "\nconsumer-note:\n\t@printf 'consumer-owned\\n'\n")
        compose = root / "compose.yaml"
        settings = dict(line.split("=", 1) for line in (root / ".devkit/runtime/toolchains.env").read_text().splitlines() if line and not line.startswith("#"))
        compose.write_text(json.dumps({"services": {"manual": {"image": settings["DEVKIT_NODE_IMAGE"], "profiles": ["manual"], "command": ["node", "--version"]}}}, indent=2) + "\n")
        preserved = {path: path.read_bytes() for path in (source, make, compose, root / ".env")}
        self.run("add-react-app", self.cli(self.release, "add-app", root, "--name", "console", "--template", "react-web"))
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["components"]["apps/console"]["toolingProfile"] == "web", "Added app not registered")
        for path, content in preserved.items(): require(path.read_bytes() == content, f"Add-app overwrote {path}")
        before = source_snapshot(root)
        self.run("add-existing-app-refused", self.cli(self.release, "add-app", root, "--name", "console", "--template", "react-web"), expected="already exists")
        require(source_snapshot(root) == before, "Duplicate add-app wrote source")
        if self.public_bootstrap:
            self.run("public-add-package", self.cli(self.release, "add-package", root,
                     "--name", "shared", "--template", "react-client"))
            before = source_snapshot(root)
            self.run("public-duplicate-package-refused", self.cli(self.release, "add-package", root,
                     "--name", "shared", "--template", "react-client"), expected="already exists")
            require(source_snapshot(root) == before, "Duplicate add-package wrote source")
            shutil.copytree(self.release / "templates/rust-api", root / "apps/worker")
            self.run("public-register-existing-rust", self.cli(self.release, "register", root,
                     "--app", "worker", "--profile", "rust"))
            state = json.loads((root / ".devkit/manifest.json").read_text())
            require(state["components"]["packages/shared"]["role"] == "library" and
                    state["components"]["apps/worker"]["toolchain"] == "rust", "Public component operations registered wrong profiles")
            workspace = node_workspace(root)
            require("packages/shared" in workspace and "apps/worker" not in workspace,
                    "Public native workspace membership is wrong")
            for path, content in preserved.items():
                require(path.read_bytes() == content, f"Public component operation overwrote {path}")
        self.healthy(root, "added-react-app")

    def electron_cache_corruption(self, root):
        state = json.loads((root / ".devkit/manifest.json").read_text())
        component = next(path for path, value in state["components"].items()
                         if value["toolingProfile"] == "electron")
        package = root / component / "node_modules/electron"
        version = json.loads((package / "package.json").read_text())["version"]
        cached = (Path(self.environment["DEVKIT_CACHE_DIR"]) / "pnpm-store/electron-dist"
                  / (version + "-linux-" + {"amd64": "x64", "arm64": "arm64"}[self.daemon_arch]) / "dist/icudtl.dat")
        local = package / "dist/icudtl.dat"
        require(cached.is_file() and local.is_file(), "Electron ICU resource is absent before corruption proof")
        require((cached.stat().st_dev, cached.stat().st_ino) ==
                (local.stat().st_dev, local.stat().st_ino), "Electron resource is not shared through hardlinks")
        expected = sha256(cached.read_bytes())
        backup = cached.parent.parent / (".verify-icu-" + uuid.uuid4().hex)
        os.link(cached, backup)
        before = source_snapshot(root)
        try:
            cached.unlink()
            self.make(root, "setup", expected="Cached Electron distribution is incomplete or corrupt",
                      label="electron-cache-corruption-refused")
            receipt = root / ".devkit/state/node-workspace.lock"
            require(not receipt.exists(), "Failed Electron setup retained a success receipt")
        finally:
            if not cached.exists():
                os.link(backup, cached)
            backup.unlink()
        self.make(root, "setup", label="electron-cache-restored-setup")
        require(sha256(cached.read_bytes()) == expected and sha256(local.read_bytes()) == expected,
                "Restored Electron distribution differs from its verified resource")
        require(source_snapshot(root) == before, "Electron cache validation changed product inputs")
        installed = package / "path.txt"
        original = installed.read_bytes()
        try:
            installed.unlink()
            os.mkfifo(installed)
            self.make(root, "setup", expected="Electron metadata must be a regular file", timeout=30,
                      label="electron-fifo-marker-refused")
            require(not (root / ".devkit/state/node-workspace.lock").exists(),
                    "FIFO marker failure retained a success receipt")
        finally:
            installed.unlink()
            installed.write_bytes(original)
        self.make(root, "setup", label="electron-fifo-restored-setup")
        require(source_snapshot(root) == before, "Electron marker validation changed product inputs")

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
            self.run("upgrade-managed-conflict", self.cli(self.next_release, "update", root),
                     expected="Managed file conflict")
            require(source_snapshot(root) == before, "Conflicting update wrote project files")
        self.run("node-upgrade", self.cli(self.next_release, "update", root))
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["version"] == next_version and state["packageMode"] == self.package_source, "Update changed package mode or wrong version")
        require((root / ".devkit/manifest.json").read_bytes() != old_state, "Update retained old accepted state")
        for path, contents in preserved.items():
            require(path.read_bytes() == contents, f"Update overwrote {path}")
        current = json.loads(package_path.read_text())
        require(current["scripts"]["consumer:example"] == package["scripts"]["consumer:example"] and
                current["dependencies"] == package["dependencies"] and current["description"] == package["description"],
                "Update changed consumer scripts/dependencies/metadata")
        assert_node_workspace(root, [path for path,c in state['components'].items() if c['toolchain'] == 'node'])
        self.make(root, "check", expected="dependencies are absent/stale", label="upgrade-requires-setup")
        self.healthy(root, "upgraded-node")
        before = source_snapshot(root)
        self.run("upgrade-identical-noop", self.cli(self.next_release, "update", root))
        require(source_snapshot(root) == before, "Identical release update rewrote source or generation")
        self.run("recover-without-pending", self.cli(self.next_release, "recover", root))

    def recovery(self, root):
        # A synthetic interrupted journal exercises the released recovery command.
        # It does not claim to reproduce the timing of a killed live transaction.
        before = source_snapshot(root)
        stage = ".devkit/state/update-" + str(uuid.uuid4())
        entries = []
        for index, relative in enumerate((".devkit/runtime/devkit.mk", ".devkit/manifest.json")):
            path = root / relative
            old = path.read_bytes()
            new = old + b"\n" if index == 0 else (json.dumps({**json.loads(old), "generation": str(uuid.uuid4())}, indent=2) + "\n").encode()
            mode = stat.S_IMODE(path.stat().st_mode)
            backup, staged = f"{stage}/backup/{index}", f"{stage}/staged/{index}"
            for saved, content in ((backup, old), (staged, new)):
                dest = root / saved
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_bytes(content)
                dest.chmod(mode)
            entries.append({"path": relative, "before": {"sha256": sha256(old), "mode": mode},
                            "after": {"sha256": sha256(new), "mode": mode}, "backup": backup, "staged": staged})
            if index == 0: path.write_bytes(new)
        pending = root / ".devkit/update-pending.json"
        pending.write_text(json.dumps({"schemaVersion": 2, "stage": stage, "files": entries, "createdDirectories": []}, indent=2) + "\n")
        self.make(root, "check", expected="update is incomplete", label="pending-state-rejected")
        runtime = root / entries[0]["path"]
        known_after = runtime.read_bytes()
        runtime.write_bytes(known_after + b"# concurrent unknown edit\n")
        conflict = source_snapshot(root)
        self.run("recovery-conflict", self.cli(self.next_release or self.release, "recover", root), expected="Recovery conflict")
        require(source_snapshot(root) == conflict, "Recovery conflict discarded an edit")
        runtime.write_bytes(known_after)
        self.run("synthetic-journal-recovery", self.cli(self.next_release or self.release, "recover", root))
        require(source_snapshot(root) == before, "Recovery did not restore known bytes")
        self.results.append({"label": "synthetic-interrupted-journal", "passed": True, "liveCrashReproduced": False})

    def upgrade_product(self, root, kind):
        """Exercise real Rust/fullstack transitions as well as the Node case."""
        if self.next_release is None:
            return
        version = json.loads((self.next_release / "release.json").read_text())["version"]
        state_file = root / ".devkit/manifest.json"
        state = json.loads(state_file.read_text())
        require(version != state["version"], "Upgrade proof needs distinct versions")
        state["verificationExtension"] = {"kind": kind, "keep": [True, "opaque"]}
        native_packages = {}
        native_dev_dependencies = {}
        manager_metadata = None
        engine_metadata = None
        root_package = root / "package.json"
        if root_package.is_file() and any(c["toolchain"] == "node" for c in state["components"].values()):
            package = json.loads(root_package.read_text())
            manager_metadata = "pnpm@0.0.0"
            package["packageManager"] = manager_metadata
            pins = dict(line.split("=", 1) for line in (root / ".devkit/runtime/toolchains.env").read_text().splitlines() if "=" in line)
            engine_metadata = {"runtime": {"name": "node", "version": pins["DEVKIT_NODE_VERSION"], "onFail": "download"}}
            package["devEngines"] = engine_metadata
            root_package.write_text(json.dumps(package, indent=2) + "\n")
        preserve = {path: path.read_bytes() for path in (root / ".env", root / "Makefile", root / "compose.yaml")}
        for path, component in state["components"].items():
            component["verificationExtension"] = {"component": path}
            component["origin"]["verificationExtension"] = {"origin": "preserve"}
            if component.get("development"):
                component["development"]["verificationExtension"] = {"development": "preserve"}
            directory = root / path
            vite_config = directory / "vite.config.mjs"
            if vite_config.is_file():
                vite_config.write_text(vite_config.read_text() + "\n// Consumer-owned migration marker.\n")
                preserve[vite_config] = vite_config.read_bytes()
            rust_source = directory / "src/lib.rs"
            if component["toolchain"] == "rust" and rust_source.is_file():
                text = rust_source.read_text()
                marker = 'pub const CONSUMER_MIGRATION_MARKER: &str = "preserve user Rust code";'
                require("CONSUMER_MIGRATION_MARKER" not in text, "Migration marker already exists")
                boundary = text.find("\n#[cfg(test)]")
                boundary = len(text) if boundary < 0 else boundary
                tail = text[boundary:].lstrip()
                rust_source.write_text(text[:boundary].rstrip() + "\n\n" + marker
                                       + ("\n\n" + tail if tail else "\n"))
            for source_dir in (directory / "src", directory / "tests"):
                if source_dir.is_dir():
                    for source in source_dir.rglob("*"):
                        if source.is_file():
                            preserve[source] = source.read_bytes()
            for native in (directory / "Cargo.toml", directory / "Cargo.lock"):
                if native.is_file():
                    preserve[native] = native.read_bytes()
            if component["toolchain"] == "node":
                package_path = directory / "package.json"
                package = json.loads(package_path.read_text())
                package["description"] = f"Preserved {kind} consumer metadata"
                package["scripts"]["consumer:retained"] = "node --version"
                package_path.write_text(json.dumps(package, indent=2) + "\n")
                native_packages[path] = {key: value for key, value in package.items() if key != "devDependencies"}
                native_dev_dependencies[path] = dict(package.get("devDependencies", {}))
        state_file.write_text(json.dumps(state, indent=2) + "\n")
        before = source_snapshot(root)
        self.run(f"{kind}-upgrade-preview", self.cli(self.next_release, "update", root, "--preview"))
        require(source_snapshot(root) == before, f"{kind} preview changed project bytes")
        self.run(f"{kind}-upgrade", self.cli(self.next_release, "update", root))
        after = json.loads(state_file.read_text())
        require(after["version"] == version and after["packageMode"] == self.package_source,
                f"{kind} accepted wrong environment")
        require(after["verificationExtension"] == state["verificationExtension"], "Top-level metadata lost")
        if manager_metadata:
            require(json.loads(root_package.read_text())["packageManager"] == manager_metadata,
                    "Migration changed consumer-owned packageManager metadata")
            require(json.loads(root_package.read_text())["devEngines"] == engine_metadata,
                    "Migration changed consumer-owned runtime metadata")
        for path, old in state["components"].items():
            current = after["components"][path]
            require(current["verificationExtension"] == old["verificationExtension"]
                    and current["origin"]["verificationExtension"] == old["origin"]["verificationExtension"],
                    f"Component/origin metadata lost: {path}")
            if old.get("development"):
                require(current["development"]["verificationExtension"] == old["development"]["verificationExtension"],
                        f"Development metadata lost: {path}")
        for path, content in preserve.items():
            require(path.read_bytes() == content, f"{kind} update overwrote {path.relative_to(root)}")
        node_paths = [path for path,c in after['components'].items() if c['toolchain'] == 'node']
        if node_paths: assert_node_workspace(root,node_paths)
        for path, fields in native_packages.items():
            current = json.loads((root / path / "package.json").read_text())
            require({key: value for key, value in current.items() if key != "devDependencies"} == fields,
                    f"{kind} update changed product manifest fields: {path}")
            old_dependencies = native_dev_dependencies[path]
            old_tools = set(state["components"][path]["toolDependencies"])
            new_tools = set(after["components"][path]["toolDependencies"])
            product_dependencies = {name: value for name, value in old_dependencies.items() if name not in old_tools}
            current_product_dependencies = {
                name: value for name, value in current.get("devDependencies", {}).items()
                if name not in old_tools | new_tools or name in product_dependencies
            }
            require(current_product_dependencies == product_dependencies,
                    f"{kind} update changed consumer-owned devDependencies: {path}")
        self.healthy(root, f"upgraded-{kind}")
        unchanged = source_snapshot(root)
        noop_out = self.run(f"{kind}-upgrade-noop", self.cli(self.next_release, "update", root))
        require("Migration notes" not in noop_out, f"{kind} identical no-op emitted migration notes")
        require(source_snapshot(root) == unchanged, f"{kind} repeated upgrade rewrote source")
        self.results.append({"label": f"{kind}-native-upgrade-preservation", "passed": True,
                             "fromVersion": state["version"], "toVersion": version,
                             "preservedFiles": len(preserve), "unknownMetadataPreserved": True})

    def verify_v04_libraries_and_design_system(self, fullstack):
        # 1. Zero-app ts-library lifecycle: init -> setup -> dev refusal -> check/test/build ->
        #    private package refusal -> "private": false + devkit sync -> npm version 0.2.0 ->
        #    setup -> make package -> verified tarball + package-receipt.json.
        tslib = self.create("tslib", "e-tslib", release=self.manager)
        state = json.loads((tslib / ".devkit/manifest.json").read_text())
        require(set(state["components"]) == {"packages/lib"}, "ts-library init registered wrong components")
        self.healthy(tslib, "tslib")
        self.make(tslib, "dev", expected="Project has no runnable apps", label="tslib-dev-refused")
        self.make(tslib, "down", label="tslib-down-noop")
        self.make(tslib, "status", label="tslib-status-noop")
        self.make(tslib, "package", expected="No packageable components found", label="tslib-private-package-refused")
        self.make(tslib, "package", "package=lib", expected='has "private": true', label="tslib-targeted-private-refused")

        pkg_path = tslib / "packages/lib/package.json"
        pkg = json.loads(pkg_path.read_text())
        pkg["name"] = "@corp/sdk"
        pkg["private"] = False
        pkg_path.write_text(json.dumps(pkg, indent=2) + "\n")
        self.run("tslib-sync", self.cli(self.manager, "sync", tslib))
        self.make(tslib, "setup", label="tslib-setup-after-sync")
        self.run("tslib-npm-version", self.exec_cli(
            tslib, "packages/lib", "pnpm", "version", "0.2.0", "--no-git-tag-version", write=True))
        self.make(tslib, "setup", label="tslib-setup-after-version")
        self.make(tslib, "package", label="tslib-package")

        tarball = tslib / "packages/lib/release/corp-sdk-0.2.0.tgz"
        receipt_path = tslib / "packages/lib/release/package-receipt.json"
        require(tarball.is_file() and receipt_path.is_file(), "Missing packaged ts-library tarball or receipt")
        receipt = json.loads(receipt_path.read_text())
        require(receipt["name"] == "@corp/sdk" and receipt["version"] == "0.2.0"
                and receipt["filename"] == "corp-sdk-0.2.0.tgz", f"Unexpected package receipt: {receipt}")
        with tarfile.open(tarball) as tar:
            members = [entry.name for entry in tar.getmembers()]
            require(any(name.startswith("package/dist/") for name in members), "Library tarball lacks dist/")
            require(not any(name.startswith("package/src/") for name in members), "Library tarball leaked src/")
        self.results.append({"label": "tslib-zero-app-package", "passed": True,
                             "archive": tarball.name, "sha256": sha256(tarball.read_bytes())})

        # 2. Design-system preset: packages/ui + apps/playground, live HMR on packages/ui/src/Button.tsx,
        #    and publishable UI library packaging.
        ds = self.create("design-system", "f-design-system", release=self.manager)
        ds_state = json.loads((ds / ".devkit/manifest.json").read_text())
        require(set(ds_state["components"]) == {"packages/ui", "apps/playground"}, "Wrong design-system components")
        require(ds_state["components"]["apps/playground"]["dependencies"] == ["packages/ui"],
                "Playground did not derive dependency on packages/ui")
        self.healthy(ds, "design-system")

        self.make(ds, "dev", "app=playground", label="design-system-dev-start")
        playground = self.service(ds, "playground")
        self.http(self.port(playground, 5173), "/", "<title>")
        ds_helper = self.helper(
            ds, "design-system-web", playground, "Design System Playground", "dev-design-system",
            hmr_marker="live-ui-hmr", electron_root=fullstack,
        )
        self.helper_ready(ds_helper)
        button_src = ds / "packages/ui/src/Button.tsx"
        original_button = button_src.read_text()
        require("data-variant={variant}" in original_button, "Unexpected Button.tsx template source")
        ui_dist_before = {str(p.relative_to(ds)): sha256(p.read_bytes())
                          for p in (ds / "packages/ui/dist").rglob("*") if p.is_file()}
        with changed_files({button_src: original_button.replace(
                "data-variant={variant}", 'data-variant={variant} data-hmr="live-ui-hmr"')}):
            self.helper_finish(ds_helper, "dev-design-system")
            ui_dist_after = {str(p.relative_to(ds)): sha256(p.read_bytes())
                             for p in (ds / "packages/ui/dist").rglob("*") if p.is_file()}
            require(ui_dist_before == ui_dist_after, "Design-system HMR unexpectedly rebuilt packages/ui/dist")
            self.results.append({"label": "design-system-live-hmr", "passed": True, "uiDistUnchanged": True})
        self.make(ds, "down", label="design-system-dev-down")
        require(self.inspect("container", playground) is None, "Playground service survived down")

        ui_pkg_path = ds / "packages/ui/package.json"
        ui_pkg = json.loads(ui_pkg_path.read_text())
        ui_pkg["private"] = False
        ui_pkg_path.write_text(json.dumps(ui_pkg, indent=2) + "\n")
        self.run("design-system-ui-sync", self.cli(self.manager, "sync", ds))
        self.make(ds, "setup", label="design-system-setup-after-sync")
        self.make(ds, "package", label="design-system-ui-package")
        ui_receipt = json.loads((ds / "packages/ui/release/package-receipt.json").read_text())
        ui_tarball = ds / "packages/ui/release" / ui_receipt["filename"]
        require(ui_tarball.is_file(), "Missing packaged react-ui archive")
        with tarfile.open(ui_tarball) as tar:
            ui_members = [entry.name for entry in tar.getmembers()]
            require(any(name.endswith(".css") and name.startswith("package/dist/") for name in ui_members),
                    "Packaged react-ui archive lacks dist/*.css")
            require(not any(name.startswith("package/src/") for name in ui_members),
                    "Packaged react-ui archive leaked src/")
        self.results.append({"label": "design-system-ui-package", "passed": True,
                             "archive": ui_tarball.name, "sha256": sha256(ui_tarball.read_bytes())})

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

    def cross_host_checkout(self, *, export=None, imported=None):
        if not (export or imported):
            return
        require(not (export and imported), "Select one cross-host checkout operation")
        if export:
            root = self.create("fullstack-web", "cross-host-export", release=self.manager)
            self.run("cross-host-init", ["git", "init", "-b", "main"], cwd=root)
            self.run("cross-host-add", ["git", "add", "."], cwd=root)
            self.run("cross-host-commit", ["git", "-c", "user.name=Fixture", "-c",
                     "user.email=fixture@example.invalid", "commit", "-m", "Create cross-host project"], cwd=root)
        else:
            root = self.scratch / "cross-host-import"
            self.run("cross-host-clone", ["git", "clone", "--branch", "main", imported, root])
            self.projects.append(root)
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["releaseIdentity"] == sha256((self.manager / "release.json").read_bytes()),
                "Cross-host checkout belongs to another frozen release")
        revision = self.run("cross-host-revision", ["git", "rev-parse", "HEAD"], cwd=root).strip()
        # A caller's default build platform cannot select the task's executable.
        opposite = "linux/amd64" if self.daemon_arch == "arm64" else "linux/arm64"
        environment = dict(self.environment, DOCKER_DEFAULT_PLATFORM=opposite)
        helper = self.make(root, "exec", "app=web", "prepare=1", "command=/devkit/devkit internal build-info",
                           label="cross-host-linux-helper", environment=environment)
        info = next(json.loads(line) for line in helper.splitlines() if line.startswith("{"))
        require(info["target"] == self.linux_target and info["version"] == state["version"],
                "Wrong release helper executed in the native Linux container")
        self.healthy(root, "cross-host")
        require(not self.run("cross-host-status", ["git", "status", "--porcelain"], cwd=root).strip(),
                "Platform-specific preparation changed the shared checkout")
        if export:
            destination = Path(export).resolve()
            require(not destination.exists(), "Cross-host bundle already exists")
            destination.parent.mkdir(parents=True, exist_ok=True)
            self.run("cross-host-bundle", ["git", "bundle", "create", destination, "refs/heads/main"], cwd=root)
        self.results.append({"label": "cross-host-checkout", "commit": revision, "target": host_target(),
                             "helper": info, "releaseIdentity": state["releaseIdentity"], "passed": True})
        return root

    def verify(self):
        failure = None
        try:
            if self.public_bootstrap:
                absent = self.scratch / "absent public release"
                command = ["sh", self.public_bootstrap, "--non-interactive", "init", absent,
                           "--to", "99.99.99", "--app", "main=node-api"]
                self.run("public-missing-release-refused", command, expected="404")
                require(not absent.exists(), "Missing public release created a target")
            node = self.create("node", "a-node-Проект")
            self.make(node, "check", expected="dependencies are absent/stale", label="node-setup-required")
            self.healthy(node, "node")
            self.readonly_mounts(node, "node")
            self.node_negative(node)
            self.emitted(node, "node")
            react = self.create("react", "b-react")
            self.healthy(react, "react")
            self.react_negative(react)
            rust = self.create("rust", "c-rust")
            self.healthy(rust, "rust")
            self.readonly_mounts(rust, "rust")
            self.rust_negative(rust)
            self.emitted(rust, "rust")
            self.upgrade_product(rust, "rust")
            fullstack = self.create("fullstack", "d-fullstack")
            self.healthy(fullstack, "fullstack")
            self.library_negative(fullstack)
            self.upgrade_product(fullstack, "fullstack")
            self.fullstack(fullstack, node)
            self.verify_v04_libraries_and_design_system(fullstack)
            self.add_app(node)
            self.upgrade(node)
            self.recovery(node)
            self.fullstack_web()
            self.electron_cache_corruption(fullstack)
            self.cross_host_checkout(export=os.environ.get("DEVKIT_VERIFY_EXPORT_CHECKOUT"),
                                     imported=os.environ.get("DEVKIT_VERIFY_IMPORT_CHECKOUT"))
            require(not self.forbidden_log.exists(), "A command tried to use a host language SDK")
        except BaseException as error:
            failure = error
        finally:
            try:
                self.cleanup()
            except BaseException as error:
                failure = RuntimeError(f"{failure or ''}\nCleanup: {error}")
            summary = {"schemaVersion": 2, "version": self.version, "token": self.token,
                       "standaloneRelease": str(self.release), "scratch": str(self.scratch),
                       "releaseIdentity": sha256((self.release / "release.json").read_bytes()),
                       "nextReleaseIdentity": sha256((self.next_release / "release.json").read_bytes()) if self.next_release else None,
                       "packageSource": self.package_source,
                       "createdVersions": self.created_versions,
                       "publicBootstrapSha256": sha256(self.public_bootstrap.read_bytes()) if self.public_bootstrap else None,
                       "nativeManagerVersion": json.loads((self.manager / "release.json").read_text())["version"],
                       "nativeManagerSha256": sha256((payload_engine(self.manager)).read_bytes()),
                       "nativeCacheIsolated": True,
                       "harnessIdentity": self.harness_identity,
                       "passed": failure is None, "error": str(failure) if failure else None,
                       "hostLanguageSDKAttempted": self.forbidden_log.exists(),
                       "nextVersionTested": bool(self.next_release),
                       "baselineUsesGenuineReleasedFactory": bool(
                           self.next_release and native_release(self.release)
                           and self.release_identity != self.next_release_identity
                       ),
                       "results": self.results,
                       "limits": (["Transport archive-failure cases are a separate suite"] if self.public_bootstrap else ["Registry and remote bootstrap are separate suites"]) +
                                 ["Recovery journal is synthetic; live crash timing is covered by core tests"]}
            (self.receipt / "receipt.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
            print(f"Receipt: {self.receipt / 'receipt.json'}", flush=True)
            if failure is None and os.environ.get("DEVKIT_VERIFY_KEEP") != "1":
                shutil.rmtree(self.scratch)
            else:
                print(f"Preserved scratch: {self.scratch}", flush=True)
        if failure:
            raise failure


    def export_cross_host_only(self, destination):
        failure = None
        try:
            self.cross_host_checkout(export=destination)
            require(not self.forbidden_log.exists(), "A command tried to use a host language SDK")
        except BaseException as error:
            failure = error
        finally:
            try:
                self.cleanup()
            except BaseException as error:
                failure = RuntimeError(f"{failure or ''}\nCleanup: {error}")
            if failure is None and os.environ.get("DEVKIT_VERIFY_KEEP") != "1":
                shutil.rmtree(self.scratch)
            else:
                print(f"Preserved scratch: {self.scratch}", flush=True)
        if failure:
            raise failure


def main():
    def interrupted(signum, _frame):
        # selectors retries InterruptedError as EINTR; cancellation must unwind.
        raise KeyboardInterrupt(f"Packaged verification interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("release", type=Path)
    parser.add_argument("--next-release", type=Path,
                        default=os.environ.get("DEVKIT_VERIFY_NEXT_RELEASE"))
    parser.add_argument("--package-source", choices=("local", "registry"), default="local")
    parser.add_argument("--public-bootstrap", type=Path,
                        help="Run lifecycle operations through this downloaded public bootstrap")
    parser.add_argument("--export-cross-host-only", type=Path,
                        help="Only create, verify, and export the cross-host checkout bundle")
    args = parser.parse_args()
    verifier = Verification(args.release, args.next_release, package_source=args.package_source,
                            public_bootstrap=args.public_bootstrap)
    if args.export_cross_host_only:
        verifier.export_cross_host_only(args.export_cross_host_only)
    else:
        verifier.verify()


if __name__ == "__main__":
    main()
