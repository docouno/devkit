#!/usr/bin/env python3
"""Black-box native delivery checks; temporary trust, caches and projects only."""
from __future__ import annotations

import errno
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socketserver
import ssl
import subprocess
import tempfile
import termios
import threading
import time
import unittest
import uuid

REPO = Path(__file__).resolve().parents[2]
DEFAULT_SET = REPO / ".artifacts/native/p3/local-artifact-proof/build-set"
BUILD_SET = Path(os.environ.get("DEVKIT_TEST_BUILD_SET", DEFAULT_SET)).resolve()
RELEASE = Path(os.environ.get("DEVKIT_TEST_RELEASE", BUILD_SET / "payload")).resolve()
ENGINE = Path(os.environ.get("DEVKIT_TEST_ENGINE", RELEASE / "devkit")).resolve()
LEGACY_RECORD = Path(os.environ.get(
    "DEVKIT_TEST_LEGACY_FIXTURES",
    REPO.parent / "devkit/.artifacts/v3/legacy-fixtures.json"))
DOCKER_ENABLED = os.environ.get("DEVKIT_TEST_DOCKER") == "1"
PROOF = Path(os.environ.get("DEVKIT_DELIVERY_PROOF_DIR",
                           REPO / ".artifacts/native/p3/delivery-tests")) / uuid.uuid4().hex
COMMANDS = []
RESULTS = []
EVIDENCE = []


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def file_map(root):
    result = {}
    for path in sorted(Path(root).rglob("*")):
        if path.is_symlink():
            raise AssertionError(f"Unexpected link in delivered payload: {path}")
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {
                "sha256": digest(path), "mode": path.stat().st_mode & 0o777}
    return result


def environment(root, **extra):
    result = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TERM")
              if key in os.environ}
    result.update(HOME=str(root / "home"), XDG_CACHE_HOME=str(root / "xdg"),
                  DEVKIT_CACHE_DIR=str(root / "cache"), CI="true", LC_ALL="C")
    result.update({key: str(value) for key, value in extra.items()})
    return result


