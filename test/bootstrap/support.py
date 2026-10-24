#!/usr/bin/env python3
"""Shared native bootstrap fixture: real TLS/proxy, raw ELF, immutable 0.2 archive."""
from __future__ import annotations
import hashlib
import http.server
import json
import os
import resource
from pathlib import Path
import shutil
import signal
import socketserver
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import uuid

AUTHOR = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(AUTHOR / 'test/delivery'))
from test_delivery import HttpsFixture  # noqa: E402


def require(value, message):
    if not value:
        raise AssertionError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


class Fixture:
    """CONNECT is confined to github.com and is terminated locally with a test CA."""
    def __init__(self, directory):
        self.cert = HttpsFixture(directory)
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.load_cert_chain(str(directory / 'server.pem'), str(directory / 'server.key'))
        self.routes = {}
        self.requests = []
        self.ready = threading.Event()
        self.resume = threading.Event()
        fixture = self
        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def log_message(self, *_):
                pass
            def do_GET(self):
                fixture.requests.append(self.path)
                value = fixture.routes.get(self.path)
                if callable(value):
                    value(self)
                    return
                if value is None:
                    self.send_response(404); self.send_header('Content-Length', '0'); self.end_headers()
                    return
                self.send_response(200)
                self.send_header('Content-Length', str(len(value)))
                self.end_headers()
                try:
                    self.wfile.write(value)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
                    pass
        class ProxyHandler(socketserver.BaseRequestHandler):
            def handle(self):
                try:
                    self.request.settimeout(15)
                    header = bytearray()
                    while not header.endswith(b'\r\n\r\n'):
                        byte = self.request.recv(1)
                        if not byte:
                            return
                        header.extend(byte)
                        require(len(header) <= 16384, 'Proxy request exceeded its bound')
                    require(bytes(header).split(b'\r\n')[0] == b'CONNECT github.com:443 HTTP/1.1', 'Unexpected proxy authority')
                    self.request.sendall(b'HTTP/1.1 200 Connection Established\r\n\r\n')
                    with fixture.context.wrap_socket(self.request, server_side=True) as secure:
                        Handler(secure, self.client_address, self.server)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError, TimeoutError):
                    pass
        class Proxy(socketserver.ThreadingTCPServer):
            daemon_threads = True
            allow_reuse_address = True
        self.server = Proxy(('127.0.0.1', 0), ProxyHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={'poll_interval': .05}, daemon=True)
        self.thread.start()
    def stall(self, handler):
        handler.send_response(200)
        handler.send_header('Content-Length', '128')
        handler.end_headers(); handler.wfile.flush()
        self.ready.set()
        self.resume.wait(20)
        handler.close_connection = True
    def close(self):
        self.resume.set()
        self.server.shutdown(); self.server.server_close(); self.thread.join(timeout=2)


