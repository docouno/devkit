#!/usr/bin/env python3
"""Production entrypoint tests. Only exact, labelled disposable Docker resources."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
import uuid

REPO = Path(__file__).resolve().parents[2]
IMAGE = "node:24.18.0-bookworm-slim@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d"
LABEL = "io.docouno.devkit.invocation"


def env():
    result = dict(os.environ)
    for key in list(result):
        if key.startswith(("COMPOSE_", "DEVKIT_")) or key in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL"):
            result.pop(key)
    return result


def run(*args, check=True, timeout=35, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout,
                            env=kwargs.pop("env", env()), **kwargs)
    if check and result.returncode:
        raise AssertionError(f"{args!r}: {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


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


def write_fixture(root, apps=("main",)):
    runtime = root / ".devkit/runtime"
    runtime.mkdir(parents=True)
    for name in ("devkit.mk", "devkit.sh", "guard.sh", "compose.tools.yaml", "verify-manifest.mjs"):
        shutil.copy2(REPO / "runtime" / name, runtime / name)
    (runtime / "toolchains.env").write_text(
        f"DEVKIT_VERSION=0.1.0\nDEVKIT_NODE_IMAGE={IMAGE}\nDEVKIT_NODE_VERSION=24.18.0\n"
        f"DEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={'0' * 64}\n")
    (root / "Makefile").write_text("DEVKIT_APPS := " + " ".join(apps) + "\ninclude .devkit/runtime/devkit.mk\n")
    for app in apps:
        path = root / "apps" / app
        path.mkdir(parents=True)
        package = {"name": f"devkit-test-{app}", "version": "1.0.0", "private": True,
                   "scripts": {name: f"node gate.cjs {name}" for name in ("check", "fix", "test", "build", "dev")}}
        (path / "package.json").write_text(json.dumps(package) + "\n")
        (path / "package-lock.json").write_text(json.dumps({"name": package["name"], "version": "1.0.0",
            "lockfileVersion": 3, "packages": {"": {"name": package["name"], "version": "1.0.0"}}}) + "\n")
        (path / "untracked.txt").write_text("untracked source\n")
        (path / "gate.cjs").write_text(r'''
const fs = require('node:fs');
const path = require('node:path');
if (process.env.DEVKIT_TEST_SECRET) throw Error('host secret leaked');
if (fs.existsSync('/var/run/docker.sock')) throw Error('Docker socket leaked');
if (!process.cwd().startsWith('/workspace/apps/')) throw Error('wrong app cwd');
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
fs.writeFileSync('node_modules/task-output', action);
if (fs.existsSync('FAIL')) process.exit(42);
console.log('GATE ' + path.basename(process.cwd()) + ' ' + action);
if (fs.existsSync('SLOW')) {
  require('node:child_process').spawn(process.execPath, ['-e', 'setTimeout(()=>{},25000)'], {stdio:'ignore'});
  console.log('READY');
  setTimeout(()=>{},25000);
}
''')
    accept_runtime(root)


def accept_runtime(root):
    def identity(path):
        return {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "mode": path.stat().st_mode & 0o777}
    managed = {str(path.relative_to(root)): identity(path)
               for path in sorted((root / ".devkit/runtime").rglob("*")) if path.is_file()}
    apps = {}
    for path in sorted((root / "apps").iterdir()):
        rust = (path / "Cargo.toml").exists()
        apps[path.name] = {"kind": "rust" if rust else "node", "template": "rust-app" if rust else "node-app",
                           "toolDependencies": {} if rust else json.loads((path / "package.json").read_text()).get("devDependencies", {})}
        if rust:
            pin = path / "rust-toolchain.toml"
            managed[str(pin.relative_to(root))] = identity(pin)
    checksum = root / ".devkit/runtime.sha256"
    checksum.write_text("".join(f"{value['sha256']}  {name}\n" for name, value in sorted(managed.items())))
    managed[str(checksum.relative_to(root))] = identity(checksum)
    state_path = root / ".devkit/manifest.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {
        "schemaVersion": 1, "version": "0.1.0", "generation": str(uuid.uuid4()), "packageMode": "registry"}
    # This is a disposable release/state fixture, not a registry-consumption proof.
    state.update({"releaseIdentity": hashlib.sha256(json.dumps(managed, sort_keys=True).encode()).hexdigest(),
                  "managed": managed, "apps": apps})
    state_path.write_text(json.dumps(state) + "\n")


class RuntimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        run("docker", "image", "inspect", IMAGE)
        run("docker", "compose", "version")

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
                  for path in (source, source.with_name("package.json"), source.with_name("package-lock.json"))}
        for command in ("check", "test", "build"):
            self.assertIn(f"GATE main {command}", self.make(command, env=e).stdout)
        self.assertEqual(hashes, {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in hashes})
        self.make("fix")
        self.assertEqual(source.read_text(), "explicit fix\n")
        lock = source.with_name("package-lock.json")
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
        self.assertIn("Missing native npm script: test", result.stderr)
        self.assertFalse((self.root / ".devkit/state/node-main.lock").exists())
        pins = self.root / ".devkit/runtime/toolchains.env"
        pins.write_text(pins.read_text().replace(IMAGE, "node:missing@sha256:" + "a" * 64))
        accept_runtime(self.root)
        result = self.make("check", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("accepted Node image is absent; run make setup", result.stderr)

    def test_rust_derived_image_state_and_native_gates(self):
        write_fixture(self.root)
        shutil.rmtree(self.root / "apps/main")
        shutil.copytree(REPO / "templates/rust-app", self.root / "apps/main",
                        ignore=shutil.ignore_patterns("target"))
        runtime = self.root / ".devkit/runtime"
        for name in ("rust.Dockerfile", "rust-check.sh"):
            shutil.copy2(REPO / "runtime" / name, runtime / name)
        recipe = hashlib.sha256((runtime / "rust.Dockerfile").read_bytes()).hexdigest()
        pins = runtime / "toolchains.env"
        pins.write_text(pins.read_text().replace("0" * 64, recipe))
        accept_runtime(self.root)
        result = self.make("check", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("derived Rust image is absent/stale; run make setup", result.stderr)
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

    def test_reject_registration_pending_and_lock_contention(self):
        write_fixture(self.root)
        makefile = self.root / "Makefile"
        accepted_makefile = makefile.read_text()
        makefile.write_text("DEVKIT_APPS :=\ninclude .devkit/runtime/devkit.mk\n")
        self.assertIn("register at least one app", self.make("check", check=False).stderr)
        makefile.write_text(accepted_makefile)
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
            self.assertIn("another devkit operation", self.make("doctor", check=False).stderr)
        (self.root / "apps/main/Cargo.toml").write_text("[package]\n")
        self.assertIn("exactly one supported", self.make("doctor", check=False).stderr)
        (self.root / ".devkit/runtime/toolchains.env").write_text("DEVKIT_VERSION=tampered\n")
        self.assertIn("managed runtime differs", self.make("doctor", check=False).stderr)

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
        directories = [".devkit", ".devkit/state", ".devkit/cache", ".devkit/cache/npm", ".devkit/cache/cargo",
                       "apps/main/node_modules", "apps/main/dist", "apps/main/target"]
        files = [".devkit/state/task.lock", ".devkit/state/node-main.lock", ".devkit/state/node-main.lock.tmp", ".devkit/state/rust-image"]
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
                    self.assertIn("symlink runtime path", result.stderr)
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
        self.assertIn("expected runtime directory", self.make("check", check=False).stderr)
        self.assertEqual(dist.read_text(), "DO NOT REPLACE\n")
        dist.unlink()
        dist.mkdir()
        lock = self.root / ".devkit/state/task.lock"
        lock.unlink()
        os.mkfifo(lock)
        self.assertIn("expected regular runtime file", self.make("doctor", check=False).stderr)

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
        self.make("setup")
        receipt = self.root / ".devkit/state/node-main.lock"
        self.assertTrue(receipt.is_file())
        before = package_path.read_bytes(), package_path.with_name("package-lock.json").read_bytes()
        (self.root / "apps/main/FAIL_SETUP").touch()
        result = self.make("setup", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.root / "apps/main/node_modules/install-mutated").is_file())
        self.assertFalse(receipt.exists())
        self.assertEqual(before, (package_path.read_bytes(), package_path.with_name("package-lock.json").read_bytes()))
        self.assertIn("dependencies are absent/stale", self.make("check", check=False).stderr)

    def test_cancelled_repeat_setup_invalidates_receipt(self):
        write_fixture(self.root)
        package_path = self.root / "apps/main/package.json"
        package = json.loads(package_path.read_text())
        package["scripts"]["postinstall"] = 'node -e "if(require(\'node:fs\').existsSync(\'SLOW_SETUP\')){console.log(\'READY_SETUP\');setTimeout(()=>{},25000)}"'
        package_path.write_text(json.dumps(package))
        self.make("setup")
        receipt = self.root / ".devkit/state/node-main.lock"
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
                return "READY_SETUP" in run("docker", "logs", name, check=False).stdout
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
        guard = self.root / ".devkit/runtime/guard.sh"
        originals = {path: (path.read_bytes(), path.stat().st_mode & 0o777) for path in (manifest, checksum, guard)}
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
                        checksum.write_text("".join(line for line in checksum.read_text().splitlines(True) if not line.endswith("/guard.sh\n")))
                        if case == "omitted-and-changed":
                            guard.write_text(guard.read_text() + "\n# benign drift\n")
                            self.assertEqual(manifest.read_bytes(), originals[manifest][0])
                        else:
                            state["managed"][".devkit/runtime.sha256"]["sha256"] = hashlib.sha256(checksum.read_bytes()).hexdigest()
                            manifest.write_text(json.dumps(state))
                    elif case == "unlisted": extra.write_text("unlisted managed runtime input\n")
                    elif case == "managed-symlink-parent":
                        nested.rename(saved_parent)
                        nested.symlink_to(saved_parent, target_is_directory=True)
                    elif case == "mode": guard.chmod(0o755)
                    else:
                        if case == "unsafe-path": state["managed"]["../outside"] = {"sha256": "0" * 64, "mode": 0o644}
                        else: state[case] = {"version": "9.9.9", "schema": 2, "generation": "initial", "packageMode": "other", "releaseIdentity": "bad"}[case]
                        if case == "schema": state["schemaVersion"] = 2
                        manifest.write_text(json.dumps(state))
                    result = self.make("check", check=False)
                    self.assertNotEqual(result.returncode, 0, case)
                    self.assertNotIn("GATE main check", result.stdout)
                    self.assertTrue(any(message in result.stderr for message in ("managed file conflict", "runtime.sha256 omits", "unlisted or nonregular", "symlink in managed/input path", "version differs", "schemaVersion", "generation UUID", "packageMode", "releaseIdentity", "unsafe managed path")), result.stderr)
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
        self.assertIn("native app is not registered: extra", self.make("check", check=False).stderr)
        state["apps"]["extra"] = {"kind": "node", "template": "node-app", "toolDependencies": {}}
        manifest.write_text(json.dumps(state))
        self.assertIn("missing from DEVKIT_APPS: extra", self.make("check", check=False).stderr)
        makefile = self.root / "Makefile"
        makefile.write_text(makefile.read_text().replace("DEVKIT_APPS := main", "DEVKIT_APPS := main extra"))
        self.make("setup")
        result = self.make("check")
        self.assertIn("GATE main check", result.stdout)
        self.assertIn("GATE extra check", result.stdout)
        self.assertEqual(json.loads(manifest.read_text())["ownerExtension"], {"custom": "preserve"})

    def test_control_service_has_only_readonly_root_and_no_network_or_dependencies(self):
        write_fixture(self.root)
        helper = self.root / ".devkit/runtime/verify-manifest.mjs"
        helper.write_text("await new Promise(resolve => setTimeout(resolve, 25000));\n" + helper.read_text())
        accept_runtime(self.root)
        project = "devkit-" + hashlib.sha256(str(self.root.resolve()).encode()).hexdigest()[:16]
        proc = subprocess.Popen(["make", "--no-print-directory", "-C", str(self.root), "doctor"],
            env=env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
        name = None
        try:
            def ready():
                nonlocal name
                names = run("docker", "container", "ls", "--filter", "label=io.docouno.devkit.checkout=" + project,
                            "--filter", "label=io.docouno.devkit.role=control", "--format", "{{.Names}}").stdout.splitlines()
                if not names: return False
                name = names[0]
                found = container(name)
                if found is None: return False
                self.resources.append(name)
                self.tokens[name] = found["Config"]["Labels"][LABEL]
                return True
            wait_until(ready)
            found = container(name)
            self.assertEqual(found["HostConfig"]["NetworkMode"], "none")
            self.assertTrue(found["HostConfig"]["ReadonlyRootfs"])
            self.assertEqual([(mount["Destination"], mount["RW"]) for mount in found["Mounts"]], [("/workspace", False)])
            self.assertEqual(found["Config"]["User"], f"{os.getuid()}:{os.getgid()}")
            self.assertFalse((self.root / "apps/main/node_modules").exists())
            self.assertFalse((self.root / ".devkit/cache").exists())
            os.killpg(proc.pid, signal.SIGTERM)
            proc.communicate(timeout=12)
            self.assertNotEqual(proc.returncode, 0)
            self.assertIsNone(container(name))
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.communicate(timeout=12)

    def guard(self, name, argv):
        return subprocess.Popen(["bash", str(REPO / "runtime/guard.sh"), name, self.token, "--", *argv],
            env=env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)

    def task_argv(self, name, code):
        return ["docker", "run", "--pull", "never", "--rm", "--init", "--name", name,
                "--label", f"{LABEL}={self.token}", IMAGE, "node", "-e", code]

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
        result = run("bash", str(REPO / "runtime/guard.sh"), name, self.token, "--",
                     "touch", str(failed), env=e, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot verify cleanup", result.stderr)
        self.assertIn("cleanup incomplete", result.stderr)

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
            run("make", "--no-print-directory", "-C", str(root), "up")
            fixtures.append((root, name, volume, marker))
        a, b = fixtures
        before = container(b[1])["Id"]
        status = run("make", "--no-print-directory", "-C", str(a[0]), "status")
        self.assertIn("127.0.0.1:", status.stdout)
        logs = run("make", "--no-print-directory", "-C", str(a[0]), "logs", "tail=1")
        self.assertIn("READY", logs.stdout)
        run("make", "--no-print-directory", "-C", str(a[0]), "down")
        self.assertIsNone(container(a[1]))
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