def execute(argv, *, cwd, env, timeout=35, check=True):
    started = time.monotonic()
    process = subprocess.Popen([str(value) for value in argv], cwd=cwd, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()
        raise
    result = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
    COMMANDS.append({"argv": [str(v) for v in argv], "code": result.returncode,
                     "seconds": time.monotonic() - started,
                     "stdout": stdout[-8192:], "stderr": stderr[-8192:]})
    if check and result.returncode:
        raise AssertionError(f"{argv!r}: {result.returncode}\n{stdout}\n{stderr}")
    return result


def json_result(result):
    value = json.loads(result.stdout)
    if value.get("schemaVersion") != 1:
        raise AssertionError(f"Unexpected JSON protocol: {value}")
    return value


class Terminal:
    """A real controlling PTY, with bounded reads and cleanup of its child."""
    def __init__(self, argv, *, cwd, env, piped_input=False):
        self.master, slave = os.openpty()
        self.output = bytearray()
        self.argv = [str(value) for value in argv]
        self.started = time.monotonic()

        def attach():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.child = subprocess.Popen(self.argv, cwd=cwd, env=env,
                                      stdin=subprocess.PIPE if piped_input else slave,
                                      stdout=slave, stderr=slave, preexec_fn=attach)
        os.close(slave)

    def read(self, seconds=0.05):
        ready, _, _ = select.select([self.master], [], [], seconds)
        if ready:
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return False
                raise
            self.output.extend(data)
            if len(self.output) > 1024 * 1024:
                raise AssertionError("PTY output exceeded 1 MiB")
            return bool(data)
        return True

    def until(self, marker, timeout=10):
        deadline = time.monotonic() + timeout
        while marker.encode() not in self.output:
            if not self.read() or time.monotonic() > deadline:
                raise AssertionError(f"PTY did not reach {marker!r}: {self.text()}")
        return self

    def send(self, value):
        os.write(self.master, value.encode() if isinstance(value, str) else value)
        return self

    def text(self):
        return self.output.decode("utf8", errors="replace")

    def wait(self, timeout=30):
        deadline = time.monotonic() + timeout
        while self.child.poll() is None:
            self.read()
            if time.monotonic() > deadline:
                raise AssertionError(f"PTY command did not exit: {self.text()}")
        while self.read(0):
            if not select.select([self.master], [], [], 0)[0]:
                break
        COMMANDS.append({"argv": self.argv, "pty": True,
                         "code": self.child.returncode,
                         "seconds": time.monotonic() - self.started,
                         "stdoutAndStderr": self.text()[-8192:]})
        return self.child.returncode, self.text()

    def close(self):
        if self.child.poll() is None:
            os.killpg(self.child.pid, signal.SIGTERM)
            try:
                self.child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.child.pid, signal.SIGKILL)
                self.child.wait(timeout=3)
        if self.child.stdin is not None:
            self.child.stdin.close()
        os.close(self.master)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class HttpsFixture:
    """TLS server and local CONNECT proxy; it never forwards upstream traffic."""
    def __init__(self, directory):
        self.root = directory
        self.requests = []
        self.connections = []
        self.ready = threading.Event()
        self.release = threading.Event()
        self.errors = []
        self.servers = []
        self.threads = []
        self._certificates()
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.context.load_cert_chain(str(directory / "server.pem"),
                                     str(directory / "server.key"))

    def _certificates(self):
        self.root.mkdir()
        env = environment(self.root)
        common = ["openssl", "req", "-newkey", "rsa:2048", "-nodes"]
        execute([*common, "-x509", "-days", "1", "-subj", "/CN=Devkit temporary test CA",
                 "-keyout", "ca.key", "-out", "ca.pem", "-addext",
                 "basicConstraints=critical,CA:TRUE", "-addext",
                 "keyUsage=critical,keyCertSign,cRLSign"], cwd=self.root, env=env)
        execute([*common, "-subj", "/CN=github.com", "-keyout", "server.key",
                 "-out", "server.csr"], cwd=self.root, env=env)
        (self.root / "extensions").write_text(
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\n"
            "extendedKeyUsage=serverAuth\n"
            "subjectAltName=DNS:github.com,DNS:localhost,IP:127.0.0.1\n")
        execute(["openssl", "x509", "-req", "-in", "server.csr", "-CA", "ca.pem",
                 "-CAkey", "ca.key", "-CAcreateserial", "-out", "server.pem",
                 "-days", "1", "-extfile", "extensions"], cwd=self.root, env=env)

    def start(self):
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                fixture.requests.append(self.path)
                try:
                    if self.path == "/ok":
                        self.send_response(200)
                        self.send_header("Content-Length", "13")
                        self.end_headers()
                        self.wfile.write(b"native bytes\n")
                    elif self.path in ("/redirect", "/downgrade", "/loop"):
                        self.send_response(302)
                        location = fixture.url("/ok") if self.path == "/redirect" else (
                            fixture.url("/ok").replace("https:", "http:")
                            if self.path == "/downgrade" else fixture.url("/loop"))
                        self.send_header("Location", location)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                    elif self.path == "/large":
                        self.send_response(200)
                        self.send_header("Content-Length", "4096")
                        self.end_headers()
                        self.wfile.write(b"x" * 4096)
                    elif self.path == "/stream-large":
                        self.send_response(200)
                        self.send_header("Connection", "close")
                        self.end_headers()
                        self.wfile.write(b"x" * 4096)
                        self.close_connection = True
                    elif self.path == "/partial":
                        self.send_response(200)
                        self.send_header("Content-Length", "128")
                        self.end_headers()
                        self.wfile.write(b"truncated")
                        self.close_connection = True
                    elif self.path.endswith("/release-index.json"):
                        self.send_response(200)
                        self.send_header("Content-Length", "128")
                        self.end_headers()
                        self.wfile.flush()
                        fixture.ready.set()
                        fixture.release.wait(15)
                        self.close_connection = True
                    else:
                        self.send_response(404)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
                    self.close_connection = True

        class Direct(http.server.ThreadingHTTPServer):
            daemon_threads = True

            def get_request(self):
                connection, address = super().get_request()
                return fixture.context.wrap_socket(connection, server_side=True), address

        direct = Direct(("127.0.0.1", 0), Handler)
        self.port = direct.server_address[1]
        self.servers.append(direct)

        class ProxyHandler(socketserver.BaseRequestHandler):
            def handle(self):
                try:
                    raw = bytearray()
                    self.request.settimeout(8)
                    while not raw.endswith(b"\r\n\r\n"):
                        block = self.request.recv(1)
                        if not block:
                            return
                        raw.extend(block)
                        if len(raw) > 16 * 1024:
                            raise AssertionError("Proxy headers exceeded budget")
                    first = bytes(raw).split(b"\r\n", 1)[0].decode("ascii")
                    fixture.connections.append(first)
                    if first != "CONNECT github.com:443 HTTP/1.1":
                        self.request.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                        return
                    self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    with fixture.context.wrap_socket(self.request, server_side=True) as secure:
                        Handler(secure, self.client_address, self.server)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError, TimeoutError):
                    pass
                except BaseException as error:
                    fixture.errors.append(repr(error))

        class Proxy(socketserver.ThreadingTCPServer):
            daemon_threads = True
            allow_reuse_address = True

        proxy = Proxy(("127.0.0.1", 0), ProxyHandler)
        self.proxy_port = proxy.server_address[1]
        self.servers.append(proxy)
        for server in self.servers:
            thread = threading.Thread(target=server.serve_forever,
                                      kwargs={"poll_interval": 0.05}, daemon=True)
            self.threads.append(thread)
            thread.start()
        return self

    def url(self, path):
        return f"https://127.0.0.1:{self.port}{path}"

    def close(self):
        self.release.set()
        for server in self.servers:
            server.shutdown()
            server.server_close()
        for thread in self.threads:
            thread.join(timeout=2)


