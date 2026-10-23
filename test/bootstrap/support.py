#!/usr/bin/env python3
"""Shared HTTP fixture for packaged bootstrap verification."""
from __future__ import annotations

import re

from hashlib import sha256
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Event, Thread
import io
import json
import os
import shutil
import signal
import subprocess
import tarfile
import tempfile
import time
import uuid


HERE = Path(__file__).resolve().parent
AUTHOR = HERE.parents[1]


def require(value, message):
    if not value:
        raise RuntimeError(message)


def release_version(path):
    require(path.is_file(), f"Missing release archive: {path}")
    with tarfile.open(path, "r:gz") as archive:
        manifests = [entry for entry in archive.getmembers() if re.fullmatch(r"devkit-[^/]+/release\.json", entry.name)]
        require(len(manifests) == 1 and manifests[0].isfile(), "Expected one release manifest")
        version = json.load(archive.extractfile(manifests[0]))["version"]
    require(isinstance(version, str) and re.fullmatch(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?", version), "Invalid fixture release version")
    require(Path(str(path) + ".sha256").read_text() == f"{sha256(path.read_bytes()).hexdigest()}  devkit-{version}.tar.gz\n", "Release fixture checksum differs")
    return version


class Harness:
    def __init__(self, release, next_release, bootstrap=None, *, render_template=False):
        self.release_path = Path(release).resolve()
        self.next_release_path = Path(next_release).resolve()
        self.version = release_version(self.release_path)
        self.next_version = release_version(self.next_release_path)
        self.modular = tuple(int(part) for part in self.version.split("-", 1)[0].split(".")) >= (0, 2, 0)
        require(self.version != self.next_version, "Provide two distinct release versions")
        self.bootstrap_version = (AUTHOR / "VERSION").read_text().strip()
        self.release_paths = {self.version: self.release_path, self.next_version: self.next_release_path}
        self.source_identities = {str(p): sha256(p.read_bytes()).hexdigest() for p in self.release_paths.values()}
        if not render_template:
            bootstrap = Path(bootstrap or AUTHOR / f".artifacts/devkit-{self.bootstrap_version}.sh").resolve()
            require(bootstrap.is_file(), f"Build the bootstrap first: {bootstrap}")
            self.script_bytes = bootstrap.read_bytes()
            baked_version = re.search(rb"^  bootstrap_version='([^']+)'$", self.script_bytes, re.MULTILINE)
            require(baked_version is not None, "Bootstrap does not declare its release version")
            self.bootstrap_version = baked_version[1].decode("ascii")
        template = (AUTHOR / "tooling/bootstrap.sh").read_text()
        pins = dict(line.split("=", 1) for line in (AUTHOR / "toolchains.env").read_text().splitlines())
        expected_script = template.replace("@@BOOTSTRAP_VERSION@@", self.bootstrap_version).replace("@@GUARD@@", (AUTHOR / "runtime/guard.sh").read_text()).replace(
            "@@NODE_IMAGE@@", pins["DEVKIT_NODE_IMAGE"]).replace("@@NODE_VERSION@@", pins["DEVKIT_NODE_VERSION"]).encode()
        require("@@" not in expected_script.decode(), "Bootstrap template has unresolved placeholders")
        if render_template:
            self.script_bytes = expected_script
        else:
            require(self.script_bytes == expected_script, "Built bootstrap differs from its current template, guard or pins")
            checksum = Path(str(bootstrap) + ".sha256")
            require(checksum.read_text() == f"{sha256(self.script_bytes).hexdigest()}  {bootstrap.name}\n", "Bootstrap checksum differs")
        self.token = uuid.uuid4().hex
        self.scratch = Path(tempfile.mkdtemp(prefix="devkit bootstrap test "))
        self.logs = AUTHOR / ".artifacts/bootstrap" / self.token
        self.logs.mkdir(parents=True)
        self.results = []
        self.processes = []
        self.requests = []
        self.routes = {}
        self.slow_started = Event()
        self.temp = self.scratch / "bootstrap temp"
        self.temp.mkdir()
        blockers = self.scratch / "without host languages"
        blockers.mkdir()
        self.forbidden = self.scratch / "host-language-attempts"
        for name in ("node", "npm", "npx", "cargo", "rustc", "rustup", "tsc", "eslint"):
            file = blockers / name
            file.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$DEVKIT_FORBIDDEN_HOST_SDK_LOG"\nexit 97\n')
            file.chmod(0o755)
        self.docker_log = self.scratch / "docker argv.jsonl"
        tar = shutil.which("tar")
        tar_wrapper = blockers / "tar"
        tar_wrapper.write_text("#!/usr/bin/python3\nimport os,sys,time\n"
                               "if os.environ.get('PROBE_PAUSE_EXTRACTION') == '1' and '--extract' in sys.argv:\n"
                               " open(os.environ['PROBE_EXTRACTION_STARTED'],'w').close()\n"
                               " while True: time.sleep(1)\n"
                               f"os.execv({tar!r},[{tar!r}]+sys.argv[1:])\n")
        tar_wrapper.chmod(0o755)
        docker = shutil.which("docker")
        # Capture exact resources created by this probe, never global docker state.
        docker_wrapper = blockers / "docker"
        docker_wrapper.write_text("#!/usr/bin/python3\nimport json,os,sys\n"
                                  "with open(os.environ['PROBE_DOCKER_LOG'],'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\n"
                                  f"os.execv({docker!r},[{docker!r}]+sys.argv[1:])\n")
        docker_wrapper.chmod(0o755)
        self.env = dict(os.environ)
        for key in list(self.env):
            if key.startswith(("COMPOSE_", "DEVKIT_", "npm_", "NPM_")) or key in ("NODE_OPTIONS", "NODE_PATH"):
                self.env.pop(key)
        self.env.update(PATH=str(blockers) + os.pathsep + os.environ["PATH"], TMPDIR=str(self.temp),
                        PROBE_DOCKER_LOG=str(self.docker_log), DEVKIT_FORBIDDEN_HOST_SDK_LOG=str(self.forbidden))
        for version, path in self.release_paths.items():
            filename = f"devkit-{version}.tar.gz"
            self.routes[f"/releases/v{version}/{filename}"] = path.read_bytes()
            self.routes[f"/releases/v{version}/{filename}.sha256"] = Path(str(path) + ".sha256").read_bytes()
        self.routes[f"/releases/v{self.bootstrap_version}/bootstrap.sh"] = self.script_bytes
        archive = self.routes[f"/releases/v{self.version}/devkit-{self.version}.tar.gz"]
        checksum = self.routes[f"/releases/v{self.version}/devkit-{self.version}.tar.gz.sha256"]
        for route in ("corrupt", "truncated", "slow"):
            self.routes[f"/{route}/v{self.version}/devkit-{self.version}.tar.gz"] = archive
            self.routes[f"/{route}/v{self.version}/devkit-{self.version}.tar.gz.sha256"] = checksum
        self.routes[f"/corrupt/v{self.version}/devkit-{self.version}.tar.gz"] = archive[:-1] + bytes([archive[-1] ^ 1])
        probe = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                probe.requests.append(self.path)
                body = probe.routes.get(self.path)
                if body is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                try:
                    if self.path.startswith("/truncated/") and self.path.endswith(".tar.gz"):
                        self.wfile.write(body[:100])
                        self.close_connection = True
                    elif self.path.startswith("/slow/") and self.path.endswith(".tar.gz"):
                        probe.slow_started.set()
                        for offset in range(0, len(body), 100):
                            self.wfile.write(body[offset:offset + 100])
                            self.wfile.flush()
                            time.sleep(0.03)
                    else:
                        self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.script = self.scratch / "downloaded bootstrap.sh"

    def record(self, name, code, output, expect=None, **extra):
        passed = code == 0 if expect is None else code != 0 and expect in output
        path = self.logs / f"{len(self.results)+1:02d}-{name}.log"
        path.write_text(output)
        self.results.append(dict(name=name, exitCode=code, passed=passed, expectedFailure=expect,
                                 log=path.name, **extra))
        print(("PASS " if passed else "FAIL ") + name, flush=True)
        if not passed:
            raise RuntimeError(f"{name}: code={code}; expected={expect!r}\n{output[-5000:]}")
        if list(self.temp.iterdir()):
            raise RuntimeError(f"{name}: bootstrap staging leaked: {list(self.temp.iterdir())}")

    def spawn(self, arguments, **kwargs):
        kwargs.setdefault("start_new_session", True)
        process = subprocess.Popen(arguments, **kwargs)
        self.processes.append(process)
        return process

    def run(self, name, arguments, expect=None):
        before = len(self.requests)
        process = self.spawn([str(x) for x in arguments], env=self.env, cwd=self.scratch,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                   start_new_session=True)
        try:
            output, _ = process.communicate(timeout=600)
        except BaseException:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                output, _ = process.communicate(timeout=20)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                output, _ = process.communicate()
            (self.logs / f"interrupted-{name}.log").write_text(output)
            raise
        self.record(name, process.returncode, output, expect, requests=self.requests[before:])
        return output

    def boot(self, name, *arguments, route="releases", expect=None):
        return self.run(name, ["bash", self.script, *arguments, "--release-base-url", self.base + "/" + route], expect)

    def attack(self, name, members):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz", format=tarfile.USTAR_FORMAT) as archive:
            for path, kind, mode in members:
                info = tarfile.TarInfo(path)
                info.mode = mode
                if kind == "directory":
                    info.type = tarfile.DIRTYPE
                elif kind == "symlink":
                    info.type = tarfile.SYMTYPE
                    info.linkname = "/tmp"
                elif kind == "hardlink":
                    info.type = tarfile.LNKTYPE
                    info.linkname = f"devkit-{self.version}/a"
                elif kind == "fifo":
                    info.type = tarfile.FIFOTYPE
                archive.addfile(info)
        body = buffer.getvalue()
        base = f"/attack-{name}/v{self.version}/devkit-{self.version}.tar.gz"
        self.routes[base] = body
        self.routes[base + ".sha256"] = f"{sha256(body).hexdigest()}  devkit-{self.version}.tar.gz\n".encode()
        self.boot(name, "init", self.scratch / "must not exist", "--version", self.version, route="attack-" + name,
                  expect="devkit bootstrap:")

    def wrong_embedded_version(self):
        buffer = io.BytesIO()
        # Synthetic three-file fixture: no existing release is unpacked or repacked.
        with tarfile.open(fileobj=buffer, mode="w:gz", format=tarfile.USTAR_FORMAT) as archive:
            directory = tarfile.TarInfo(f"devkit-{self.version}/")
            directory.type = tarfile.DIRTYPE
            directory.mode = 0o755
            archive.addfile(directory)
            for name, data, mode in (("devkit", b"#!/bin/sh\nexit 98\n", 0o755),
                                     ("release.json", json.dumps({"schemaVersion": 1, "version": self.next_version}).encode(), 0o644)):
                info = tarfile.TarInfo(f"devkit-{self.version}/" + name)
                info.mode = mode
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        body = buffer.getvalue()
        path = f"/wrong-version/v{self.version}/devkit-{self.version}.tar.gz"
        self.routes[path] = body
        self.routes[path + ".sha256"] = f"{sha256(body).hexdigest()}  devkit-{self.version}.tar.gz\n".encode()
        self.boot("wrong-embedded-version", "init", self.scratch / "wrong version", "--version", self.version, route="wrong-version",
                  expect="archive release version differs from requested version")

    def execute(self):
        self.run("download-bootstrap-via-curl", ["curl", "-q", "--fail", "--silent", "--show-error", "--output",
                                                  self.script, self.base + f"/releases/v{self.bootstrap_version}/bootstrap.sh"])
        self.run("shell-syntax", ["bash", "-n", self.script])
        help_text = self.run("help", ["bash", self.script, "--help"])
        require(self.bootstrap_version in help_text, "Help omitted the bootstrap release version")
        product = self.scratch / "product with spaces and $(touch injected)"
        init_options = ["--app", "api=rust-api", "--app", "batch=rust-api"] if self.modular else ["--template", "rust"]
        self.boot("http-init-selected-version", "init", product, "--version", self.version, *init_options, "--package-source", "local")
        state = json.loads((product / ".devkit/manifest.json").read_text())
        require(state["version"] == self.version, "Init accepted an unexpected release version")
        require(not self.modular or state["schemaVersion"] >= 2, "A modular release generated an old manifest schema")
        require(not self.modular or (product / "apps/batch/Cargo.toml").is_file(), "Repeated --app did not reach the factory")
        assert not (self.scratch / "injected").exists()
        source = product / "apps/api/src/main.rs"
        source.write_text(source.read_text() + "\n// Preserve this product source.\n")
        (product / ".env").write_text("PRODUCT_SENTINEL=preserve\n")
        preserved = {p: p.read_bytes() for p in (source, product / ".env", product / "Makefile", product / "compose.yaml")}
        if self.modular:
            self.boot("http-add-app-accepted-version", "add-app", product, "--name", "background", "--template", "rust-api")
            require(all(f"v{self.version}/" in path for path in self.results[-1]["requests"]), "Add-app did not use accepted release")
            self.boot("http-add-package-accepted-version", "add-package", product, "--name", "client", "--template", "react-client")
            require(all(f"v{self.version}/" in path for path in self.results[-1]["requests"]), "Add-package did not use accepted release")
            require((product / "apps/background/Cargo.toml").is_file(), "Add-app omitted Rust application")
            require((product / "packages/client/package.json").is_file(), "Add-package omitted shared package")
            require(source.read_bytes() == preserved[source] and (product / ".env").read_bytes() == preserved[product / ".env"], "Adding a component changed product files")
            preserved = {p: p.read_bytes() for p in preserved}
        self.boot("http-update-selected-version", "update", product, "--version", self.next_version)
        assert json.loads((product / ".devkit/manifest.json").read_text())["version"] == self.next_version
        assert all(p.read_bytes() == data for p, data in preserved.items())
        self.boot("recover-needs-explicit-version", "recover", product, expect="recover requires explicit --version")
        self.boot("http-recover-no-pending", "recover", product, "--version", self.next_version)
        shutil.copytree(product / "apps/api", product / "apps/worker")
        self.boot("accepted-version-mismatch", "register", product, "--version", self.version, "--app", "worker", "--template", "rust",
                  expect="this action requires the project accepted version")
        profile_options = ["--profile", "rust"] if self.modular else ["--template", "rust"]
        self.boot("accepted-version-selected", "register", product, "--app", "worker", *profile_options)
        assert self.results[-1]["requests"] == [f"/releases/v{self.next_version}/devkit-{self.next_version}.tar.gz.sha256", f"/releases/v{self.next_version}/devkit-{self.next_version}.tar.gz"]
        for name, value, expected in (
            ("compact-json", '{"schemaVersion":1,"version":"0.1.1"}', "devkit:"),
            ("nested-json-version", '{"noise":{"version":"9.9.9"},"version":"0.1.1","schemaVersion":1}', "devkit:"),
            ("duplicate-json-last-wins", '{"schemaVersion":1,"version":"9.9.9","version":"0.1.1"}', "devkit:"),
            ("invalid-json-version-type", '{"schemaVersion":1,"version":3}', "invalid project version/schema"),
            ("unsupported-json-schema", '{"schemaVersion":99,"version":"0.1.1"}', "invalid project version/schema"),
            ("invalid-json", '{"schemaVersion":1,"version":', "SyntaxError"),
        ):
            root = self.scratch / name
            (root / ".devkit").mkdir(parents=True)
            (root / ".devkit/manifest.json").write_text(value.replace("0.1.1", self.next_version))
            self.boot(name, "register", root, "--app", "worker", "--template", "rust", expect=expected)
            if expected == "devkit:":
                assert all(f"v{self.next_version}/" in path for path in self.results[-1]["requests"])
            else:
                assert not self.results[-1]["requests"]
        for name, route, expected in (("missing-release", "missing", "404"), ("corrupt-archive", "corrupt", "archive checksum mismatch"),
                                      ("interrupted-http-response", "truncated", "bytes missing")):
            self.boot(name, "init", self.scratch / name, "--version", self.version, "--template", "rust", route=route, expect=expected)
        root = (f"devkit-{self.version}/", "directory", 0o755)
        base = [root, (f"devkit-{self.version}/a", "file", 0o644)]
        for name, member in (
            ("traversal", (f"devkit-{self.version}/../escape", "file", 0o644)),
            ("absolute-path", ("/tmp/escape", "file", 0o644)),
            ("newline-path", (f"devkit-{self.version}/a\nb", "file", 0o644)),
            ("symlink", (f"devkit-{self.version}/b", "symlink", 0o777)),
            ("hardlink", (f"devkit-{self.version}/b", "hardlink", 0o644)),
            ("fifo", (f"devkit-{self.version}/b", "fifo", 0o644)),
            ("setuid-mode", (f"devkit-{self.version}/b", "file", 0o4755)),
            ("wrong-root", ("another-release/b", "file", 0o644)),
            ("duplicate-entry", (f"devkit-{self.version}/a", "file", 0o644)),
        ):
            self.attack(name, base + [member])
        self.wrong_embedded_version()
        self.boot("local-archive-without-http", "init", self.scratch / "offline rust", "--version", self.version, "--template", "rust", "--package-source", "local",
                  "--release-archive", self.release_path)
        assert not self.results[-1]["requests"]
        before = len(self.requests)
        process = self.spawn(["bash", str(self.script), "init", str(self.scratch / "cancelled"), "--version", self.version, "--template", "rust",
                                    "--release-base-url", self.base + "/slow"], env=self.env, cwd=self.scratch,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        assert self.slow_started.wait(10)
        process.send_signal(signal.SIGTERM)
        output, _ = process.communicate(timeout=15)
        self.record("cancel-download-cleanup", process.returncode, output + f"exit={process.returncode}\n", "exit=143", requests=self.requests[before:])
        assert not (self.scratch / "cancelled").exists()
        marker = self.scratch / "extraction started"
        environment = dict(self.env, PROBE_PAUSE_EXTRACTION="1", PROBE_EXTRACTION_STARTED=str(marker))
        process = self.spawn(["bash", str(self.script), "init", str(self.scratch / "extract cancelled"), "--version", self.version,
                                    "--template", "rust", "--release-base-url", self.base + "/releases"], env=environment,
                                   cwd=self.scratch, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        deadline = time.monotonic() + 10
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert marker.exists(), "extraction phase did not start"
        process.send_signal(signal.SIGTERM)
        output, _ = process.communicate(timeout=15)
        self.record("cancel-extraction-supervision", process.returncode, output + f"exit={process.returncode}\n", "exit=143")
        assert not (self.scratch / "extract cancelled").exists()
        assert not self.forbidden.exists()
        self.record("no-host-language-sdk", 0, "No host Node/npm/Rust executable was invoked.\n")
        self.owned_cleanup()

    def owned_containers(self):
        names = {}
        for line in self.docker_log.read_text().splitlines() if self.docker_log.exists() else []:
            args = json.loads(line)
            if "--name" in args:
                labels = [args[i + 1] for i, value in enumerate(args[:-1]) if value == "--label"]
                tokens = [label.split("=", 1)[1] for label in labels if label.startswith("io.docouno.devkit.invocation=")]
                if len(tokens) == 1:
                    names[args[args.index("--name") + 1]] = tokens[0]
        return names

    def container_present(self, name):
        listed = subprocess.run(["docker", "container", "ls", "--all", "--filter", f"name=^/{name}$",
                                 "--format", "{{.Names}}"], capture_output=True, text=True, timeout=20)
        require(listed.returncode == 0, f"Cannot verify container cleanup: {listed.stderr.strip()}")
        require(listed.stdout.strip() in ("", name), f"Unexpected exact-name listing for {name}")
        return bool(listed.stdout.strip())

    def owned_cleanup(self):
        names = sorted(self.owned_containers())
        remaining = []
        for name in names:
            if self.container_present(name):
                remaining.append(name)
        assert not remaining, f"Owned container cleanup failed: {remaining}"
        self.record("exact-owned-containers-cleaned", 0, f"Verified absent {len(names)} exact created containers.\n", ownedNames=names)

    def finish(self, success):
        initially_successful = success
        cleanup_problems = []
        for process in self.processes:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
        for name, token in self.owned_containers().items():
            try:
                if not self.container_present(name):
                    continue
            except (RuntimeError, subprocess.TimeoutExpired) as error:
                cleanup_problems.append(str(error))
                continue
            inspected = subprocess.run(["docker", "container", "inspect", name], capture_output=True, text=True, timeout=20)
            if inspected.returncode != 0:
                cleanup_problems.append(f"Cannot inspect owned container: {name}")
            else:
                labels = json.loads(inspected.stdout)[0]["Config"].get("Labels", {})
                if labels.get("io.docouno.devkit.invocation") != token:
                    cleanup_problems.append(f"Refused mismatched container identity: {name}")
                else:
                    removed = subprocess.run(["docker", "rm", "--force", name], capture_output=True, text=True, timeout=20)
                    cleanup_problems.append(f"Container survived its guard: {name}; cleanup exit {removed.returncode}")
        self.server.shutdown()
        self.server.server_close()
        for path, expected in self.source_identities.items():
            if sha256(Path(path).read_bytes()).hexdigest() != expected:
                success = False
                self.results.append(dict(name="source-archive-unchanged", passed=False, path=path))
                cleanup_problems.append(f"Source archive changed during verification: {path}")
        if cleanup_problems:
            success = False
        (self.logs / "receipt.json").write_text(json.dumps(dict(
            success=success, releaseVersion=self.version, nextReleaseVersion=self.next_version, results=self.results,
            sourceArtifacts=self.source_identities, cleanupProblems=cleanup_problems,
            scriptSha256=sha256(self.script_bytes).hexdigest()), indent=2) + "\n")
        if success:
            shutil.rmtree(self.scratch)
        print(f"Evidence: {self.logs}; scratch: {'removed' if success else self.scratch}", flush=True)
        if initially_successful and cleanup_problems:
            raise RuntimeError("; ".join(cleanup_problems))
