#!/usr/bin/env python3
"""Shared native bootstrap fixture: real TLS/proxy, raw ELF, immutable v0.4.1 baseline."""
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

BASELINE_VERSION = '0.4.1'
BASELINE_INDEX_SHA256 = '0f95ce2c5a51c9f17e6439f7e2c5cf6bc185ed5657aba5a676a558d9806389aa'
BASELINE_INDEX_SIZE = 2255
BASELINE_RELEASE_IDENTITY = '0c2f111d8d53cd95aaa9dc7ed6e34e4c764d96ea4f537a7e758ee2f44e5b8a5a'
BASELINE_RELEASE_SIZE = 58547
BASELINE_ARCHIVE_SHA256 = '4c76c7ffd0ab48c710512af9874f8a1bc189bb663b939daa6ddc76522511143a'
BASELINE_ARCHIVE_SIZE = 4003689
BASELINE_ENGINE_SHA256 = '206f66322e7ae009bc9368a05df64cfef0285ec30e32e9b9dbf0ffbe1069f929'
BASELINE_ENGINE_SIZE = 7960008


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
    def __init__(self, build_set, baseline_set):
        self.build_set = Path(build_set).resolve()
        self.payload = self.build_set / 'payload'
        self.index = json.loads((self.build_set / 'release-index.json').read_text())
        self.version = self.index['version']
        self.engine = self.build_set / self.index['engine']
        for name, identity in self.index['files'].items():
            data = (self.build_set / name).read_bytes()
            require(len(data) == identity['size'] and digest(data) == identity['sha256'], f'Build-set identity differs: {name}')
        self.source_before = {str(p.relative_to(self.build_set)): digest(p.read_bytes()) for p in self.build_set.rglob('*') if p.is_file()}
        self.baseline = Path(baseline_set).resolve()
        index_bytes = (self.baseline / 'release-index.json').read_bytes()
        require(len(index_bytes) == BASELINE_INDEX_SIZE and digest(index_bytes) == BASELINE_INDEX_SHA256, 'Baseline release-index.json identity differs')
        self.old_index = json.loads(index_bytes)
        self.old_version = self.old_index['version']
        require(self.old_version == BASELINE_VERSION and self.old_index['publicationEligible'] is True, 'Unexpected baseline release-index metadata')
        require(self.old_index['releaseIdentity'] == BASELINE_RELEASE_IDENTITY, 'Unexpected baseline releaseIdentity')
        archive_name = self.old_index['archive']
        engine_name = self.old_index['engine']
        self.baseline_archive = self.baseline / archive_name
        archive_bytes = self.baseline_archive.read_bytes()
        engine_bytes = (self.baseline / engine_name).read_bytes()
        require(
            len(archive_bytes) == BASELINE_ARCHIVE_SIZE
            and digest(archive_bytes) == BASELINE_ARCHIVE_SHA256
            and self.old_index['files'][archive_name] == {'sha256': BASELINE_ARCHIVE_SHA256, 'size': BASELINE_ARCHIVE_SIZE},
            'Baseline archive identity differs',
        )
        require(
            len(engine_bytes) == BASELINE_ENGINE_SIZE
            and digest(engine_bytes) == BASELINE_ENGINE_SHA256
            and self.old_index['files'][engine_name] == {'sha256': BASELINE_ENGINE_SHA256, 'size': BASELINE_ENGINE_SIZE},
            'Baseline engine identity differs',
        )
        with tarfile.open(self.baseline_archive, 'r:gz') as archive:
            names = [e for e in archive.getmembers() if e.name.endswith('/release.json')]
            require(len(names) == 1 and names[0].isfile(), 'Expected one genuine baseline manifest')
            manifest_bytes = archive.extractfile(names[0]).read()
        require(len(manifest_bytes) == BASELINE_RELEASE_SIZE and digest(manifest_bytes) == BASELINE_RELEASE_IDENTITY, 'Baseline manifest inside archive differs')
        if (self.baseline / 'payload/release.json').exists():
            payload_manifest = (self.baseline / 'payload/release.json').read_bytes()
            require(len(payload_manifest) == BASELINE_RELEASE_SIZE and digest(payload_manifest) == BASELINE_RELEASE_IDENTITY, 'Unpacked baseline payload/release.json differs')
        self.baseline_before = {
            name: digest((self.baseline / name).read_bytes())
            for name in ('release-index.json', archive_name, engine_name)
        }
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
        self.old_index_route = f'{self.old_base}/release-index.json'
        self.archive_route = f'{self.old_base}/{archive_name}'
        self.old_engine_route = f'{self.old_base}/{engine_name}'
        self.fixture.routes[self.old_index_route] = index_bytes
        self.fixture.routes[self.archive_route] = archive_bytes
        self.fixture.routes[self.old_engine_route] = engine_bytes
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
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout,
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
        self.cold_old('genuine-baseline-http-init')
        baseline = self.scratch / 'genuine-baseline-http-init'
        source = baseline / 'apps/api/src/main.rs'
        source.write_text(source.read_text() + '\n// Preserve the product change.\n')
        (baseline / '.env').write_text('PRODUCT_SENTINEL=preserve\n')
        before = {p: p.read_bytes() for p in (source, baseline / '.env', baseline / 'Makefile', baseline / 'compose.yaml')}
        self.run('native-accepted-baseline-add', [self.engine, 'add-app', baseline, '--name', 'worker', '--template', 'rust-api', '--non-interactive'], env=self.environment('genuine-baseline-http-init'))
        self.run('explicit-baseline-preview', [self.engine, 'update', baseline, '--to', self.version, '--release-root', self.payload, '--preview', '--non-interactive'])
        self.run('explicit-baseline-native-update', [self.engine, 'update', baseline, '--to', self.version, '--release-root', self.payload, '--non-interactive'])
        require(all(p.read_bytes() == data for p, data in before.items()), 'Migration changed product inputs')
        require(json.loads((baseline / '.devkit/manifest.json').read_text())['version'] == self.version, 'Migration version differs')
        pre_040 = self.scratch / 'pre-040-checkout'
        (pre_040 / '.devkit').mkdir(parents=True)
        (pre_040 / '.devkit/manifest.json').write_text(json.dumps({
            'schemaVersion': 2,
            'version': '0.3.2',
            'releaseIdentity': '0' * 64,
            'generation': 'gen-0',
            'packageMode': 'local',
            'managed': {},
            'components': {},
        }))
        self.run('refuse-pre-040-project-zero-mutation', [self.engine, 'status', '--root', pre_040], expected='0.4.0')
        self.run('refuse-pre-040-update-zero-mutation', [self.engine, 'update', pre_040, '--to', self.version, '--release-root', self.payload, '--non-interactive'], expected='0.4.1')
        require(not (pre_040 / '.devkit/state').exists(), 'Pre-0.4.0 refusal created lock or state directory')
    def finish(self, passed):
        self.fixture.close()
        require(self.source_before == {str(p.relative_to(self.build_set)): digest(p.read_bytes()) for p in self.build_set.rglob('*') if p.is_file()}, 'Modified source build-set')
        require(self.baseline_before == {name: digest((self.baseline / name).read_bytes()) for name in self.baseline_before}, 'Modified baseline set')
        (self.logs / 'receipt.json').write_text(json.dumps(dict(passed=passed, cases=self.results, hostSdkAttempts=0,
            sourceCommit=self.index['sourceCommit'], sourceTree=self.index['sourceTree'], releaseIdentity=self.index['releaseIdentity'],
            baselineArchiveSha256=BASELINE_ARCHIVE_SHA256, engineSha256=digest(self.engine.read_bytes()),
            limits=['Temporary local CA/proxy; immutable input bytes. Public transport is a separate release gate.']), indent=2) + '\n')
        self.temp.cleanup()