class ReleaseFixture:
    @classmethod
    def setUpClass(cls):
        if not ENGINE.is_file() or not (RELEASE / "release.json").is_file():
            raise RuntimeError("Build a local native release or set DEVKIT_TEST_ENGINE/RELEASE")
        cls.manifest = json.loads((RELEASE / "release.json").read_text())
        cls.version = cls.manifest["version"]
        cls.identity = digest(RELEASE / "release.json")
        cls.source_before = file_map(RELEASE)

    @classmethod
    def tearDownClass(cls):
        if file_map(RELEASE) != cls.source_before:
            raise AssertionError("Delivery tests changed the source release payload")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="devkit-delivery-")
        self.root = Path(self.temp.name)
        self.env = environment(self.root)

    def tearDown(self):
        self.temp.cleanup()

    def cli(self, *args, check=True, timeout=35, env=None):
        return execute([ENGINE, *args], cwd=self.root, env=env or self.env,
                       check=check, timeout=timeout)

    def create(self, target=None):
        target = target or self.root / "project"
        result = self.cli("init", target, "--app", "api=rust-api", "--name", "project",
                          "--release-root", RELEASE, "--package-source", "local", "--json")
        value = json_result(result)
        self.assertTrue(value["ok"])
        self.assertEqual(value["result"]["releaseIdentity"], self.identity)
        return target