class Harness:
    def __init__(self, build_set, legacy_release):
        self.build_set = Path(build_set).resolve()
        self.payload = self.build_set / 'payload'
        self.index = json.loads((self.build_set / 'release-index.json').read_text())
        self.version = self.index['version']
        self.engine = self.build_set / self.index['engine']
        for name, identity in self.index['files'].items():
            data = (self.build_set / name).read_bytes()
            require(len(data) == identity['size'] and digest(data) == identity['sha256'], f'Build-set identity differs: {name}')
        self.source_before = {str(p.relative_to(self.build_set)): digest(p.read_bytes()) for p in self.build_set.rglob('*') if p.is_file()}
        self.legacy = Path(legacy_release).resolve()
        self.legacy_before = digest(self.legacy.read_bytes())
        with tarfile.open(self.legacy, 'r:gz') as archive:
            names = [e for e in archive.getmembers() if e.name.endswith('/release.json')]
            require(len(names) == 1 and names[0].isfile(), 'Expected one genuine legacy manifest')
            self.legacy_manifest = archive.extractfile(names[0]).read()
        self.old_version = json.loads(self.legacy_manifest)['version']
        require(self.old_version in ('0.2.0', '0.2.1'), 'Only published schema2 legacy releases are supported')
        expected = f'{self.legacy_before}  devkit-{self.old_version}.tar.gz\n'
        require(Path(str(self.legacy) + '.sha256').read_text() == expected, 'Legacy checksum differs')
        self.temp = tempfile.TemporaryDirectory(prefix='devkit bootstrap ')
        self.scratch = Path(self.temp.name)
        self.logs = AUTHOR / '.artifacts/bootstrap' / uuid.uuid4().hex
        self.logs.mkdir(parents=True)
        self.fixture = Fixture(self.scratch / 'tls')
        self.base = '/docouno/devkit/releases/download'
        self.engine_route = f'{self.base}/v{self.version}/{self.index["engine"]}'
        self.fixture.routes[self.engine_route] = self.engine.read_bytes()
        self.fixture.routes[f'{self.base}/v{self.version}/install.sh'] = (self.build_set / 'install.sh').read_bytes()
        self.old_base = f'{self.base}/v{self.old_version}'
        self.archive_route = f'{self.old_base}/devkit-{self.old_version}.tar.gz'
        self.fixture.routes[self.archive_route] = self.legacy.read_bytes()
        self.fixture.routes[self.archive_route + '.sha256'] = expected.encode()
        self.fixture.routes[self.old_base + '/release.json'] = self.legacy_manifest
        self.forbidden = self.scratch / 'host-sdk-attempts'
        blockers = self.scratch / 'without host SDK'
        blockers.mkdir()
        for name in ('node', 'npm', 'npx', 'cargo', 'rustc', 'rustup', 'tsc', 'eslint'):
            path = blockers / name
            path.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$DEVKIT_FORBIDDEN_HOST_SDK_LOG"\nexit 97\n')
            path.chmod(0o755)
        self.env = {k: v for k, v in os.environ.items() if k in ('PATH', 'LANG', 'LC_ALL', 'TERM')}
        self.env.update(PATH=str(blockers) + os.pathsep + self.env['PATH'], HOME=str(self.scratch / 'home'),
            CI='true', HTTPS_PROXY=f'http://127.0.0.1:{self.fixture.server.server_address[1]}',
            SSL_CERT_FILE=str(self.fixture.cert.root / 'ca.pem'), CURL_CA_BUNDLE=str(self.fixture.cert.root / 'ca.pem'),
            DEVKIT_FORBIDDEN_HOST_SDK_LOG=str(self.forbidden))
        self.script = self.scratch / 'downloaded install.sh'
        self.results = []
    def environment(self, name):
        return dict(self.env, DEVKIT_CACHE_DIR=str(self.scratch / ('cache ' + name)))
    def run(self, name, argv, *, expected=None, env=None, timeout=45, memory_limit=None):
        started = time.monotonic()
        result = subprocess.run([str(v) for v in argv], env=env or self.environment(name), cwd=self.scratch,
            capture_output=True, text=True, timeout=timeout,
            preexec_fn=(lambda: resource.setrlimit(resource.RLIMIT_AS, (memory_limit, memory_limit))) if memory_limit else None)
        output = result.stdout + result.stderr
        (self.logs / f'{len(self.results):02d}-{name}.log').write_text(output)
        passed = result.returncode == 0 if expected is None else result.returncode != 0 and expected in output
        self.results.append(dict(name=name, exitCode=result.returncode, passed=passed, seconds=time.monotonic() - started, addressSpaceLimit=memory_limit))
        require(passed, f'{name}: {result.returncode}\n{output}')
        print('PASS ' + name, flush=True)
        require(not self.forbidden.exists(), 'Invoked host language SDK')
        return result
    def boot(self, name, *args, **kwargs):
        return self.run(name, ['sh', self.script, *args], **kwargs)
    def cold_old(self, name, *, expected=None):
        target = self.scratch / name
        result = self.run(name, [self.engine, 'init', target, '--to', self.old_version, '--app', 'api=rust-api', '--package-source', 'local', '--non-interactive'], expected=expected, memory_limit=512 * 1024 * 1024 if expected is not None else None)
        cache = self.scratch / ('cache ' + name)
        require(not list(cache.glob('.acquire-*')), 'Unaccepted acquisition directory leaked')
        if expected is not None:
            require(not target.exists(), 'Rejected archive wrote the project')
        return result
    def execute(self):
        self.run('download-versioned-loader', ['curl', '--disable', '--fail', '--silent', '--show-error', '--output', self.script,
            f'https://github.com{self.base}/v{self.version}/install.sh'])
        require(self.script.read_bytes() == (self.build_set / 'install.sh').read_bytes(), 'Downloaded loader bytes differ')
        self.run('shell-syntax', ['sh', '-n', self.script])
        target = self.scratch / 'product $(touch injected) with spaces'
        env = self.environment('native-current')
        self.boot('raw-ELF-bootstrap-init', 'init', target, '--app', 'api=rust-api', '--release-root', self.payload,
            '--package-source', 'local', '--non-interactive', env=env)
        state = json.loads((target / '.devkit/manifest.json').read_text())
        require(state['version'] == self.version and not (self.scratch / 'injected').exists(), 'Wrong accepted version or shell expansion')
        self.boot('cached-bootstrap-offline-metadata', 'describe', '--root', target, env=dict(env, HTTPS_PROXY='http://127.0.0.1:1'))
        self.boot('recover-local-journal-no-cache', 'recover', target, env=self.environment('empty'))
        self.cold_old('genuine-legacy-http-init')
        legacy = self.scratch / 'genuine-legacy-http-init'
        source = legacy / 'apps/api/src/main.rs'
        source.write_text(source.read_text() + '\n// Preserve the product change.\n')
        (legacy / '.env').write_text('PRODUCT_SENTINEL=preserve\n')
        before = {p: p.read_bytes() for p in (source, legacy / '.env', legacy / 'Makefile', legacy / 'compose.yaml')}
        self.run('native-accepted-legacy-add', [self.engine, 'add-app', legacy, '--name', 'worker', '--template', 'rust-api', '--non-interactive'], env=self.environment('genuine-legacy-http-init'))
        self.run('explicit-legacy-preview', [self.engine, 'update', legacy, '--to', self.version, '--release-root', self.payload, '--preview', '--non-interactive'])
        self.run('explicit-legacy-native-update', [self.engine, 'update', legacy, '--to', self.version, '--release-root', self.payload, '--non-interactive'])
        require(all(p.read_bytes() == data for p, data in before.items()), 'Migration changed product inputs')
        require(json.loads((legacy / '.devkit/manifest.json').read_text())['version'] == self.version, 'Migration version differs')
    def finish(self, passed):
        self.fixture.close()
        require(self.source_before == {str(p.relative_to(self.build_set)): digest(p.read_bytes()) for p in self.build_set.rglob('*') if p.is_file()}, 'Modified source build-set')
        require(digest(self.legacy.read_bytes()) == self.legacy_before, 'Modified legacy archive')
        (self.logs / 'receipt.json').write_text(json.dumps(dict(passed=passed, cases=self.results, hostSdkAttempts=0,
            sourceCommit=self.index['sourceCommit'], sourceTree=self.index['sourceTree'], releaseIdentity=self.index['releaseIdentity'],
            legacyArchiveSha256=self.legacy_before, engineSha256=digest(self.engine.read_bytes()),
            limits=['Temporary local CA/proxy; immutable input bytes. Public transport is a separate release gate.']), indent=2) + '\n')
        self.temp.cleanup()
