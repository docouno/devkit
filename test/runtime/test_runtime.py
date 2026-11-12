#!/usr/bin/env python3
"""Production entrypoint tests. Only exact, labelled disposable Docker resources."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
import uuid

REPO = Path(__file__).resolve().parents[2]
PINS = dict(line.split("=", 1) for line in (REPO / "toolchains.env").read_text().splitlines()
            if line and not line.startswith("#"))
IMAGE = PINS["DEVKIT_NODE_IMAGE"]
NODE_VERSION = PINS["DEVKIT_NODE_VERSION"]
LABEL = "io.docouno.devkit.invocation"
RUNTIME_SOURCE = REPO / "runtime"
ENGINE = Path(os.environ.get("DEVKIT_TEST_ENGINE", REPO / ".artifacts/native/target/x86_64-unknown-linux-musl/debug/devkit")).resolve()
FIXTURE = Path(os.environ.get("DEVKIT_TEST_FIXTURE", ENGINE.parent / "examples/runtime_fixture")).resolve()


def env():
    result = dict(os.environ)
    for key in list(result):
        if key.startswith(("COMPOSE_", "DEVKIT_")) or key in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL"):
            result.pop(key)
    result["DEVKIT_ENGINE_PATH"] = str(ENGINE)
    return result


def run(*args, check=True, timeout=35, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout,
                            env=kwargs.pop("env", env()), **kwargs)
    if check and result.returncode:
        raise AssertionError(f"{args!r}: {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def selected_engine_version():
    metadata = json.loads(run(str(ENGINE), "version", "--json", timeout=10).stdout)
    result = metadata.get("result") if isinstance(metadata, dict) else None
    version = result.get("version") if isinstance(result, dict) else None
    if not (isinstance(metadata, dict)
            and type(metadata.get("schemaVersion")) is int and metadata["schemaVersion"] == 1
            and metadata.get("ok") is True and metadata.get("command") == "version"
            and isinstance(version, str) and len(version) <= 128
            and re.fullmatch(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?", version)
            and not any(re.fullmatch(r"0\d+", part) for part in version.split("+", 1)[0].partition("-")[2].split("."))):
        raise AssertionError("Selected runtime engine returned invalid version metadata")
    return version


VERSION = selected_engine_version()


def container(name):
    result = run("docker", "container", "inspect", name, check=False)
    return json.loads(result.stdout)[0] if result.returncode == 0 else None


def wait_until(predicate, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("bounded readiness condition not met")


def fixture_tool(root, script):
    """Use the real compiled project helpers, without host Node or a fake admission oracle."""
    token = uuid.uuid4().hex
    name = "devkit-runtime-fixture-" + token
    return run(str(ENGINE), "internal", "guard", name, token, "--",
               "docker", "run", "--pull", "never", "--rm", "--init", "--name", name,
               "--label", f"{LABEL}={token}", "--network", "none",
               "--user", f"{os.getuid()}:{os.getgid()}",
               "--volume", f"{root}:/fixture:z",
               "--workdir", "/fixture", "--env", "npm_config_cache=/tmp/npm-cache",
               "--env", "npm_config_userconfig=/dev/null", IMAGE,
               "node", "--input-type=module", "-e", script, timeout=35)


def write_fixture(root, apps=("main",), packages=(), package_profiles=None):
    runtime = root / ".devkit/runtime"
    shutil.copytree(RUNTIME_SOURCE, runtime)
    (runtime / "toolchains.env").write_text(
        f"DEVKIT_VERSION={VERSION}\nDEVKIT_NODE_IMAGE={IMAGE}\nDEVKIT_NODE_VERSION={NODE_VERSION}\n"
        f"DEVKIT_RUST_VERSION={PINS['DEVKIT_RUST_VERSION']}\n"
        f"DEVKIT_RUST_RECIPE_HASH={hashlib.sha256((runtime / 'rust.Dockerfile').read_bytes()).hexdigest()}\n"
        f"DEVKIT_DESKTOP_RECIPE_HASH={hashlib.sha256((runtime / 'desktop.Dockerfile').read_bytes()).hexdigest()}\n")
    (root / "Makefile").write_text("include .devkit/runtime/devkit.mk\n")
    for area, app in [("apps", name) for name in apps] + [("packages", name) for name in packages]:
        path = root / area / app
        path.mkdir(parents=True)
        package = {"name": f"devkit-test-{app}", "version": "1.0.0", "private": True,
                   "scripts": {name: f"node gate.cjs {name}" for name in ("check", "fix", "test", "build", "dev")
                               if name != "dev" or area == "apps"}}
        if area == "packages":
            package["exports"] = {".": "./dist/index.js", "./package.json": "./package.json"}
        (path / "package.json").write_text(json.dumps(package) + "\n")
        (path / "untracked.txt").write_text("untracked source\n")
        (path / "gate.cjs").write_text("const expectedNode = " + json.dumps(NODE_VERSION) + ";\n" + r'''
const fs = require('node:fs');
const path = require('node:path');
if (process.versions.node !== expectedNode) throw Error('wrong pinned Node');
if (process.env.DEVKIT_TEST_SECRET) throw Error('host secret leaked');
if (fs.existsSync('/var/run/docker.sock')) throw Error('Docker socket leaked');
if (!/^\/workspace\/(apps|packages)\//.test(process.cwd())) throw Error('wrong component cwd');
const action = process.argv[2];
if (action === 'fix') fs.writeFileSync('untracked.txt', 'explicit fix\n');
else {
  let blocked = false;
  try { fs.writeFileSync('untracked.txt', 'ILLEGAL'); }
  catch (error) { if (['EROFS', 'EACCES'].includes(error.code)) blocked = true; else throw error; }
  if (!blocked) throw Error('source is writable');
}
fs.mkdirSync('dist', {recursive:true});
fs.writeFileSync('dist/result', action);
fs.writeFileSync('dist/index.js', 'module.exports = true;\n');
fs.writeFileSync('node_modules/task-output', action);
if (fs.existsSync('FAIL')) process.exit(42);
console.log('GATE ' + path.basename(process.cwd()) + ' ' + action);
if (fs.existsSync('SLOW')) {
  require('node:child_process').spawn(process.execPath, ['-e', 'setTimeout(()=>{},25000)'], {stdio:'ignore'});
  console.log('READY');
  setTimeout(()=>{},25000);
}
''')
    accept_runtime(root, native=True, package_profiles=package_profiles)


def seal_runtime(root):
    """Accept known fixture bytes; the same native engine verifies admission."""
    runtime = root / ".devkit/runtime"
    engine_sha = hashlib.sha256(ENGINE.read_bytes()).hexdigest()
    loader = runtime / "devkit.sh"
    source = loader.read_text()
    if "@@DEVKIT_MODE@@" in source:
        for key, value in {"@@DEVKIT_MODE@@": "project", "@@DEVKIT_VERSION@@": VERSION,
                           "@@DEVKIT_ENGINE_SHA256@@": engine_sha,
                           "@@DEVKIT_ENGINE_TARGET@@": "x86_64-unknown-linux-musl"}.items():
            assert source.count(key) == 1
            source = source.replace(key, value)
        loader.write_text(source)
    pins = runtime / "toolchains.env"
    source = pins.read_text()
    if "DEVKIT_ENGINE_SHA256=" not in source:
        pins.write_text(source + f"DEVKIT_ENGINE_SHA256={engine_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\n")
    def identity(path):
        return {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "mode": path.stat().st_mode & 0o777}
    managed = {str(path.relative_to(root)): identity(path)
               for path in sorted((root / ".devkit/runtime").rglob("*")) if path.is_file()}
    state_path = root / ".devkit/manifest.json"
    state = json.loads(state_path.read_text())
    for name in ("project.mk", "compose.apps.yaml", "compose.workspace.json", "tooling-profiles.json"):
        managed[".devkit/" + name] = identity(root / ".devkit" / name)
    for component, info in state["components"].items():
        if info["toolchain"] == "rust":
            pin = root / component / "rust-toolchain.toml"
            managed[str(pin.relative_to(root))] = identity(pin)
    checksum = root / ".devkit/runtime.sha256"
    checksum.write_text("".join(f"{value['sha256']}  {name}\n" for name, value in sorted(managed.items())))
    managed[str(checksum.relative_to(root))] = identity(checksum)
    state.update({"releaseIdentity": hashlib.sha256(json.dumps(managed, sort_keys=True).encode()).hexdigest(),
                  "managed": managed})
    state_path.write_text(json.dumps(state) + "\n")


def accept_runtime(root, native=False, package_profiles=None):
    package_profiles = package_profiles or {}
    state_path = root / ".devkit/manifest.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {
        "schemaVersion": 2, "version": VERSION, "generation": str(uuid.uuid4()), "packageMode": "registry", "releaseIdentity": "0"*64, "managed": {}}
    previous = state.get("components", {})
    components = {}
    for area in ("apps", "packages"):
        if not (root / area).exists():
            continue
        for path in sorted((root / area).iterdir()):
            if not path.is_dir():
                continue
            rust = (path / "Cargo.toml").exists()
            if not rust and not (path / "package.json").exists():
                continue
            name = area + "/" + path.name
            role = "app" if area == "apps" else "library"
            default_lib_profile = package_profiles.get(name) or previous.get(name, {}).get("toolingProfile") or "react-library"
            profile = "rust" if rust else "node" if role == "app" else default_lib_profile
            default_template = "rust-api" if rust else "node-api" if role == "app" else (
                profile if profile in ("ts-library", "react-ui") else "react-client")
            components[name] = {"role": role, "toolchain": "rust" if rust else "node",
                "toolingProfile": profile, "origin": {"template": default_template, "version": VERSION},
                "toolDependencies": {}, "dependencies": [], **previous.get(name, {})}
            components[name].update({"role": role, "toolchain": "rust" if rust else "node", "toolingProfile": profile})
            if role == "app":
                components[name].setdefault("development", {"owner": "project", "kind": "compose", "service": path.name})
    state["components"] = components
    state_path.write_text(json.dumps(state) + "\n")
    # Native npm owns fixture locks; Rust fixture helper renders the actual
    # production projections. No Node metadata/admission implementation is used.
    nodes = sorted(name for name, c in components.items() if c["toolchain"] == "node")
    if native:
        if nodes:
            root_file = root / "package.json"
            manifest = json.loads(root_file.read_text()) if root_file.exists() else {"name":"runtime-fixture","version":"1.0.0","private":True}
            manifest["workspaces"] = nodes
            root_file.write_text(json.dumps(manifest)+"\n")
            for name in nodes:
                (root / name / "package-lock.json").unlink(missing_ok=True)
            fixture_tool(root, "import {spawnSync} from 'node:child_process';const r=spawnSync('npm',['install','--package-lock-only','--ignore-scripts','--include=dev','--strict-peer-deps','--offline','--no-audit','--no-fund'],{stdio:'inherit'});process.exit(r.status??1)")
        else:
            (root / "package.json").unlink(missing_ok=True)
            (root / "package-lock.json").unlink(missing_ok=True)
    run(str(FIXTURE), str(root))
    seal_runtime(root)


def write_rust_fixture(root):
    runtime = root / ".devkit/runtime"
    shutil.copytree(RUNTIME_SOURCE, runtime)
    recipe = hashlib.sha256((runtime / "rust.Dockerfile").read_bytes()).hexdigest()
    (runtime / "toolchains.env").write_text(
        f"DEVKIT_VERSION={VERSION}\nDEVKIT_NODE_IMAGE={IMAGE}\nDEVKIT_NODE_VERSION={NODE_VERSION}\n"
        f"DEVKIT_RUST_VERSION={PINS['DEVKIT_RUST_VERSION']}\n"
        f"DEVKIT_RUST_RECIPE_HASH={recipe}\n"
        f"DEVKIT_DESKTOP_RECIPE_HASH={hashlib.sha256((runtime / 'desktop.Dockerfile').read_bytes()).hexdigest()}\n")
    (root / "Makefile").write_text("include .devkit/runtime/devkit.mk\n")
    shutil.copytree(REPO / "templates/rust-api", root / "apps/api", ignore=shutil.ignore_patterns("target"))
    accept_runtime(root, native=True)
    return recipe


class EngineVersionAdmissionTests(unittest.TestCase):
    def test_wrong_accepted_engine_version_is_rejected_before_docker(self):
        with tempfile.TemporaryDirectory(prefix="devkit-engine-version-") as temporary:
            base = Path(temporary)
            root = base / "checkout with space"
            write_rust_fixture(root)
            wrong = "9.9.9" if VERSION != "9.9.9" else "9.9.8"
            manifest = root / ".devkit/manifest.json"
            state = json.loads(manifest.read_text())
            state["version"] = wrong
            manifest.write_text(json.dumps(state) + "\n")
            pins = root / ".devkit/runtime/toolchains.env"
            pins.write_text(pins.read_text().replace("DEVKIT_VERSION=" + VERSION, "DEVKIT_VERSION=" + wrong))
            seal_runtime(root)
            fake = base / "bin"
            fake.mkdir()
            log = base / "docker-called"
            docker = fake / "docker"
            docker.write_text("#!/bin/sh\nprintf called >> \"$ENGINE_VERSION_DOCKER_LOG\"\nexit 91\n")
            docker.chmod(0o755)
            environment = dict(env(), PATH=str(fake) + os.pathsep + os.environ["PATH"],
                               ENGINE_VERSION_DOCKER_LOG=str(log))
            result = run(str(ENGINE), "task", "doctor", "--root", str(root),
                         env=environment, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Accepted engine version differs from its build", result.stderr)
            self.assertFalse(log.exists(), "Version mismatch reached Docker")


class ComposeCompatibilityTests(unittest.TestCase):
    def test_runtime_and_lifecycle_refuse_old_compose_before_configuration(self):
        with tempfile.TemporaryDirectory(prefix="devkit-compose-compat-") as temporary:
            base = Path(temporary)
            root = base / "checkout with space"
            write_rust_fixture(root)
            fake = base / "bin"
            fake.mkdir()
            docker = fake / "docker"
            docker.write_text('''#!/usr/bin/python3
import json, os, pathlib, sys
argv = sys.argv[1:]
with pathlib.Path(os.environ['COMPAT_DOCKER_LOG']).open('a') as log:
    log.write(json.dumps(argv)+'\\n')
if argv == ['compose', 'version', '--short']:
    print(os.environ['COMPAT_COMPOSE_VERSION'])
elif argv == ['info', '--format', '{{.OSType}}']:
    print('linux')
elif argv[0] == 'compose' and 'config' in argv:
    print(json.dumps({'services': {'api': {}}}))
elif argv[0] == 'compose' and 'ps' in argv:
    print('COMPAT_STATUS_READY')
else:
    print('unexpected Docker query: '+repr(argv), file=sys.stderr)
    sys.exit(90)
''')
            docker.chmod(0o755)
            log = base / "docker.jsonl"
            for version in ("2.38.2", "5.0.0", "v5.6.0-desktop.1"):
                for surface, argv in (
                        ("runtime", [str(ENGINE), "task", "status", "--root", str(root)]),
                        ("lifecycle", [str(FIXTURE), str(root), "--compose-inventory"])):
                    with self.subTest(version=version, surface=surface):
                        log.write_text("")
                        environment = dict(env(), PATH=str(fake)+os.pathsep+os.environ["PATH"],
                                           COMPAT_DOCKER_LOG=str(log), COMPAT_COMPOSE_VERSION=version)
                        result = run(*argv, check=False, env=environment)
                        calls = [json.loads(line) for line in log.read_text().splitlines()]
                        if version == "2.38.2":
                            self.assertNotEqual(result.returncode, 0)
                            self.assertIn("Compose >=5.0.0", result.stderr)
                            self.assertEqual(calls, [["compose", "version", "--short"]],
                                             "unsupported Compose reached configuration or daemon queries")
                        else:
                            self.assertEqual(result.returncode, 0, result.stderr)
                            if surface == "runtime":
                                self.assertIn("COMPAT_STATUS_READY", result.stdout)
                            else:
                                self.assertEqual(json.loads(result.stdout)["services"], ["api"])
                                self.assertTrue(any("config" in call for call in calls))


class DoctorSnapshotTests(unittest.TestCase):
    def test_doctor_rechecks_admitted_snapshot_after_external_query(self):
        # Real admission and CLI, with only Docker's read-only query boundary
        # controlled. This regression needs neither a daemon nor prepared images.
        with tempfile.TemporaryDirectory(prefix="devkit-doctor-snapshot-") as temporary:
            base = Path(temporary)
            root = base / "checkout with space"
            recipe = write_rust_fixture(root)
            (root / ".devkit/state").mkdir(exist_ok=True)
            (root / ".devkit/state/rust-image").write_text("sha256:" + "a"*64 + "\n")
            fake = base / "bin"
            fake.mkdir()
            docker = fake / "docker"
            docker.write_text('''#!/usr/bin/python3
import os, pathlib, sys, time
argv = sys.argv[1:]
base = pathlib.Path(os.environ['DOCTOR_SNAPSHOT_BASE'])
if argv[:2] == ['compose', 'version']:
    print('v5.0.0')
elif argv[:2] == ['info', '--format']:
    print('linux')
elif argv[:2] == ['image', 'inspect']:
    if (base/'armed').exists():
        (base/'ready').write_text('admission finished before image query')
        deadline = time.monotonic() + 5
        while not (base/'go').exists():
            if time.monotonic() > deadline:
                sys.exit(89)
            time.sleep(.01)
    print(os.environ['DOCTOR_SNAPSHOT_RECIPE'])
else:
    print('unexpected Docker query: '+repr(argv), file=sys.stderr)
    sys.exit(90)
''')
            docker.chmod(0o755)
            environment = dict(env(), PATH=str(fake)+os.pathsep+os.environ["PATH"],
                               DOCTOR_SNAPSHOT_BASE=str(base), DOCTOR_SNAPSHOT_RECIPE=recipe)
            argv = [str(ENGINE), "task", "doctor", "--root", str(root)]
            baseline = run(*argv, env=environment, timeout=12)
            self.assertIn("accepted components verified", baseline.stdout)
            evidence = {"engineSha256": hashlib.sha256(ENGINE.read_bytes()).hexdigest(),
                        "baseline": {"code": baseline.returncode, "stdout": baseline.stdout}, "cases": []}
            for mutation in ("native-lock", "manifest-generation"):
                with self.subTest(mutation=mutation):
                    for name in ("ready", "go"):
                        (base / name).unlink(missing_ok=True)
                    (base / "armed").touch()
                    path = root / ("apps/api/Cargo.lock" if mutation == "native-lock" else ".devkit/manifest.json")
                    original = path.read_bytes()
                    child = subprocess.Popen(argv, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    try:
                        wait_until(lambda: (base / "ready").exists() or child.poll() is not None, timeout=5)
                        self.assertTrue((base / "ready").exists(), "doctor exited before the query barrier")
                        if mutation == "native-lock":
                            path.write_bytes(original+b"\n")
                        else:
                            state = json.loads(original)
                            state["generation"] = "11111111-1111-4111-8111-111111111111"
                            path.write_text(json.dumps(state)+"\n")
                        self.assertNotEqual(path.read_bytes(), original)
                        (base / "go").touch()
                        stdout, stderr = child.communicate(timeout=12)
                        evidence["cases"].append({"mutation": mutation, "code": child.returncode,
                                                  "stdout": stdout, "stderr": stderr})
                        self.assertNotEqual(child.returncode, 0, "doctor reported stale success after admitted input changed")
                        self.assertNotIn("accepted components verified", stdout)
                        self.assertIn("changed", stderr.lower())
                    finally:
                        if child.poll() is None:
                            child.kill()
                            child.communicate(timeout=5)
                        path.write_bytes(original)
                        (base / "armed").unlink(missing_ok=True)
            print("Doctor snapshot evidence: " + json.dumps(evidence), flush=True)


class RuntimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        global RUNTIME_SOURCE
        run("docker", "image", "inspect", IMAGE)
        run("docker", "compose", "version")
        if not ENGINE.is_file() or not FIXTURE.is_file():
            raise AssertionError("Build the native devkit and runtime_fixture example first")
        cls.capture = tempfile.TemporaryDirectory(prefix="devkit-runtime-source-")
        captured = Path(cls.capture.name)
        shutil.copytree(RUNTIME_SOURCE, captured / "runtime")
        RUNTIME_SOURCE = captured / "runtime"
        records = [(str(path.relative_to(RUNTIME_SOURCE)), hashlib.sha256(path.read_bytes()).hexdigest())
                   for path in sorted(RUNTIME_SOURCE.rglob("*")) if path.is_file()]
        print("Runtime test source: " + json.dumps({
            "runtime": hashlib.sha256(json.dumps(records, separators=(",", ":")).encode()).hexdigest(),
            "nativeEngine": hashlib.sha256(ENGINE.read_bytes()).hexdigest(),
            "nodeImage": IMAGE}), flush=True)

    @classmethod
    def tearDownClass(cls):
        cls.capture.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="devkit-runtime-test-")
        self.base = Path(self.temp.name)
        self.root = self.base / "checkout with space"
        self.token = uuid.uuid4().hex
        self.resources = []
        self.tokens = {}
        self.projects = []

    def tearDown(self):
        errors = []
        for name in self.resources:
            found = container(name)
            if found:
                if found["Config"]["Labels"].get(LABEL) != self.tokens.get(name, self.token):
                    errors.append(f"unexpected ownership: {name}")
                else:
                    run("docker", "rm", "--force", name)
            if container(name):
                errors.append(f"container left: {name}")
        for project, root, volume in self.projects:
            for kind, name in (("network", project + "_default"), ("volume", volume)):
                found = run("docker", kind, "inspect", name, check=False)
                if found.returncode == 0:
                    labels = json.loads(found.stdout)[0].get("Labels", {})
                    if labels.get(LABEL) != self.token:
                        errors.append(f"unexpected {kind} ownership: {name}")
                    else:
                        run("docker", kind, "rm", name)
        self.temp.cleanup()
        self.assertEqual(errors, [])

    def make(self, *args, check=True, **kwargs):
        return run("make", "--no-print-directory", "-C", str(self.root), *args, check=check, **kwargs)

    def owned_name(self):
        name = "devkit-runtime-test-" + self.token + "-" + str(len(self.resources))
        self.resources.append(name)
        return name

    def test_native_setup_gates_fix_purity_stale_lock_and_secret_boundary(self):
        write_fixture(self.root)
        source = self.root / "apps/main/untracked.txt"
        e = env()
        e["DEVKIT_TEST_SECRET"] = "must-not-reach-tools"
        self.make("setup", env=e)
        hashes = {path: hashlib.sha256(path.read_bytes()).hexdigest()
                  for path in (source, source.with_name("package.json"), self.root / "package.json", self.root / "package-lock.json")}
        for command in ("check", "test", "build"):
            self.assertIn(f"GATE main {command}", self.make(command, env=e).stdout)
        self.assertEqual(hashes, {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in hashes})
        self.make("fix")
        self.assertEqual(source.read_text(), "explicit fix\n")
        lock = self.root / "package-lock.json"
        lock.write_text(lock.read_text() + "\n")
        result = self.make("check", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dependencies are absent/stale", result.stderr)

    def test_make_named_edges_selector_native_failure(self):
        write_fixture(self.root, ("web", "api"))
        self.make("setup")
        with (self.root / "Makefile").open("a") as file:
            file.write("build-api: build-web\n")
        result = self.make("build", "app=api")
        self.assertLess(result.stdout.index("GATE web build"), result.stdout.index("GATE api build"))
        (self.root / "apps/api/FAIL").write_text("fail")
        result = self.make("test-api", check=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("Error 42", result.stderr)

    def test_one_workspace_setup_shared_library_gates_and_full_input_receipt(self):
        write_fixture(self.root, ("web", "api"), ("client",))
        for area, name in (("apps", "web"), ("apps", "api"), ("packages", "client")):
            path = self.root / area / name / "package.json"
            package = json.loads(path.read_text())
            package["scripts"]["postinstall"] = "node -e \"require('node:fs').appendFileSync(process.env.NPM_CONFIG_CACHE+'/setup-count',process.cwd()+'\\n')\""
            if name == "web":
                package["dependencies"] = {"devkit-test-client": "file:../../packages/client",
                                           "fixture-link": "file:../../shared/link"}
            path.write_text(json.dumps(package))
        for name, dependencies in (("link", {"fixture-leaf": "file:../leaf"}), ("leaf", {})):
            path = self.root / "shared" / name
            path.mkdir(parents=True)
            (path / "package.json").write_text(json.dumps({"name": "fixture-" + name,
                "version": "1.0.0", "dependencies": dependencies}))
            (path / "index.js").write_text("module.exports = true;\n")
        accept_runtime(self.root, native=True)
        self.make("setup")
        count = (self.root / ".devkit/cache/npm/setup-count").read_text().splitlines()
        self.assertEqual(sorted(count), ["/workspace/apps/api", "/workspace/apps/web", "/workspace/packages/client"])
        for name in ("apps/web", "apps/api", "packages/client"):
            self.assertFalse((self.root / name / "package-lock.json").exists())
        checked = self.make("-j4", "check", "build", "app=web").stdout
        self.assertLess(checked.index("GATE client check"), checked.index("GATE web check"))
        self.assertLess(checked.index("GATE client build"), checked.index("GATE web check"))
        self.assertIn("GATE web build", checked)
        self.assertNotIn("GATE api check", checked)
        self.assertIn("GATE client check", self.make("check", "package=client").stdout)
        failure = self.root / "packages/client/FAIL"
        failure.touch()
        blocked = self.make("check", "app=web", check=False)
        self.assertNotEqual(blocked.returncode, 0)
        self.assertIn("Error 42", blocked.stderr)
        self.assertNotIn("GATE web check", blocked.stdout)
        failure.unlink()
        receipt = (self.root / ".devkit/state/node-workspace.lock").read_bytes()
        for relative in ("package.json", "package-lock.json", "packages/client/package.json",
                         "shared/link/package.json", "shared/leaf/package.json"):
            with self.subTest(input=relative):
                path = self.root / relative
                before = path.read_bytes()
                path.write_bytes(before + b"\n")
                try:
                    stale = self.make("check", "app=web", check=False)
                    self.assertNotEqual(stale.returncode, 0)
                    self.assertIn("workspace dependencies are absent/stale", stale.stderr)
                    self.assertEqual((self.root / ".devkit/state/node-workspace.lock").read_bytes(), receipt)
                finally:
                    path.write_bytes(before)
        config = self.root / "shared/leaf/.npmrc"
        config.write_text("\n")
        try:
            self.assertIn("workspace dependencies are absent/stale", self.make("check", check=False).stderr)
        finally:
            config.unlink()
        self.assertIn("GATE web check", self.make("check", "app=web").stdout)

    def test_control_uses_exact_node_pin_and_real_tool_profile_validation(self):
        write_fixture(self.root)
        self.make("setup")
        pins = self.root / ".devkit/runtime/toolchains.env"
        before = pins.read_text()
        pins.write_text(before.replace("DEVKIT_NODE_VERSION=" + NODE_VERSION, "DEVKIT_NODE_VERSION=0.0.1"))
        seal_runtime(self.root)
        mismatch = self.make("setup", check=False)
        self.assertNotEqual(mismatch.returncode, 0)
        self.assertIn("Unexpected Node version", mismatch.stderr)
        self.assertNotIn("GATE main check", mismatch.stdout)
        pins.write_text(before)
        profiles = self.root / ".devkit/tooling-profiles.json"
        profile = json.loads(profiles.read_text())
        profile["profiles"]["node"] = {"fixture-owned": "1.0.0"}
        profiles.write_text(json.dumps(profile))
        seal_runtime(self.root)
        invalid = self.make("check", check=False)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("Accepted tooling profile differs", invalid.stderr)
        self.assertNotIn("GATE main check", invalid.stdout)

    def test_manifest_generation_changes_between_apps_or_before_final_barrier_fail(self):
        write_fixture(self.root, ("web", "api"))
        self.make("setup")
        update = self.root / "simulate-update.sh"
        update.write_text('''#!/usr/bin/env bash
set -euo pipefail
exec 9>.devkit/state/task.lock
flock -n 9
python3 - <<'PY'
import json, os
path = '.devkit/manifest.json'
state = json.load(open(path))
state['generation'] = '11111111-1111-4111-8111-111111111111'
with open(path + '.next', 'w') as file: json.dump(state, file)
os.replace(path + '.next', path)
PY
''')
        original = (self.root / "Makefile").read_text()
        (self.root / "Makefile").write_text(original + '''
.PHONY: simulate-update
simulate-update: build-web
\t@bash simulate-update.sh
build-api: simulate-update
''')
        result = self.make("build", "app=api", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("GATE web build", result.stdout)
        self.assertNotIn("GATE api build", result.stdout)
        self.assertIn("accepted toolkit changed", result.stderr)
        # A fresh invocation accepts the current generation; its final aggregate
        # barrier must still catch a change after the last successful native gate.
        update.write_text(update.read_text().replace('11111111-1111-4111-8111-111111111111', '22222222-2222-4222-8222-222222222222'))
        (self.root / "Makefile").write_text(original + '''
.PHONY: simulate-update
simulate-update: build-web
\t@bash simulate-update.sh
build: simulate-update
''')
        result = self.make("build", "app=web", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("GATE web build", result.stdout)
        self.assertIn("accepted toolkit changed", result.stderr)

    def test_missing_native_script_and_missing_accepted_image_fail(self):
        write_fixture(self.root)
        path = self.root / "apps/main/package.json"
        package = json.loads(path.read_text())
        del package["scripts"]["test"]
        path.write_text(json.dumps(package))
        result = self.make("setup", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("native script apps/main:test", result.stderr)
        self.assertFalse((self.root / ".devkit/state/node-workspace.lock").exists())
        package["scripts"]["test"] = "node gate.cjs test"
        path.write_text(json.dumps(package))
        pins = self.root / ".devkit/runtime/toolchains.env"
        pins.write_text(pins.read_text().replace(IMAGE, "node:missing@sha256:" + "a" * 64))
        accept_runtime(self.root)
        result = self.make("check", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("accepted Node image is absent; run make setup", result.stderr)

    def test_rust_derived_image_state_and_native_gates(self):
        write_fixture(self.root)
        shutil.rmtree(self.root / "apps/main")
        shutil.copytree(REPO / "templates/rust-api", self.root / "apps/main",
                        ignore=shutil.ignore_patterns("target"))
        runtime = self.root / ".devkit/runtime"
        recipe = hashlib.sha256((runtime / "rust.Dockerfile").read_bytes()).hexdigest()
        accept_runtime(self.root, native=True)
        result = self.make("check", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rust image is absent/stale; run make setup", result.stderr)
        self.make("setup", timeout=180)
        image_id = (self.root / ".devkit/state/rust-image").read_text().strip()
        self.assertRegex(image_id, r"^sha256:[a-f0-9]{64}$")
        image = json.loads(run("docker", "image", "inspect", image_id).stdout)[0]
        self.assertEqual(image["Config"]["Labels"]["io.docouno.devkit.rust-recipe"], recipe)
        files = [path for path in (self.root / "apps/main").rglob("*") if path.is_file() and "target" not in path.parts]
        hashes = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in files}
        for command in ("check", "test", "build"):
            self.make(command, timeout=180)
        self.assertEqual(hashes, {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in files})
        self.assertEqual((self.root / ".devkit/state/rust-image").read_text().strip(), image_id)

    def test_development_admits_compose_dependencies_before_start(self):
        write_fixture(self.root, ("web", "api"))
        api = self.root / "apps/api"
        shutil.rmtree(api)
        (api / "src").mkdir(parents=True)
        (api / "Cargo.toml").write_text('[package]\nname="runtime-api"\nversion="0.1.0"\nedition="2024"\n')
        (api / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "runtime-api"\nversion = "0.1.0"\n')
        (api / "rust-toolchain.toml").write_text('[toolchain]\nchannel="1.99.1"\n')
        (api / "rustfmt.toml").write_text('edition="2024"\nstyle_edition="2024"\n')
        (api / "src/main.rs").write_text('fn main() { loop { std::thread::sleep(std::time::Duration::from_secs(1)); } }\n')
        package_path = self.root / "apps/web/package.json"
        package = json.loads(package_path.read_text())
        package["scripts"]["dev"] = "node server.cjs"
        package_path.write_text(json.dumps(package))
        (package_path.parent / "server.cjs").write_text("require('node:http').createServer((q,s)=>s.end('ok')).listen(8080,'0.0.0.0',()=>console.log('PROFILE_READY'));\n")
        accept_runtime(self.root, native=True)
        state_path = self.root / ".devkit/manifest.json"
        state = json.loads(state_path.read_text())
        for component in state["components"].values():
            component["development"]["owner"] = "managed"
        state_path.write_text(json.dumps(state))
        accept_runtime(self.root)
        project = "devkit-" + hashlib.sha256(str(self.root.resolve()).encode()).hexdigest()[:16]
        names = {name: project + "-" + name + "-1" for name in ("web", "api")}
        self.resources.extend(names.values())
        self.projects.append((project, self.root, "unused-volume-" + self.token))
        labels = {LABEL: self.token}

        def compose(dependent=None, profiled=False):
            services = {name: {"labels": labels} for name in names}
            if dependent == "api":
                services["api"]["depends_on"] = ["web"]
            if dependent == "web":
                services["web"]["depends_on"] = ["api"]
            if profiled:
                services["web"]["profiles"] = ["client"]
                services["api"]["profiles"] = ["unused"]
                services["api"]["env_file"] = [str(self.base / "inactive-service.env")]
            (self.root / "compose.yaml").write_text(json.dumps({"services": services,
                "networks": {"default": {"labels": labels}}}))

        self.make("setup", "app=api", timeout=180)
        compose("api")
        missing = self.make("up", "app=api", check=False)
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("workspace dependencies are absent/stale", missing.stderr)
        self.assertTrue(all(container(name) is None for name in names.values()))
        self.make("setup", "app=web")
        self.make("up", "app=api")
        self.assertTrue(all(container(name)["State"]["Running"] for name in names.values()))
        self.make("down")

        recipe = self.root / ".devkit/runtime/rust.Dockerfile"
        before = hashlib.sha256(recipe.read_bytes()).hexdigest()
        recipe.write_bytes(recipe.read_bytes() + b"\n# accepted recipe revision\n")
        after = hashlib.sha256(recipe.read_bytes()).hexdigest()
        pins = self.root / ".devkit/runtime/toolchains.env"
        pins.write_text(pins.read_text().replace(before, after))
        seal_runtime(self.root)
        self.make("setup", "app=web")
        compose("web")
        stale = self.make("up", "app=web", check=False)
        self.assertNotEqual(stale.returncode, 0)
        self.assertIn("rust image is absent/stale", stale.stderr)
        self.assertTrue(all(container(name) is None for name in names.values()))
        compose(profiled=True)
        self.make("up", "app=web")
        self.assertTrue(container(names["web"])["State"]["Running"])
        self.assertIsNone(container(names["api"]), "unrelated unprepared app was started")
        self.assertIn(names["web"], self.make("status").stdout)
        self.assertIn("PROFILE_READY", self.make("logs", "app=web").stdout)
        self.assertIn("PROFILE_READY", self.make("logs").stdout)
        self.make("down")
        self.assertTrue(all(container(name) is None for name in names.values()), "profiled service survived down")

    def test_reject_registration_pending_and_lock_contention(self):
        write_fixture(self.root)
        manifest = self.root / ".devkit/manifest.json"
        accepted_manifest = manifest.read_bytes()
        empty = json.loads(accepted_manifest)
        empty["components"] = {}
        manifest.write_text(json.dumps(empty))
        self.assertIn("Project has no registered components", self.make("check", check=False).stderr)
        manifest.write_bytes(accepted_manifest)
        self.make("setup")
        for selector in ("absent", "main;touch bad"):
            result = self.make("doctor", "app=" + selector, check=False)
            self.assertNotEqual(result.returncode, 0)
        (self.root / ".devkit/update-pending.json").write_text("{}")
        self.assertIn("update is incomplete", self.make("doctor", check=False).stderr)
        (self.root / ".devkit/update-pending.json").unlink()
        lock = self.root / ".devkit/state/task.lock"
        # Linux flock is shared with the real updater admission primitive.
        import fcntl
        with lock.open("w") as file:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertIn("Another devkit operation", self.make("doctor", check=False).stderr)
        (self.root / "apps/main/Cargo.toml").write_text("[package]\n")
        ambiguous = self.make("doctor", check=False)
        self.assertNotEqual(ambiguous.returncode, 0, ambiguous.stdout + ambiguous.stderr)
        self.assertIn("Ambiguous native", ambiguous.stderr)
        (self.root / ".devkit/runtime/toolchains.env").write_text("DEVKIT_VERSION=tampered\n")
        self.assertIn("Managed file conflict", self.make("doctor", check=False).stderr)

    def test_writable_symlink_paths_and_unexpected_types_never_mutate_sentinels(self):
        write_fixture(self.root)
        self.make("setup")
        outside = self.base / "outside"
        outside.mkdir()
        sentinel = outside / "result"
        sentinel.write_text("IMPORTANT\n")
        (outside / "task.lock").write_text("OUTSIDE LOCK\n")
        source = self.root / "apps/main/src"
        source.mkdir()
        (source / "result").write_text("SOURCE\n")
        directories = [".devkit", ".devkit/state", ".devkit/cache", ".devkit/cache/npm", ".devkit/cache/cargo", ".devkit/cache/electron",
                       "node_modules", "apps/main/node_modules", "apps/main/dist"]
        files = [".devkit/state/task.lock", ".devkit/state/node-workspace.lock", ".devkit/state/rust-image", ".devkit/state/desktop-image"]
        dist_occurrence = 0
        for relative in directories + files + ["apps/main/dist"]:
            with self.subTest(path=relative):
                path = self.root / relative
                saved = path.with_name(path.name + ".saved")
                had_original = path.exists()
                if had_original:
                    path.rename(saved)
                # .devkit must still resolve the real entrypoint for this test.
                if relative == "apps/main/dist": dist_occurrence += 1
                target = saved if relative == ".devkit" else (source if relative == "apps/main/dist" and dist_occurrence == 1 else outside)
                if relative in files:
                    target = sentinel
                path.symlink_to(target, target_is_directory=relative in directories)
                try:
                    result = self.make("check", check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("symlink", result.stderr)
                    self.assertEqual(sentinel.read_text(), "IMPORTANT\n")
                    self.assertEqual((outside / "task.lock").read_text(), "OUTSIDE LOCK\n")
                    self.assertEqual((source / "result").read_text(), "SOURCE\n")
                    self.assertEqual(sorted(path.name for path in outside.iterdir()), ["result", "task.lock"])
                finally:
                    path.unlink()
                    if had_original:
                        saved.rename(path)
        # Non-directory mount sources and nonregular lock leaves also fail before open/bind.
        dist = self.root / "apps/main/dist"
        shutil.rmtree(dist)
        dist.write_text("DO NOT REPLACE\n")
        self.assertIn("Expected directory", self.make("check", check=False).stderr)
        self.assertEqual(dist.read_text(), "DO NOT REPLACE\n")
        dist.unlink()
        dist.mkdir()
        lock = self.root / ".devkit/state/task.lock"
        lock.unlink()
        os.mkfifo(lock)
        self.assertIn("regular file", self.make("doctor", check=False).stderr)

    def test_nontruncating_lock_and_every_pending_object_blocks(self):
        write_fixture(self.root)
        self.make("setup")
        lock = self.root / ".devkit/state/task.lock"
        lock.write_text("LOCK CONTENT MUST SURVIVE\n")
        self.make("doctor")
        self.assertEqual(lock.read_text(), "LOCK CONTENT MUST SURVIVE\n")
        pending = self.root / ".devkit/update-pending.json"
        for kind in ("json", "malformed", "directory", "symlink", "dangling"):
            with self.subTest(kind=kind):
                if kind == "directory":
                    pending.mkdir()
                elif kind in ("symlink", "dangling"):
                    pending.symlink_to(lock if kind == "symlink" else self.base / "missing-journal")
                else:
                    pending.write_text("{}" if kind == "json" else "not json")
                try:
                    for command in ("doctor", "check"):
                        result = self.make(command, check=False)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("update is incomplete", result.stderr)
                    self.assertEqual(lock.read_text(), "LOCK CONTENT MUST SURVIVE\n")
                finally:
                    if pending.is_dir() and not pending.is_symlink(): pending.rmdir()
                    else: pending.unlink()

    def test_failed_repeat_setup_invalidates_receipt_before_mutation(self):
        write_fixture(self.root)
        package_path = self.root / "apps/main/package.json"
        package = json.loads(package_path.read_text())
        package["scripts"]["postinstall"] = 'node -e "const f=require(\'node:fs\');f.writeFileSync(\'node_modules/install-mutated\',\'yes\');if(f.existsSync(\'FAIL_SETUP\'))process.exit(42)"'
        package_path.write_text(json.dumps(package))
        accept_runtime(self.root, native=True)
        self.make("setup")
        receipt = self.root / ".devkit/state/node-workspace.lock"
        self.assertTrue(receipt.is_file())
        before = package_path.read_bytes(), (self.root / "package-lock.json").read_bytes()
        (self.root / "apps/main/FAIL_SETUP").touch()
        result = self.make("setup", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.root / "apps/main/node_modules/install-mutated").is_file())
        self.assertFalse(receipt.exists())
        self.assertEqual(before, (package_path.read_bytes(), (self.root / "package-lock.json").read_bytes()))
        self.assertIn("dependencies are absent/stale", self.make("check", check=False).stderr)

    def test_cancelled_repeat_setup_invalidates_receipt(self):
        write_fixture(self.root)
        package_path = self.root / "apps/main/package.json"
        package = json.loads(package_path.read_text())
        package["scripts"]["postinstall"] = 'node -e "const f=require(\'node:fs\');if(f.existsSync(\'SLOW_SETUP\')){f.writeFileSync(\'node_modules/ready-setup\',\'ready\');console.log(\'READY_SETUP\');setTimeout(()=>{},25000)}"'
        package_path.write_text(json.dumps(package))
        accept_runtime(self.root, native=True)
        self.make("setup")
        receipt = self.root / ".devkit/state/node-workspace.lock"
        self.assertTrue(receipt.is_file())
        (self.root / "apps/main/SLOW_SETUP").touch()
        project = "devkit-" + hashlib.sha256(str(self.root.resolve()).encode()).hexdigest()[:16]
        proc = subprocess.Popen(["make", "--no-print-directory", "-C", str(self.root), "setup"],
            env=env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
        name = None
        try:
            def ready():
                nonlocal name
                names = run("docker", "container", "ls", "--filter", "label=io.docouno.devkit.checkout=" + project,
                            "--filter", "label=io.docouno.devkit.role=tool", "--format", "{{.Names}}").stdout.splitlines()
                if not names: return False
                name = names[0]
                found = container(name)
                if found is None: return False
                if name not in self.resources:
                    self.resources.append(name)
                    self.tokens[name] = found["Config"]["Labels"][LABEL]
                # npm may buffer successful workspace lifecycle stdout. A file
                # written by the hook is the actual in-container READY barrier.
                return (self.root / "apps/main/node_modules/ready-setup").is_file()
            wait_until(ready)
            self.assertFalse(receipt.exists())
            os.killpg(proc.pid, signal.SIGTERM)
            output, _ = proc.communicate(timeout=12)
            self.assertNotEqual(proc.returncode, 0, output)
            self.assertIsNone(container(name))
            self.assertFalse(receipt.exists())
            self.assertIn("dependencies are absent/stale", self.make("check", check=False).stderr)
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.communicate(timeout=12)

    def test_manifest_binds_checksum_set_modes_and_essential_schema(self):
        write_fixture(self.root)
        nested = self.root / ".devkit/runtime/nested"
        nested.mkdir()
        (nested / "managed.txt").write_text("managed fixture input\n")
        accept_runtime(self.root)
        self.make("setup")
        manifest = self.root / ".devkit/manifest.json"
        checksum = self.root / ".devkit/runtime.sha256"
        loader = self.root / ".devkit/runtime/devkit.sh"
        originals = {path: (path.read_bytes(), path.stat().st_mode & 0o777) for path in (manifest, checksum, loader)}
        def restore():
            for path, (content, mode) in originals.items():
                path.write_bytes(content)
                path.chmod(mode)
        for case in ("omitted-and-changed", "incomplete-table", "unlisted", "managed-symlink-parent", "mode", "version", "schema", "generation", "packageMode", "releaseIdentity", "unsafe-path"):
            with self.subTest(case=case):
                extra = self.root / ".devkit/runtime/unlisted.txt"
                saved_parent = self.base / "managed-parent-saved"
                state = json.loads(manifest.read_text())
                try:
                    if case in ("omitted-and-changed", "incomplete-table"):
                        checksum.write_text("".join(line for line in checksum.read_text().splitlines(True) if not line.endswith("/devkit.sh\n")))
                        if case == "omitted-and-changed":
                            loader.write_text(loader.read_text() + "\n# benign drift\n")
                            self.assertEqual(manifest.read_bytes(), originals[manifest][0])
                        else:
                            state["managed"][".devkit/runtime.sha256"]["sha256"] = hashlib.sha256(checksum.read_bytes()).hexdigest()
                            manifest.write_text(json.dumps(state))
                    elif case == "unlisted": extra.write_text("unlisted managed runtime input\n")
                    elif case == "managed-symlink-parent":
                        nested.rename(saved_parent)
                        nested.symlink_to(saved_parent, target_is_directory=True)
                    elif case == "mode": loader.chmod(0o644 if originals[loader][1] == 0o755 else 0o755)
                    else:
                        if case == "unsafe-path": state["managed"]["../outside"] = {"sha256": "0" * 64, "mode": 0o644}
                        else: state[case] = {"version": "9.9.9", "schema": 2, "generation": "initial", "packageMode": "other", "releaseIdentity": "bad"}[case]
                        if case == "schema": state["schemaVersion"] = 99
                        manifest.write_text(json.dumps(state))
                    result = self.make("check", check=False)
                    self.assertNotEqual(result.returncode, 0, case)
                    self.assertNotIn("GATE main check", result.stdout)
                    self.assertTrue(any(message in result.stderr.lower() for message in (
                        "managed file conflict", "checksum table", "unlisted or nonregular", "unlisted runtime", "symlink",
                        "version differs", "schemaversion", "identity/generation", "packagemode",
                        "releaseidentity", "unsafe", "invalid project", "unknown variant")), result.stderr)
                finally:
                    extra.unlink(missing_ok=True)
                    if nested.is_symlink():
                        nested.unlink()
                        saved_parent.rename(nested)
                    restore()

    def test_registration_covers_physical_apps_and_preserves_unknown_metadata(self):
        write_fixture(self.root)
        self.make("setup")
        manifest = self.root / ".devkit/manifest.json"
        state = json.loads(manifest.read_text())
        state["ownerExtension"] = {"custom": "preserve"}
        manifest.write_text(json.dumps(state))
        accepted = manifest.read_bytes()
        self.make("check")
        self.assertEqual(manifest.read_bytes(), accepted)
        shutil.copytree(self.root / "apps/main", self.root / "apps/extra", ignore=shutil.ignore_patterns("node_modules", "dist"))
        package = self.root / "apps/extra/package.json"
        extra = json.loads(package.read_text())
        extra["name"] = "devkit-test-extra"
        package.write_text(json.dumps(extra))
        self.assertIn("Unregistered native component: apps/extra", self.make("check", check=False).stderr)
        state["components"]["apps/extra"] = {**state["components"]["apps/main"],
            "development": {"owner": "project", "kind": "compose", "service": "extra"}}
        manifest.write_text(json.dumps(state))
        self.assertIn("workspace membership differs", self.make("check", check=False).stderr)
        makefile = self.root / "Makefile"
        makefile.write_text(makefile.read_text() + "\nowner-hook:\n\t@true\n")
        accepted_makefile = makefile.read_bytes()
        accept_runtime(self.root, native=True)
        self.make("setup")
        result = self.make("check")
        self.assertIn("GATE main check", result.stdout)
        self.assertIn("GATE extra check", result.stdout)
        self.assertEqual(json.loads(manifest.read_text())["ownerExtension"], {"custom": "preserve"})
        self.assertEqual(makefile.read_bytes(), accepted_makefile)
        projection = self.root / ".devkit/project.mk"
        projection.write_text(projection.read_text().replace("DEVKIT_REGISTERED_APPS := extra main", "DEVKIT_REGISTERED_APPS := main"))
        seal_runtime(self.root)
        rejected = self.make("check", check=False)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("Make registration projection is stale", rejected.stderr)

    def test_metadata_is_native_and_requires_no_docker_or_dependencies(self):
        write_fixture(self.root)
        e = env()
        e["DOCKER_HOST"] = "unix:///nonexistent-devkit-test-docker.sock"
        result = run(str(ENGINE), "describe", "--root", str(self.root), env=e)
        self.assertEqual(json.loads(result.stdout)["version"], VERSION)
        self.assertFalse((self.root / "node_modules").exists())
        self.assertFalse((self.root / ".devkit/cache").exists())

    def guard(self, name, argv):
        return subprocess.Popen([str(ENGINE), "internal", "guard", name, self.token, "--", *argv],
            env=env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)

    def task_argv(self, name, code):
        return ["docker", "run", "--pull", "never", "--rm", "--init", "--name", name,
                "--label", f"{LABEL}={self.token}", IMAGE, "node", "-e", code]

    def test_exec_repairs_stale_lock_invalidates_receipt_and_preserves_exact_argv(self):
        write_fixture(self.root)
        self.make("setup")
        receipt = self.root / ".devkit/state/node-workspace.lock"
        (self.root / "package-lock.json").write_text("invalid native lock\n")
        self.assertNotEqual(self.make("check", check=False).returncode, 0)
        run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main", "--write", "--",
            "npm", "install", "--package-lock-only", "--ignore-scripts", "--offline", "--strict-peer-deps")
        self.assertFalse(receipt.exists())
        self.make("setup")
        args = ["space value", "$literal", ";", "$(not-a-shell)", "quote'\"value"]
        result = run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main", "--",
                     "node", "-e", "process.stdout.write(JSON.stringify(process.argv.slice(1)))", *args)
        self.assertEqual(json.loads(result.stdout), args)
        before = receipt.read_bytes()
        result = run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main", "--",
                     "node", "-e", "require('node:fs').writeFileSync('/workspace/node_modules/ILLEGAL','bad')", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(receipt.read_bytes(), before)
        self.assertFalse((self.root / "node_modules/ILLEGAL").exists())
        self.assertIn("GATE main check", self.make("check").stdout)
        result = self.make("exec", "app=main", "command=node -e 'process.stdout.write(process.argv[1])' 'with spaces'")
        self.assertEqual(result.stdout, "with spaces")
        literal = "$HOME;$(touch NEVER_EVALUATE)"
        result = self.make("exec", "app=main", "command=node -e 'process.stdout.write(process.argv[1])' '" + literal + "'")
        self.assertEqual(result.stdout, literal)
        self.assertFalse((self.root / "NEVER_EVALUATE").exists())
        result = run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main", "--write", "--", "node", "-e", "process.exit(42)", check=False)
        self.assertEqual(result.returncode, 42)
        self.assertFalse(receipt.exists())

    def test_exec_cargo_add_uses_native_toolchain_without_accepting_integration(self):
        write_fixture(self.root)
        shutil.rmtree(self.root / "apps/main")
        app = self.root / "apps/main"
        # This std-only program must not retain the HTTP template library/tests.
        shutil.copytree(REPO / "templates/rust-api", app, ignore=shutil.ignore_patterns("target", "tests", "lib.rs"))
        (app / "Cargo.toml").write_text('[package]\nname="exec-api"\nversion="0.1.0"\nedition="2024"\n')
        (app / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname="exec-api"\nversion="0.1.0"\n')
        (app / "src/main.rs").write_text('fn main() { println!("native exec"); }\n')
        accept_runtime(self.root, native=True)
        (app / "Cargo.lock").unlink()
        helper = app / "vendor helper"
        (helper / "src").mkdir(parents=True)
        (helper / "Cargo.toml").write_text('[package]\nname="exec-helper"\nversion="0.1.0"\nedition="2024"\n')
        (helper / "src/lib.rs").write_text('pub fn value() -> u8 { 1 }\n')
        before = (self.root / ".devkit/manifest.json").read_bytes()
        run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main", "--write", "--prepare", "--", "cargo", "add", "--offline", "--path", "vendor helper", timeout=180)
        self.assertIn("exec-helper", (app / "Cargo.toml").read_text())
        self.assertTrue((app / "Cargo.lock").exists())
        self.assertEqual((self.root / ".devkit/manifest.json").read_bytes(), before)
        self.make("setup", timeout=180)
        self.make("build", timeout=180)

    def test_node_receipt_ignores_rust_config_changes_but_node_config_stales_it(self):
        write_fixture(self.root, ("main", "api"))
        shutil.rmtree(self.root / "apps/api")
        shutil.copytree(REPO / "templates/rust-api", self.root / "apps/api", ignore=shutil.ignore_patterns("target"))
        accept_runtime(self.root, native=True)
        self.make("setup", "app=main")
        receipt = self.root / ".devkit/state/node-workspace.lock"
        before = receipt.read_bytes()
        config = self.root / "apps/api/rustfmt.toml"
        config.write_text(config.read_text()+"\nmax_width=90\n")
        cargo = self.root / "apps/api/.cargo"
        cargo.mkdir(exist_ok=True)
        (cargo / "config.toml").write_text("[build]\njobs=1\n")
        self.assertIn("GATE main check", self.make("check", "app=main").stdout)
        self.assertEqual(receipt.read_bytes(), before)
        (self.root / ".npmrc").write_text("fund=false\n")
        self.assertIn("dependencies are absent/stale", self.make("check", "app=main", check=False).stderr)
        self.assertEqual(receipt.read_bytes(), before)

    def test_status_logs_down_need_no_images_node_modules_or_native_lock(self):
        write_fixture(self.root)
        state_path = self.root / ".devkit/manifest.json"
        state = json.loads(state_path.read_text())
        state["components"]["apps/main"]["development"]["owner"] = "managed"
        state_path.write_text(json.dumps(state))
        accept_runtime(self.root)
        pins = self.root / ".devkit/runtime/toolchains.env"
        pins.write_text(pins.read_text().replace(IMAGE, "node:missing@sha256:"+"a"*64))
        seal_runtime(self.root)
        (self.root / "package-lock.json").unlink()
        for command in ("status", "logs", "down"):
            self.make(command)
        self.assertFalse((self.root / "node_modules").exists())
        self.assertFalse((self.root / ".devkit/cache").exists())

    def test_guard_preserves_native_failure(self):
        name = self.owned_name()
        proc = self.guard(name, self.task_argv(name, "process.exit(42)"))
        output, _ = proc.communicate(timeout=12)
        self.assertEqual(proc.returncode, 42, output)
        self.assertIsNone(container(name))

    def test_guard_cancellation_after_ready_reaps_child(self):
        for sig in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=sig):
                name = self.owned_name()
                proc = self.guard(name, self.task_argv(name,
                    "const cp=require('node:child_process');cp.spawn(process.execPath,['-e',"
                    "'setTimeout(()=>{},25000)'],{stdio:'ignore'});console.log('READY');setTimeout(()=>{},25000)"))
                try:
                    wait_until(lambda: container(name) is not None and "READY" in run("docker", "logs", name, check=False).stdout)
                    pids = run("docker", "top", name, "-eo", "pid").stdout.splitlines()[1:]
                    stamps = {int(pid): Path(f"/proc/{pid.strip()}/stat").read_text().rsplit(")", 1)[1].split()[19] for pid in pids}
                    os.kill(proc.pid, sig)
                    output, _ = proc.communicate(timeout=12)
                    self.assertEqual(proc.returncode, 130 if sig == signal.SIGINT else 143, output)
                    self.assertIsNone(container(name))
                    for pid, stamp in stamps.items():
                        stat = Path(f"/proc/{pid}/stat")
                        self.assertFalse(stat.exists() and stat.read_text().rsplit(")", 1)[1].split()[19] == stamp, f"process survived: {pid}")
                finally:
                    if proc.poll() is None:
                        os.killpg(proc.pid, signal.SIGTERM)
                        proc.communicate(timeout=12)

    def test_pre_ready_cancellation_stops_creator_before_delayed_create(self):
        # A deterministic creator barrier, not a lucky cancellation after READY.
        # The shell forks sleep, then would exec the actual pinned Docker run.
        name = self.owned_name()
        marker = self.base / "creator-started"
        child_pid = self.base / "creator-child"
        script = self.base / "creator.sh"
        script.write_text('#!/usr/bin/env bash\nset -e\nprintf started > "$1"\nshift\nsleep 1.5 &\nprintf "%s" "$!" > "$1"\nshift\nwait\nexec "$@"\n')
        argv = ["bash", str(script), str(marker), str(child_pid), *self.task_argv(name, "setTimeout(()=>{},25000)")]
        proc = self.guard(name, argv)
        try:
            wait_until(lambda: child_pid.exists())
            self.assertIsNone(container(name))
            os.kill(proc.pid, signal.SIGTERM)
            output, _ = proc.communicate(timeout=12)
            self.assertEqual(proc.returncode, 143, output)
            # Beyond the old creator delay: a late container would now be visible.
            time.sleep(1.7)
            self.assertIsNone(container(name))
            pid = int(child_pid.read_text())
            stat = Path(f"/proc/{pid}/stat")
            if stat.exists():
                self.assertEqual(stat.read_text().rsplit(")", 1)[1].split()[0], "Z", "creator child is still running")
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.communicate(timeout=12)

    def test_pre_ready_inflight_create_is_observed_after_creator_reap(self):
        # Model a create request completing during client cancellation. Cleanup
        # before stopping the creator deterministically misses this container.
        name = self.owned_name()
        marker = self.base / "barrier"
        created = self.base / "created"
        script = self.base / "inflight.sh"
        script.write_text('''#!/usr/bin/env bash
set -e
marker=$1 created=$2
shift 2
trap '"$@" > "$created"; exit 143' TERM
printf ready > "$marker"
sleep 25 &
wait
''')
        create = ["docker", "create", "--name", name, "--label", f"{LABEL}={self.token}",
                  IMAGE, "node", "-e", "setTimeout(()=>{},25000)"]
        proc = self.guard(name, ["bash", str(script), str(marker), str(created), *create])
        try:
            wait_until(marker.exists)
            self.assertIsNone(container(name))
            os.kill(proc.pid, signal.SIGTERM)
            output, _ = proc.communicate(timeout=12)
            self.assertEqual(proc.returncode, 143, output)
            self.assertTrue(created.exists() and created.read_text().strip(), output)
            self.assertIsNone(container(name), "in-flight create completed after the final cleanup")
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.communicate(timeout=12)

    def test_cleanup_verification_failure_is_visible(self):
        name = self.owned_name()
        fake = self.base / "fake-bin"
        fake.mkdir()
        failed = self.base / "daemon-failed"
        docker = fake / "docker"
        docker.write_text('#!/usr/bin/env bash\n[[ ! -f "$FAKE_FAILURE" ]] || exit 99\nexit 0\n')
        docker.chmod(0o755)
        e = env()
        e["PATH"] = str(fake) + os.pathsep + e["PATH"]
        e["FAKE_FAILURE"] = str(failed)
        result = run(str(ENGINE), "internal", "guard", name, self.token, "--",
                     "touch", str(failed), env=e, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot verify cleanup", result.stderr.lower())

    def test_make_cancellation_reaches_guard_and_own_processes(self):
        write_fixture(self.root)
        self.make("setup")
        (self.root / "apps/main/SLOW").write_text("slow")
        project = "devkit-" + hashlib.sha256(str(self.root.resolve()).encode()).hexdigest()[:16]
        for sig, group in ((signal.SIGINT, True), (signal.SIGTERM, True), (signal.SIGTERM, False)):
            with self.subTest(signal=sig, group=group):
                proc = subprocess.Popen(["make", "--no-print-directory", "-C", str(self.root), "test"],
                    env=env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
                name = None
                try:
                    def ready():
                        nonlocal name
                        names = run("docker", "container", "ls", "--filter", "label=io.docouno.devkit.checkout=" + project,
                                    "--filter", "label=io.docouno.devkit.role=tool",
                                    "--format", "{{.Names}}").stdout.splitlines()
                        if names:
                            name = names[0]
                            if name not in self.resources:
                                self.resources.append(name)
                                self.tokens[name] = container(name)["Config"]["Labels"][LABEL]
                            return "READY" in run("docker", "logs", name, check=False).stdout
                        return False
                    wait_until(ready)
                    pids = run("docker", "top", name, "-eo", "pid").stdout.splitlines()[1:]
                    stamps = {int(pid): Path(f"/proc/{pid.strip()}/stat").read_text().rsplit(")", 1)[1].split()[19] for pid in pids}
                    (os.killpg if group else os.kill)(proc.pid, sig)
                    output, _ = proc.communicate(timeout=12)
                    self.assertNotEqual(proc.returncode, 0, output)
                    self.assertIsNone(container(name), output)
                    for pid, stamp in stamps.items():
                        stat = Path(f"/proc/{pid}/stat")
                        self.assertFalse(stat.exists() and stat.read_text().rsplit(")", 1)[1].split()[19] == stamp,
                                         f"process survived cancellation: {pid}")
                finally:
                    if proc.poll() is None:
                        os.killpg(proc.pid, signal.SIGTERM)
                        proc.communicate(timeout=12)

    def test_dev_checkout_isolation_finite_logs_ports_and_preserved_volume(self):
        fixtures = []
        for index, parent in enumerate(("first", "parent with space")):
            root = self.base / parent / "checkout"
            write_fixture(root)
            project = "devkit-" + hashlib.sha256(str(root.resolve()).encode()).hexdigest()[:16]
            volume = f"devkit-runtime-test-{self.token}-data-{index}"
            name = project + "-main-1"
            self.resources.append(name)
            self.projects.append((project, root, volume))
            marker = "data-" + str(index)
            code = ("const fs=require('node:fs');if(!fs.existsSync('/state/value'))fs.writeFileSync('/state/value',"
                    + json.dumps(marker) + ");require('node:http').createServer((q,s)=>s.end('ok')).listen(3000,'0.0.0.0');console.log('READY')")
            compose = {"services": {"main": {"image": "${DEVKIT_NODE_IMAGE:?}", "init": True,
                "command": ["node", "-e", code], "ports": [{"target": 3000, "published": "0", "host_ip": "127.0.0.1"}],
                "labels": {LABEL: self.token, "io.docouno.devkit.checkout": "${DEVKIT_PROJECT_NAME:?}"},
                "volumes": [{"type": "volume", "source": "state", "target": "/state"}]}},
                "volumes": {"state": {"name": volume, "labels": {LABEL: self.token}}},
                "networks": {"default": {"labels": {LABEL: self.token}}}}
            (root / "compose.yaml").write_text(json.dumps(compose))
            # A project .env must not replace the accepted image at the boundary.
            (root / ".env").write_text("DEVKIT_NODE_IMAGE=nonexistent:should-never-pull\n")
            run("make", "--no-print-directory", "-C", str(root), "setup")
            run("make", "--no-print-directory", "-C", str(root), "up")
            fixtures.append((root, name, volume, marker))
        a, b = fixtures
        orphan = self.owned_name()
        run("docker", "run", "--detach", "--pull", "never", "--name", orphan,
            "--label", f"{LABEL}={self.token}", "--label", f"com.docker.compose.project={self.projects[0][0]}",
            "--label", "com.docker.compose.service=unmanaged-orphan", IMAGE,
            "node", "-e", "setTimeout(()=>{},25000)")
        before = container(b[1])["Id"]
        status = run("make", "--no-print-directory", "-C", str(a[0]), "status")
        self.assertIn("127.0.0.1:", status.stdout)
        logs = run("make", "--no-print-directory", "-C", str(a[0]), "logs", "tail=1")
        self.assertIn("READY", logs.stdout)
        run("make", "--no-print-directory", "-C", str(a[0]), "down")
        self.assertIsNone(container(a[1]))
        self.assertTrue(container(orphan)["State"]["Running"], "down removed an unmanaged orphan")
        self.assertEqual(container(b[1])["Id"], before)
        self.assertTrue(container(b[1])["State"]["Running"])
        self.assertEqual(run("docker", "volume", "inspect", a[2]).returncode, 0)
        run("make", "--no-print-directory", "-C", str(a[0]), "up")
        self.assertEqual(run("docker", "exec", a[1], "node", "-e",
                             "process.stdout.write(require('node:fs').readFileSync('/state/value','utf8'))").stdout, a[3])

    def test_guard_refuses_existing_name_without_deleting_it(self):
        name = self.owned_name()
        run("docker", "run", "--pull", "never", "--detach", "--name", name,
            "--label", f"{LABEL}={self.token}", IMAGE, "node", "-e", "setTimeout(()=>{},25000)")
        before = container(name)["Id"]
        proc = self.guard(name, ["true"])
        output, _ = proc.communicate(timeout=8)
        self.assertNotEqual(proc.returncode, 0, output)
        self.assertEqual(container(name)["Id"], before)

    def test_zero_app_library_repository_and_package_library_mount_isolation(self):
        write_fixture(self.root, apps=(), packages=("lib",), package_profiles={"packages/lib": "ts-library"})
        lib_dir = self.root / "packages/lib"
        pkg_path = lib_dir / "package.json"
        pkg = json.loads(pkg_path.read_text())
        pkg["files"] = ["dist"]
        pkg["scripts"]["package"] = "node gate-pack.cjs"
        pkg_path.write_text(json.dumps(pkg, indent=2) + "\n")
        (lib_dir / "gate-pack.cjs").write_text(r'''
const fs = require('node:fs');
for (const ro of ['package.json', 'dist/index.js', '../../node_modules/probe-ro']) {
  let blocked = false;
  try { fs.writeFileSync(ro, 'ILLEGAL'); }
  catch (err) { if (['EROFS', 'EACCES'].includes(err.code)) blocked = true; else throw err; }
  if (!blocked) throw new Error('expected read-only mount during package task: ' + ro);
}
const manifest = JSON.parse(fs.readFileSync('package.json', 'utf8'));
const slug = manifest.name.replace(/^@/, '').replace(/\//g, '-');
const filename = `${slug}-${manifest.version}.tgz`;
fs.writeFileSync(`release/${filename}`, 'TARBALL_FIXTURE\n');
fs.writeFileSync('release/package-receipt.json', JSON.stringify({
  name: manifest.name,
  version: manifest.version,
  filename
}, null, 2) + '\n');
console.log('PACKED ' + filename);
''')
        accept_runtime(self.root, native=True, package_profiles={"packages/lib": "ts-library"})

        self.make("setup")
        for target in ("check", "test", "build"):
            self.assertIn(f"GATE lib {target}", self.make(target).stdout)
        for service_target in ("status", "logs", "down"):
            self.assertEqual(self.make(service_target).returncode, 0)

        for dev_target in ("dev", "up"):
            failed_dev = self.make(dev_target, check=False)
            self.assertNotEqual(failed_dev.returncode, 0)
            self.assertIn("Project has no runnable apps; add a playground or app with devkit add-app",
                          failed_dev.stderr)

        no_pkg = self.make("package", check=False)
        self.assertNotEqual(no_pkg.returncode, 0)
        self.assertIn('No packageable components found; set "private": false in packages/lib/package.json and run devkit sync',
                      no_pkg.stderr)
        priv_pkg = self.make("package", "package=lib", check=False)
        self.assertNotEqual(priv_pkg.returncode, 0)
        self.assertIn('Package packages/lib has "private": true in package.json; set "private": false and run devkit sync before packaging',
                      priv_pkg.stderr)

        # Switch to publishable mode ("private": false), bump version via devkit exec --write, setup and package.
        pkg["name"] = "@corp/sdk"
        pkg["private"] = False
        pkg_path.write_text(json.dumps(pkg, indent=2) + "\n")
        accept_runtime(self.root, native=True, package_profiles={"packages/lib": "ts-library"})
        self.make("setup")
        run(str(ENGINE), "exec", "--root", str(self.root), "--component", "packages/lib", "--write",
            "--", "npm", "version", "0.2.0", "--no-git-tag-version")
        self.assertEqual(json.loads(pkg_path.read_text())["version"], "0.2.0")
        self.assertIn("dependencies are absent/stale", self.make("package", check=False).stderr)
        self.make("setup")
        packed = self.make("package")
        self.assertIn("PACKED corp-sdk-0.2.0.tgz", packed.stdout)
        self.assertTrue((lib_dir / "release/corp-sdk-0.2.0.tgz").is_file())
        receipt = json.loads((lib_dir / "release/package-receipt.json").read_text())
        self.assertEqual(receipt["name"], "@corp/sdk")
        self.assertEqual(receipt["version"], "0.2.0")

    def test_private_gitlab_https_san_registry_with_custom_ca_and_token_isolation(self):
        secret_token = "glpat-runtime-secret-token-777"
        reg_dir = self.base / "gitlab-registry"
        pkg_src = self.base / "token-lib-src"
        certs_dir = self.base / "certs"
        for d in (reg_dir, pkg_src, certs_dir):
            d.mkdir(parents=True)

        # Build authentic @corp/token-lib@1.0.0 tarball via pinned Node image.
        (pkg_src / "package.json").write_text(json.dumps({
            "name": "@corp/token-lib",
            "version": "1.0.0",
            "main": "./index.cjs",
            "exports": {".": "./index.cjs"}
        }, indent=2) + "\n")
        (pkg_src / "index.cjs").write_text("module.exports = { value: 'from-private-gitlab-registry' };\n")
        fixture_tool(pkg_src, (
            "import {spawnSync} from 'node:child_process';"
            "const r=spawnSync('npm',['pack','--json','--pack-destination','/fixture'],{stdio:'inherit'});"
            "process.exit(r.status??1)"
        ))
        tarball_path = pkg_src / "corp-token-lib-1.0.0.tgz"
        tarball_bytes = tarball_path.read_bytes()
        shutil.copy2(tarball_path, reg_dir / "corp-token-lib-1.0.0.tgz")
        import base64
        tarball_shasum = hashlib.sha1(tarball_bytes).hexdigest()
        tarball_integrity = "sha512-" + base64.b64encode(hashlib.sha512(tarball_bytes).digest()).decode()

        # Generate Root CA first, launch container to discover its exact bridge IP, then sign SAN server cert.
        ca_key = certs_dir / "ca.key"
        ca_pem = certs_dir / "ca.pem"
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(ca_key), "-out", str(ca_pem), "-days", "2",
            "-subj", "/CN=Devkit Test Root CA")

        (reg_dir / "server.cjs").write_text(r'''
const fs = require('node:fs');
const https = require('node:https');

const expectedAuth = 'Bearer ' + process.env.EXPECTED_TOKEN;
const prefix = '/api/v4/projects/7/packages/npm';
const tarball = fs.readFileSync('/registry/corp-token-lib-1.0.0.tgz');

function waitForCerts() {
  if (!fs.existsSync('/registry/server.pem') || !fs.existsSync('/registry/server.key')) {
    setTimeout(waitForCerts, 30);
    return;
  }
  const server = https.createServer({
    cert: fs.readFileSync('/registry/server.pem'),
    key: fs.readFileSync('/registry/server.key'),
  }, (req, res) => {
    if (req.headers.authorization !== expectedAuth) {
      res.writeHead(401, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: 'unauthorized' }));
      return;
    }
    const baseUrl = `https://${req.headers.host}${prefix}`;
    const url = req.url.split('?')[0];
    if (url === `${prefix}/@corp%2ftoken-lib` || url === `${prefix}/@corp%2Ftoken-lib` || url === `${prefix}/@corp/token-lib`) {
      const tarballUrl = `${baseUrl}/@corp/token-lib/-/@corp/token-lib-1.0.0.tgz`;
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({
        name: '@corp/token-lib',
        'dist-tags': { latest: '1.0.0' },
        versions: {
          '1.0.0': {
            name: '@corp/token-lib',
            version: '1.0.0',
            main: './index.cjs',
            dist: {
              tarball: tarballUrl,
              shasum: process.env.TARBALL_SHASUM,
              integrity: process.env.TARBALL_INTEGRITY,
            },
          },
        },
      }));
      return;
    }
    if (url === `${prefix}/@corp/token-lib/-/@corp/token-lib-1.0.0.tgz`) {
      res.writeHead(200, { 'content-type': 'application/octet-stream' });
      res.end(tarball);
      return;
    }
    res.writeHead(404);
    res.end('not found: ' + url);
  });
  server.listen(8443, '0.0.0.0', () => console.log('GITLAB_REGISTRY_READY'));
}
waitForCerts();
''')

        reg_name = self.owned_name()
        run("docker", "run", "--detach", "--pull", "never", "--name", reg_name,
            "--label", f"{LABEL}={self.token}", "--network", "bridge",
            "--user", f"{os.getuid()}:{os.getgid()}",
            "--volume", f"{reg_dir}:/registry:ro,z",
            "--env", f"EXPECTED_TOKEN={secret_token}",
            "--env", f"TARBALL_SHASUM={tarball_shasum}",
            "--env", f"TARBALL_INTEGRITY={tarball_integrity}",
            IMAGE, "node", "/registry/server.cjs")
        bridge_ip = container(reg_name)["NetworkSettings"]["Networks"]["bridge"]["IPAddress"]
        self.assertTrue(bridge_ip, "missing bridge IP for private registry container")

        srv_key = certs_dir / "server.key"
        srv_csr = certs_dir / "server.csr"
        ext_cfg = certs_dir / "san.cnf"
        ext_cfg.write_text(f"subjectAltName=IP:{bridge_ip},DNS:gitlab.registry.test\nextendedKeyUsage=serverAuth\n")
        run("openssl", "req", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(srv_key), "-out", str(srv_csr),
            "-subj", f"/CN={bridge_ip}")
        srv_pem_tmp = certs_dir / "server.pem"
        run("openssl", "x509", "-req", "-in", str(srv_csr),
            "-CA", str(ca_pem), "-CAkey", str(ca_key), "-CAcreateserial",
            "-out", str(srv_pem_tmp), "-days", "2", "-extfile", str(ext_cfg))
        shutil.copy2(srv_key, reg_dir / "server.key")
        shutil.copy2(srv_pem_tmp, reg_dir / "server.pem")
        wait_until(lambda: "GITLAB_REGISTRY_READY" in run("docker", "logs", reg_name, check=False).stdout)

        # Build project with BOTH Node (apps/main) and Rust (apps/api) components.
        write_fixture(self.root, apps=("main",))
        shutil.copytree(REPO / "templates/rust-api", self.root / "apps/api", ignore=shutil.ignore_patterns("target"))
        (self.root / "apps/api/tests/secret_isolation.rs").write_text(
            "#[test]\n"
            "fn private_npm_tokens_and_ca_never_enter_rust_containers() {\n"
            "    assert!(\n"
            '        std::env::var("GITLAB_NPM_TOKEN").is_err(),\n'
            '        "GITLAB_NPM_TOKEN leaked to Rust container"\n'
            "    );\n"
            "    assert!(\n"
            '        std::env::var("NODE_EXTRA_CA_CERTS").is_err(),\n'
            '        "NODE_EXTRA_CA_CERTS leaked to Rust container"\n'
            "    );\n"
            "    assert!(\n"
            '        !std::path::Path::new("/devkit/ca.pem").exists(),\n'
            '        "/devkit/ca.pem mounted in Rust container"\n'
            "    );\n"
            "}\n"
        )
        (self.root / "apps/main/gate.cjs").write_text(
            (self.root / "apps/main/gate.cjs").read_text() + r'''
if (process.env.GITLAB_NPM_TOKEN !== 'devkit-offline-unused') {
  throw new Error('offline Node task saw unexpected GITLAB_NPM_TOKEN: ' + process.env.GITLAB_NPM_TOKEN);
}
if (process.env.NODE_EXTRA_CA_CERTS || fs.existsSync('/devkit/ca.pem')) {
  throw new Error('offline Node task mounted CA cert unexpectedly');
}
const priv = require('@corp/token-lib');
if (priv.value !== 'from-private-gitlab-registry') {
  throw new Error('unexpected @corp/token-lib payload');
}
''')
        registry_url = f"https://{bridge_ip}:8443/api/v4/projects/7/packages/npm/"
        nerf = f"//{bridge_ip}:8443/api/v4/projects/7/packages/npm/"
        (self.root / ".npmrc").write_text(
            f"@corp:registry={registry_url}\n"
            f"{nerf}:_authToken=${{GITLAB_NPM_TOKEN}}\n"
            f"{nerf}:always-auth=true\n"
        )
        accept_runtime(self.root, native=True)

        # 1. Missing GITLAB_NPM_TOKEN fails closed before container launch.
        no_tok_env = env()
        no_tok_env["DEVKIT_CA_CERT"] = str(ca_pem)
        missing_tok = run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main",
                          "--write", "--network", "bridge", "--prepare", "--",
                          "npm", "install", "@corp/token-lib@1.0.0", "--save-exact",
                          env=no_tok_env, check=False, timeout=90)
        self.assertNotEqual(missing_tok.returncode, 0)
        self.assertIn("Missing environment variable GITLAB_NPM_TOKEN required by .npmrc", missing_tok.stderr)

        # 2. Missing DEVKIT_CA_CERT fails TLS verification (strict-ssl remains enforced).
        no_ca_env = env()
        no_ca_env["GITLAB_NPM_TOKEN"] = secret_token
        missing_ca = run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main",
                         "--write", "--network", "bridge", "--prepare", "--",
                         "npm", "install", "@corp/token-lib@1.0.0", "--save-exact",
                         env=no_ca_env, check=False, timeout=90)
        self.assertNotEqual(missing_ca.returncode, 0)

        # 3. With DEVKIT_CA_CERT + GITLAB_NPM_TOKEN, install & lock @corp/token-lib, then run full setup + offline gates.
        auth_env = env()
        auth_env["DEVKIT_CA_CERT"] = str(ca_pem)
        auth_env["GITLAB_NPM_TOKEN"] = secret_token
        run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main",
            "--write", "--network", "bridge", "--prepare", "--",
            "npm", "install", "@corp/token-lib@1.0.0", "--save-exact", "--ignore-scripts",
            env=auth_env, timeout=120)
        accept_runtime(self.root, native=False)

        self.make("setup", env=auth_env, timeout=240)
        for target in ("check", "test", "build"):
            out = self.make(target, env=auth_env, timeout=240).stdout
            self.assertIn(f"GATE main {target}", out)

        # 4. Verify direct .devkit/runtime/devkit.sh exec CLI forwarding & component .npmrc rejection.
        sh_exec = run("sh", str(self.root / ".devkit/runtime/devkit.sh"), "exec", "--component", "apps/main",
                      "--", "node", "-e", "process.stdout.write('SH_EXEC_OK')", env=auth_env)
        self.assertEqual(sh_exec.stdout, "SH_EXEC_OK")

        comp_npmrc = self.root / "apps/main/.npmrc"
        comp_npmrc.write_text("strict-ssl=false\n")
        try:
            bad_exec = run(str(ENGINE), "exec", "--root", str(self.root), "--component", "apps/main",
                           "--", "node", "-v", env=auth_env, check=False)
            self.assertNotEqual(bad_exec.returncode, 0)
            self.assertIn("workspace context: apps/main/.npmrc", bad_exec.stderr)
        finally:
            comp_npmrc.unlink(missing_ok=True)

        # 5. Verify secret_token is absent from ALL files under checkout (including package-lock.json, .npmrc, .devkit, node_modules).
        lock_text = (self.root / "package-lock.json").read_text()
        self.assertIn(f"{registry_url}@corp/token-lib/-/@corp/token-lib-1.0.0.tgz", lock_text)
        for file_path in self.root.rglob("*"):
            if file_path.is_file() and not file_path.is_symlink():
                data = file_path.read_bytes()
                self.assertNotIn(secret_token.encode(), data, f"Secret token leaked into {file_path}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