class DeliveryTests(ReleaseFixture, unittest.TestCase):
    def test_cached_engine_permissions_refuse_before_lifecycle_writes(self):
        self.create()
        accepted = self.manifest["files"]["devkit"]["sha256"]
        cached = self.root / "cache/engines/x86_64-unknown-linux-musl" / self.version / accepted / "devkit"
        before = digest(cached)
        for mode in (0o644, 0o645):
            with self.subTest(mode=oct(mode)):
                cached.chmod(mode)
                target = self.root / f"not-created-{mode:o}"
                try:
                    result = self.cli("init", target, "--app", "api=rust-api", "--name", "project",
                                      "--release-root", RELEASE, "--package-source", "local", "--json", check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(json_result(result)["ok"])
                    self.assertFalse(target.exists())
                    self.assertEqual(cached.stat().st_mode & 0o7777, mode)
                    self.assertEqual(digest(cached), before)
                finally:
                    cached.chmod(0o755)

    def test_native_metadata_accepts_relative_project_roots_without_sdk(self):
        target = self.create()
        nested = target / "nested"
        nested.mkdir()
        for cwd, selected in ((target, "."), (self.root, "project"), (nested, "..")):
            result = execute([ENGINE, "describe", "--root", selected, "--json"],
                             cwd=cwd, env=dict(self.env, PATH=""))
            value = json_result(result)
            self.assertTrue(value["ok"])
            self.assertEqual(Path(value["result"]["root"]), target)
            self.assertEqual(value["result"]["releaseIdentity"], self.identity)

    def test_real_local_init_uses_exact_release_and_offline_metadata(self):
        target = self.create(self.root / "project with spaces")
        manifest_before = (target / ".devkit/manifest.json").read_bytes()
        no_sdk = dict(self.env, PATH="")
        result = self.cli("describe", "--root", target, "--json", env=no_sdk)
        self.assertEqual(json_result(result)["result"]["version"], self.version)
        self.assertEqual((target / ".devkit/manifest.json").read_bytes(), manifest_before)
        cached = self.root / "cache/releases" / self.version / self.identity
        self.assertEqual(file_map(cached), self.source_before)
        self.assertEqual(digest(cached / "devkit"), digest(ENGINE))

    def test_source_and_cached_corruption_refuse_without_creating_a_project(self):
        self.create()
        cached = self.root / "cache/releases" / self.version / self.identity
        owned = next(name for name in self.manifest["files"]
                     if name.startswith("templates/rust-api/") and name.endswith(".rs"))
        original = (cached / owned).read_bytes()
        (cached / owned).write_bytes(original + b"\n// cache corruption\n")
        output = self.cli("init", self.root / "rejected", "--app", "api=rust-api",
                          "--name", "rejected", "--offline", "--json", check=False)
        self.assertNotEqual(output.returncode, 0)
        self.assertFalse(json_result(output)["ok"])
        self.assertFalse((self.root / "rejected").exists())
        damaged_source = self.root / "damaged-source"
        shutil.copytree(RELEASE, damaged_source)
        (damaged_source / owned).write_bytes(original + b"\n// source corruption\n")
        output = self.cli("init", self.root / "source-rejected", "--app", "api=rust-api",
                          "--release-root", damaged_source, "--name", "rejected", "--json",
                          check=False)
        self.assertNotEqual(output.returncode, 0)
        self.assertFalse((self.root / "source-rejected").exists())

    def test_two_processes_can_import_the_same_release_without_partial_cache(self):
        children = []
        for name in ("first", "second"):
            children.append(subprocess.Popen([str(ENGINE), "init", str(self.root / name),
                "--app", "api=rust-api", "--name", name, "--release-root", str(RELEASE),
                "--package-source", "local", "--json"], cwd=self.root, env=self.env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
        try:
            for child in children:
                stdout, stderr = child.communicate(timeout=40)
                self.assertEqual(child.returncode, 0, stdout + stderr)
                self.assertEqual(json.loads(stdout)["result"]["releaseIdentity"], self.identity)
            cached = self.root / "cache/releases" / self.version / self.identity
            self.assertEqual(file_map(cached), self.source_before)
            self.assertFalse(any(p.name.startswith((".import-", ".acquire-"))
                                 for p in (self.root / "cache").iterdir()))
        finally:
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=3)

    def test_initial_loader_empty_engine_directory_can_be_completed(self):
        first = self.create()
        before = (first / ".devkit/manifest.json").read_bytes()
        engine_sha = self.manifest["files"]["devkit"]["sha256"]
        cached = self.root / "cache/engines/x86_64-unknown-linux-musl" / self.version / engine_sha / "devkit"
        self.assertEqual(digest(cached), engine_sha)
        cached.unlink()
        self.assertTrue(cached.parent.is_dir())
        self.assertEqual(list(cached.parent.iterdir()), [])
        second = self.root / "second-project"
        result = self.cli("init", second, "--app", "api=rust-api", "--name", "second-project",
                          "--release-root", RELEASE, "--package-source", "local", "--json", check=False)
        EVIDENCE.append({"case": "empty-engine-directory", "removedEngineSha256": engine_sha,
                         "shaDirectoryPresentBeforeSecondInit": True,
                         "shaDirectoryEntriesBeforeSecondInit": [],
                         "sourceReleaseIdentity": self.identity,
                         "secondInitCode": result.returncode,
                         "secondInit": json.loads(result.stdout)})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(digest(cached), engine_sha)
        self.assertEqual((first / ".devkit/manifest.json").read_bytes(), before)

    def test_existing_corrupt_engine_is_refused_without_overwriting_it(self):
        self.create()
        engine_sha = self.manifest["files"]["devkit"]["sha256"]
        cached = self.root / "cache/engines/x86_64-unknown-linux-musl" / self.version / engine_sha / "devkit"
        with cached.open("ab") as output:
            output.write(b"damaged-cache")
        corrupt = digest(cached)
        result = self.cli("init", self.root / "rejected", "--app", "api=rust-api",
                          "--name", "rejected", "--release-root", RELEASE, "--json", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "rejected").exists())
        self.assertEqual(digest(cached), corrupt)

    def test_real_terminal_create_and_complete_arguments_do_not_need_input(self):
        human = self.root / "human-project"
        interactive = dict(self.env, CI="")
        argv = [ENGINE, "init", "--release-root", RELEASE, "--package-source", "local"]
        with Terminal(argv, cwd=self.root, env=interactive) as terminal:
            terminal.until("Project directory").send(str(human) + "\n")
            terminal.until("Application template").send("2\n")
            terminal.until("Application name").send("api\n")
            terminal.until("Add another application").send("n\n")
            code, output = terminal.wait()
            self.assertEqual(code, 0, output)
        state = json.loads((human / ".devkit/manifest.json").read_text())
        self.assertEqual(state["version"], self.version)
        self.assertEqual(list(state["components"]), ["apps/api"])
        complete = self.root / "complete-project"
        argv = [ENGINE, "init", complete, "--app", "api=rust-api", "--release-root", RELEASE,
                "--package-source", "local", "--json"]
        with Terminal(argv, cwd=self.root, env=interactive) as terminal:
            code, output = terminal.wait()
            self.assertEqual(code, 0, output)
            self.assertTrue(json.loads(output)["ok"])
            self.assertNotIn("Application template", output)

    def test_prompt_cancellation_eof_and_ci_refusal_happen_before_effects(self):
        for key, expected in ((b"\x03", 130), (b"\x04", 1)):
            scoped = self.root / str(expected)
            scoped.mkdir()
            env = environment(scoped, CI="")
            argv = [ENGINE, "init", "--release-root", RELEASE]
            with Terminal(argv, cwd=scoped, env=env) as terminal:
                terminal.until("Project directory").send(key)
                code, output = terminal.wait(timeout=5)
                self.assertEqual(code, expected, output)
            self.assertEqual(list(scoped.iterdir()), [])
        with Terminal([ENGINE, "init", "--json"], cwd=self.root, env=self.env) as terminal:
            code, output = terminal.wait(timeout=5)
            self.assertNotEqual(code, 0)
            self.assertFalse(json.loads(output)["ok"])
            self.assertNotIn("Project directory", output)

    def test_controlling_terminal_works_with_piped_standard_input(self):
        target = self.root / "pipe-project"
        argv = [ENGINE, "init", "--interactive", "--release-root", RELEASE,
                "--package-source", "local"]
        with Terminal(argv, cwd=self.root, env=dict(self.env, CI=""), piped_input=True) as terminal:
            terminal.until("Project directory").send(str(target) + "\n")
            terminal.until("Application template").send("2\n")
            terminal.until("Application name").send("api\n")
            terminal.until("Add another application").send("n\n")
            code, output = terminal.wait()
            self.assertEqual(code, 0, output)
        self.assertEqual(json.loads((target / ".devkit/manifest.json").read_text())["releaseIdentity"], self.identity)


class TransportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="devkit-delivery-tls-")
        cls.root = Path(cls.temp.name)
        cls.fixture = HttpsFixture(cls.root / "tls").start()

    @classmethod
    def tearDownClass(cls):
        cls.fixture.close()
        cls.temp.cleanup()

    def setUp(self):
        self.output = self.root / uuid.uuid4().hex
        self.env = environment(self.root, SSL_CERT_FILE=self.fixture.root / "ca.pem")

    def fetch(self, path, maximum=128, check=False, env=None):
        return execute([ENGINE, "internal", "fetch", "--url", self.fixture.url(path),
                        "--output", self.output, "--max-bytes", str(maximum)],
                       cwd=self.root, env=env or self.env, timeout=15, check=check)

    def test_native_https_requires_trust_and_follows_only_https_redirects(self):
        denied = self.fetch("/ok", env=environment(self.root))
        self.assertNotEqual(denied.returncode, 0)
        self.assertFalse(self.output.exists())
        allowed = self.fetch("/redirect", check=True)
        self.assertEqual(allowed.returncode, 0)
        self.assertEqual(self.output.read_bytes(), b"native bytes\n")
        self.output.unlink()
        for path in ("/downgrade", "/loop"):
            output = self.fetch(path)
            self.assertNotEqual(output.returncode, 0, path)
            self.assertFalse(self.output.exists())
        self.assertEqual(self.fixture.errors, [])

    def test_oversized_and_truncated_https_bodies_never_publish_partial_files(self):
        for path in ("/large", "/stream-large", "/partial"):
            output = self.fetch(path, maximum=128)
            self.assertNotEqual(output.returncode, 0, path)
            self.assertFalse(self.output.exists(), path)

    def test_parent_cancellation_reaps_https_worker_and_discards_unaccepted_cache(self):
        work = self.root / uuid.uuid4().hex
        work.mkdir()
        env = environment(work, SSL_CERT_FILE=self.fixture.root / "ca.pem",
                          HTTPS_PROXY=f"http://127.0.0.1:{self.fixture.proxy_port}")
        self.fixture.ready.clear()
        self.fixture.release.clear()
        process = subprocess.Popen([str(ENGINE), "init", str(work / "project"),
            "--app", "api=rust-api", "--to", "9.9.9-test", "--json"],
            cwd=work, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        pids = []
        try:
            self.assertTrue(self.fixture.ready.wait(8), "Cold native metadata request never reached the TLS proxy")
            children = Path(f"/proc/{process.pid}/task/{process.pid}/children")
            pids = [int(value) for value in children.read_text().split()]
            self.assertTrue(pids, "No supervised native HTTPS worker was present")
            started = time.monotonic()
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=6)
            self.assertEqual(process.returncode, 143, stdout + stderr)
            self.assertFalse(json.loads(stdout)["ok"])
            self.assertLess(time.monotonic() - started, 5)
            self.assertTrue(all(not Path(f"/proc/{pid}").exists() for pid in pids))
            self.assertFalse((work / "project").exists())
            cache = work / "cache"
            self.assertFalse(any(path.name.startswith(".acquire-") for path in cache.iterdir()))
            self.assertFalse((cache / "refs").exists())
            self.assertFalse((cache / "releases").exists())
            self.assertTrue(any(value == "CONNECT github.com:443 HTTP/1.1"
                                for value in self.fixture.connections))
            self.assertEqual(self.fixture.errors, [])
        finally:
            self.fixture.release.set()
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)


@unittest.skipUnless(DOCKER_ENABLED, "Set DEVKIT_TEST_DOCKER=1 in the coordinated Docker test phase")
class NpxTests(ReleaseFixture, unittest.TestCase):
    def test_accepted_cached_release_wins_over_implicit_adjacent_preview(self):
        target = self.create()
        before = json.loads((target / ".devkit/manifest.json").read_text())
        adjacent = self.root / "adjacent-preview"
        shutil.copytree(RELEASE, adjacent)
        readme = adjacent / "README.md"
        readme.write_bytes(readme.read_bytes() + b"\nLocal same-version preview marker.\n")
        manifest = json.loads((adjacent / "release.json").read_text())
        old_engine_sha = manifest["files"]["devkit"]["sha256"]
        caller_engine_sha = digest(ENGINE)
        shutil.copy2(ENGINE, adjacent / "devkit")
        for name in ("runtime/devkit.sh", "runtime/toolchains.env"):
            path = adjacent / name
            path.write_text(path.read_text().replace(old_engine_sha, caller_engine_sha))
            manifest["files"][name]["sha256"] = digest(path)
        manifest["files"]["devkit"]["sha256"] = caller_engine_sha
        manifest["files"]["README.md"]["sha256"] = digest(readme)
        (adjacent / "release.json").write_text(json.dumps(manifest, indent=2) + "\n")
        new_identity = digest(adjacent / "release.json")
        self.assertNotEqual(new_identity, self.identity)
        self.assertEqual(digest(adjacent / "devkit"), caller_engine_sha)
        implicit = execute([adjacent / "devkit", "add-app", target, "--name", "extra",
                            "--template", "rust-api", "--json"],
                           cwd=self.root, env=self.env, check=False)
        explicit = execute([adjacent / "devkit", "add-app", target, "--name", "wrong",
                            "--template", "rust-api", "--release-root", adjacent, "--json"],
                           cwd=self.root, env=self.env, check=False)
        EVIDENCE.append({"case": "accepted-data-before-implicit-adjacent-preview",
                         "acceptedReleaseIdentity": self.identity,
                         "adjacentPreviewIdentity": new_identity,
                         "cachedReleasePresent": True,
                         "callerElfSha256": digest(adjacent / "devkit"),
                         "implicitCode": implicit.returncode, "implicitResult": json.loads(implicit.stdout),
                         "explicitWrongCode": explicit.returncode,
                         "explicitWrongResult": json.loads(explicit.stdout)})
        self.assertNotEqual(explicit.returncode, 0)
        self.assertFalse((target / "apps/wrong").exists())
        self.assertEqual(implicit.returncode, 0, implicit.stdout + implicit.stderr)
        after = json.loads((target / ".devkit/manifest.json").read_text())
        self.assertEqual(after["releaseIdentity"], before["releaseIdentity"])
        self.assertEqual(after["version"], before["version"])
        self.assertIn("apps/extra", after["components"])

    def test_real_terminal_add_rust_application_uses_only_compose_prerequisite_and_inventory(self):
        target = self.create()
        original = (target / "apps/api/src/lib.rs").read_bytes()
        original_make = (target / "Makefile").read_bytes()
        real_docker = shutil.which("docker", path=self.env["PATH"])
        self.assertIsNotNone(real_docker)
        tools = self.root / "tools"
        tools.mkdir()
        wrapper = tools / "docker"
        wrapper.write_text(
            "#!/usr/bin/python3\nimport json,os,sys\n"
            "with open(os.environ['DEVKIT_TEST_DOCKER_LOG'],'a') as out:"
            "out.write(json.dumps(sys.argv[1:])+'\\n')\n"
            "os.execv(os.environ['DEVKIT_TEST_REAL_DOCKER'],"
            "[os.environ['DEVKIT_TEST_REAL_DOCKER'],*sys.argv[1:]])\n")
        wrapper.chmod(0o755)
        log = self.root / "docker-argv.jsonl"
        env = dict(self.env, CI="", PATH=str(tools) + os.pathsep + self.env["PATH"],
                   DEVKIT_TEST_DOCKER_LOG=str(log), DEVKIT_TEST_REAL_DOCKER=real_docker)
        with Terminal([ENGINE, "add-app", target, "--release-root", RELEASE],
                      cwd=self.root, env=env) as terminal:
            terminal.until("Application name").send("worker\n")
            terminal.until("Application template").send("2\n")
            code, output = terminal.wait()
            self.assertEqual(code, 0, output)
        state = json.loads((target / ".devkit/manifest.json").read_text())
        self.assertEqual(set(state["components"]), {"apps/api", "apps/worker"})
        self.assertEqual(state["releaseIdentity"], self.identity)
        self.assertEqual((target / "apps/api/src/lib.rs").read_bytes(), original)
        self.assertEqual((target / "Makefile").read_bytes(), original_make)
        commands = [json.loads(line) for line in log.read_text().splitlines()]
        version = ["compose", "version", "--short"]
        def inventory(argv):
            return argv[:1] == ["compose"] and argv[-4:] == [
                "config", "--no-env-resolution", "--format", "json"]
        self.assertIn(version, commands)
        self.assertTrue(any(inventory(argv) for argv in commands), commands)
        self.assertTrue(all(argv == version or inventory(argv) for argv in commands), commands)
        EVIDENCE.append({"case": "real-pty-add-rust", "dockerArgv": commands,
                         "acceptedReleaseIdentity": self.identity,
                         "existingCodeAndMakePreserved": True})

    def sdk(self, output, mask, command):
        pins = dict(line.split("=", 1) for line in (REPO / "toolchains.env").read_text().splitlines()
                    if line and not line.startswith("#"))
        token = uuid.uuid4().hex
        name = "devkit-delivery-npx-" + token
        script = r'''
set -eu
test ! -e /var/run/docker.sock
test -z "${DEVKIT_HOST_SECRET+x}"
umask "$1"; shift
rm -f /proof/proxy.port
node -e 'const fs=require("node:fs"),net=require("node:net");const server=net.createServer(s=>{fs.appendFileSync("/proof/network.log","connect\n");s.end("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")});server.listen(0,"127.0.0.1",()=>fs.writeFileSync("/proof/proxy.port",String(server.address().port)))' &
proxy=$!
trap 'kill "$proxy" 2>/dev/null || :; wait "$proxy" 2>/dev/null || :' 0
i=0
while test ! -s /proof/proxy.port; do
  i=$((i+1)); test "$i" -lt 100; sleep .02
done
HTTPS_PROXY=http://127.0.0.1:$(cat /proof/proxy.port)
export HTTPS_PROXY
"$@"
'''
        argv = [ENGINE, "internal", "guard", name, token, "--", "docker", "run",
                "--pull", "never", "--rm", "--init", "--name", name,
                "--label", f"io.docouno.devkit.invocation={token}",
                "--network", "none", "--read-only", "--memory", "1g", "--cpus", "2",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
                "--user", f"{os.getuid()}:{os.getgid()}",
                "--tmpfs", "/tmp:mode=1777,size=268435456",
                "--mount", f"type=bind,source={BUILD_SET},target=/input,readonly",
                "--mount", f"type=bind,source={output},target=/proof",
                "--workdir", "/proof"]
        for value in ("HOME=/tmp/home", "CI=true", "DEVKIT_CACHE_DIR=/proof/native-cache",
                      "npm_config_cache=/proof/npm-cache", "npm_config_userconfig=/tmp/user.npmrc",
                      "npm_config_globalconfig=/tmp/global.npmrc", "npm_config_update_notifier=false",
                      "npm_config_audit=false", "npm_config_fund=false"):
            argv.extend(("--env", value))
        argv.extend((pins["DEVKIT_NODE_IMAGE"], "sh", "-c", script, "sh", mask, *command))
        return execute(argv, cwd=self.root,
                       env=dict(self.env, DEVKIT_HOST_SECRET="must-not-enter-the-sdk"), timeout=45)

    def npx_mode(self, mask):
        output = self.root / "npx"
        output.mkdir()
        index = json.loads((BUILD_SET / "release-index.json").read_text())
        cli_archive = index["packages"]["cli"]["archive"]
        base = ["npm", "exec", "--yes", "--offline", "--ignore-scripts",
                "--package", "/input/" + cli_archive, "--", "devkit"]
        version = self.sdk(output, mask, [*base, "--version"])
        self.assertEqual(version.stdout.strip(), "devkit " + self.version)
        installed = list((output / "npm-cache/_npx").glob("*/node_modules/@docouno/devkit"))
        self.assertEqual(len(installed), 1)
        before = file_map(installed[0])
        self.assertEqual(set(before), set(self.source_before))
        for name, identity in before.items():
            self.assertEqual(identity["sha256"], self.source_before[name]["sha256"], name)
            self.assertEqual(bool(identity["mode"] & 0o111),
                             bool(self.source_before[name]["mode"] & 0o111), name)
        if mask == "077":
            self.assertTrue(any(value["mode"] == 0o600 for value in before.values()))
        created = self.sdk(output, mask, [*base, "init", "/proof/project", "--app", "api=rust-api",
                          "--name", "project", "--package-source", "local", "--offline", "--json"])
        self.assertEqual(json_result(created)["result"]["releaseIdentity"], self.identity)
        self.assertEqual(file_map(installed[0]), before, "Canonical import mutated npm's source tree")
        canonical = output / "native-cache/releases" / self.version / self.identity
        self.assertEqual(file_map(canonical), self.source_before)
        self.assertEqual(digest(canonical / "devkit"), digest(ENGINE))
        self.assertFalse((output / "network.log").exists(), "Same-version native acquisition contacted the proxy")
        EVIDENCE.append({"case": "actual-npx", "umask": mask,
                         "cliArchiveSha256": digest(BUILD_SET / cli_archive),
                         "installedFiles": len(before), "sourceUnchanged": True,
                         "canonicalReleaseIdentity": self.identity,
                         "canonicalElfSha256": digest(canonical / "devkit"),
                         "proxyConnections": 0, "network": "none", "ignoreScripts": True})

    def test_npx_direct_elf_with_ignore_scripts_umask_022(self):
        self.npx_mode("022")

    def test_npx_direct_elf_with_ignore_scripts_umask_077(self):
        self.npx_mode("077")


@unittest.skipUnless(LEGACY_RECORD.is_file(), "Genuine public 0.2.1 consumer fixtures are not configured")
class LegacyTests(ReleaseFixture, unittest.TestCase):
    def legacy(self, kind):
        record = json.loads(LEGACY_RECORD.read_text())
        project = record["projects"][kind]
        source = Path(project["path"])
        if not source.is_dir():
            self.skipTest("Genuine legacy fixture directory is unavailable; restore its verified backup")
        for name, expected in project["preservedFiles"].items():
            self.assertEqual(digest(source / name), expected, "Legacy baseline changed")
        state = json.loads((source / ".devkit/manifest.json").read_text())
        excluded = {".git", ".artifacts", ".devkit/state", ".devkit/cache", "target", "dist", "release"}
        for component in state["components"]:
            excluded.update(f"{component}/{directory}" for directory in ("target", "dist", "release"))

        def ignore(directory, names):
            relative = Path(directory).relative_to(source)
            return [name for name in names if name == "node_modules"
                    or (relative / name).as_posix() in excluded]

        target = self.root / (kind + " legacy with spaces")
        shutil.copytree(source, target, ignore=ignore)
        return target, project, state

    def test_native_metadata_reads_genuine_old_data_without_sdk_or_cache(self):
        for kind in ("node", "rust"):
            root, project, state = self.legacy(kind)
            before = (root / ".devkit/manifest.json").read_bytes()
            result = self.cli("describe", "--root", root, "--json", env=dict(self.env, PATH=""))
            self.assertEqual(json_result(result)["result"]["version"], "0.2.1")
            self.assertEqual(json_result(result)["result"]["releaseIdentity"], state["releaseIdentity"])
            self.assertEqual((root / ".devkit/manifest.json").read_bytes(), before)
            first = next(iter(state["components"]))
            rejected = self.cli("exec", "--root", root, "--component", first, "--", "true",
                                check=False, env=dict(self.env, PATH=""))
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("requires Devkit 0.3", rejected.stderr)
            self.assertFalse((self.root / "cache").exists())
            for name, expected in project["preservedFiles"].items():
                self.assertEqual(digest(root / name), expected)

    def test_genuine_old_rust_project_updates_to_local_native_payload_with_preservation(self):
        root, project, state = self.legacy("rust")
        result = self.cli("update", root, "--to", self.version, "--release-root", RELEASE,
                          "--json", env=dict(self.env, PATH=""))
        self.assertEqual(json_result(result)["result"]["releaseIdentity"], self.identity)
        updated = json.loads((root / ".devkit/manifest.json").read_text())
        self.assertEqual(updated["version"], self.version)
        self.assertEqual(updated["productMetadata"], state["productMetadata"])
        for name, previous in state["components"].items():
            self.assertEqual(updated["components"][name]["productMetadata"], previous["productMetadata"])
            self.assertEqual(updated["components"][name]["origin"], previous["origin"])
            self.assertEqual(updated["components"][name]["development"], previous["development"])
        for name, expected in project["preservedFiles"].items():
            self.assertEqual(digest(root / name), expected)
        EVIDENCE.append({"case": "genuine-legacy-rust-to-local-native",
                         "fromVersion": state["version"], "fromIdentity": state["releaseIdentity"],
                         "toVersion": self.version, "toIdentity": self.identity,
                         "preservedFiles": project["preservedFiles"], "unknownMetadataPreserved": True,
                         "hostSdkPath": "empty", "scope": "local prototype, not public 0.3 publication"})


class RecordingResult(unittest.TextTestResult):
    def startTest(self, test):
        self.case_started = time.monotonic()
        super().startTest(test)

    def _record(self, test, status, detail=None):
        item = {"test": test.id(), "status": status,
                "seconds": time.monotonic() - getattr(self, "case_started", time.monotonic())}
        if detail:
            item["detail"] = detail
        RESULTS.append(item)

    def addSuccess(self, test):
        self._record(test, "passed")
        super().addSuccess(test)

    def addFailure(self, test, err):
        self._record(test, "failed", self._exc_info_to_string(err, test))
        super().addFailure(test, err)

    def addError(self, test, err):
        self._record(test, "error", self._exc_info_to_string(err, test))
        super().addError(test, err)

    def addSkip(self, test, reason):
        self._record(test, "skipped", reason)
        super().addSkip(test, reason)


if __name__ == "__main__":
    PROOF.mkdir(parents=True)
    shutil.copy2(__file__, PROOF / "test_source.py")
    program = unittest.main(exit=False, verbosity=2,
                            testRunner=unittest.TextTestRunner(resultclass=RecordingResult, verbosity=2))
    receipt = {
        "engine": {"path": str(ENGINE), "sha256": digest(ENGINE)},
        "release": {"path": str(RELEASE), "manifestSha256": digest(RELEASE / "release.json")},
        "testSha256": digest(Path(__file__)),
        "successful": program.result.wasSuccessful(),
        "testsRun": program.result.testsRun,
        "failures": len(program.result.failures), "errors": len(program.result.errors),
        "skipped": len(program.result.skipped), "cases": RESULTS, "commands": COMMANDS,
        "evidence": EVIDENCE,
        "limits": ["Local artifact and temporary trusted-CA/proxy/PTY checks; not public GitHub/npm release proof.",
                   "No production/Cargo/index/global configuration changes or host language SDK."]}
    (PROOF / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    raise SystemExit(not program.result.wasSuccessful())
