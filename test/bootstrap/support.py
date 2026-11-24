#!/usr/bin/env python3
"""Shared native bootstrap fixture: real TLS/proxy, raw ELF, immutable v0.5.1 baseline."""
from __future__ import annotations
from concurrent.futures import ThreadPoolExecutor
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
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import uuid

AUTHOR = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(AUTHOR / 'test/delivery'))
from test_delivery import HttpsFixture, worktrunk  # noqa: E402
sys.path.insert(0, str(AUTHOR / 'test'))
from release_targets import engine_asset, host_target  # noqa: E402

BASELINE_VERSION = '0.5.1'
BASELINE_INDEX_SHA256 = 'c55c5b5e0457f5c934079a8bbce272545b1cdbc2b7da25061bd7a95d2780d08a'
BASELINE_INDEX_SIZE = 2258
BASELINE_RELEASE_IDENTITY = '870146c4055749dd0e4555e49605c3dcd697444f481ef197ff043796c33df74c'
BASELINE_RELEASE_SIZE = 65946
BASELINE_ARCHIVE_SHA256 = 'f8b8a3bf4aea83c932d667d1c8e08779c610ab6999b1144ad6d5fbc8b7cf8f1b'
BASELINE_ARCHIVE_SIZE = 5145930
BASELINE_ENGINE_SHA256 = '8388c1c2105b7f4b952277922ecb82747ab81d506a66d6d439258ae8196ef479'
BASELINE_ENGINE_SIZE = 10589864


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
        asset = engine_asset(self.index)
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
        # A raw downloaded engine has no adjacent release.json. Exercise the actual
        # network acquisition path even when the requested version is the current one.
        self.engine = self.scratch / asset
        shutil.copy2(self.build_set / asset, self.engine)
        self.engine.chmod(0o755)
        self.logs = AUTHOR / '.artifacts/bootstrap' / uuid.uuid4().hex
        self.logs.mkdir(parents=True)
        self.fixture = Fixture(self.scratch / 'tls')
        self.base = '/docouno/devkit/releases/download'
        self.engine_route = f'{self.base}/v{self.version}/{asset}'
        self.fixture.routes[self.engine_route] = self.engine.read_bytes()
        self.index_route = f'{self.base}/v{self.version}/release-index.json'
        self.archive_route = f'{self.base}/v{self.version}/{self.index["archive"]}'
        self.fixture.routes[self.index_route] = (self.build_set / 'release-index.json').read_bytes()
        self.fixture.routes[self.archive_route] = (self.build_set / self.index['archive']).read_bytes()
        self.fixture.routes[f'{self.base}/v{self.version}/install.sh'] = (self.build_set / 'install.sh').read_bytes()
        self.old_base = f'{self.base}/v{self.old_version}'
        self.old_index_route = f'{self.old_base}/release-index.json'
        self.old_archive_route = f'{self.old_base}/{archive_name}'
        self.old_engine_route = f'{self.old_base}/{engine_name}'
        self.fixture.routes[self.old_index_route] = index_bytes
        self.fixture.routes[self.old_archive_route] = archive_bytes
        self.fixture.routes[self.old_engine_route] = engine_bytes
        self.forbidden = self.scratch / 'host-sdk-attempts'
        blockers = self.scratch / 'without host SDK'
        blockers.mkdir()
        for name in ('node', 'npm', 'npx', 'pnpm', 'pnpx', 'corepack', 'cargo', 'rustc', 'rustup', 'tsc', 'eslint'):
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
    def run(self, name, argv, *, expected=None, env=None, timeout=45, memory_limit=None, cwd=None):
        started = time.monotonic()
        result = subprocess.run([str(v) for v in argv], env=env or self.environment(name), cwd=cwd or self.scratch,
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
    def cold_init(self, name, version, *, expected=None):
        target = self.scratch / name
        result = self.run(name, [self.engine, 'init', target, '--to', version, '--app', 'api=rust-api', '--package-source', 'local', '--non-interactive'], expected=expected, memory_limit=512 * 1024 * 1024 if expected is not None else None)
        cache = self.scratch / ('cache ' + name)
        require(not list(cache.glob('.acquire-*')), 'Unaccepted acquisition directory leaked')
        if expected is not None:
            require(not target.exists(), 'Rejected archive wrote the project')
        return result
    def verify_concurrent_bootstrap(self):
        env = self.environment('simultaneous installs')
        before = len(self.fixture.requests)
        def install(_):
            return subprocess.run(['sh', self.script, '--version'], cwd=self.scratch, env=env,
                                  stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                  umask=0o077, timeout=25)
        with ThreadPoolExecutor(max_workers=3) as pool:
            for result in pool.map(install, range(3)):
                require(result.returncode == 0 and self.version in result.stdout, result.stderr)
        require(set(self.fixture.requests[before:]) == {self.engine_route},
                'Cold installers requested another target or release asset')
        fingerprint = self.index['files'][engine_asset(self.index)]['sha256']
        cache = Path(env['DEVKIT_CACHE_DIR'])
        binary = cache / 'engines' / host_target() / self.version / fingerprint / 'devkit'
        require(digest(binary.read_bytes()) == fingerprint and stat.S_IMODE(binary.stat().st_mode) == 0o755,
                'Concurrent strict-umask installation did not publish the pinned executable')
        require(not list(cache.rglob('.engine.*')), 'Concurrent engine installation left partial files')
        before = len(self.fixture.requests)
        binary.chmod(0o644)
        self.boot('refuse-unexecutable-cache', '--version', expected='cached engine integrity differs', env=env)
        require(len(self.fixture.requests) == before, 'An unusable cache entry was silently refetched')
    def verify_cold_worktree(self, target):
        wt = worktrunk(self.scratch)
        worktrees = self.scratch / 'worktree paths'
        env = dict(self.environment('cold-worktrunk'),
                   XDG_CONFIG_HOME=str(self.scratch / 'worktrunk config'),
                   WORKTRUNK_WORKTREE_PATH=str(worktrees / '{{ branch | sanitize }}'),
                   DOCKER_HOST=f'unix://{self.scratch / "unavailable-docker.sock"}')
        def run(name, argv, **kwargs):
            return self.run(name, argv, env=env, cwd=target, **kwargs)
        run('worktrunk-git-init', ['git', 'init', '-b', 'main'])
        run('worktrunk-git-add', ['git', 'add', '.'])
        run('worktrunk-git-commit', ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'Create test checkout'])
        sw = [wt, 'switch', '--create', '--base', 'main', '--no-cd']
        before = self.fixture.requests.count(self.engine_route)
        unchecked = subprocess.run([str(v) for v in [*sw, 'unapproved']], env=env, cwd=target,
                                   stdin=subprocess.DEVNULL, capture_output=True, timeout=15)
        require(not (worktrees / 'unapproved/.devkit/state/ports.json').exists(), 'Unapproved Worktrunk hook reserved a port')
        require(self.fixture.requests.count(self.engine_route) == before, 'Unapproved Worktrunk ran a cold download')
        require(unchecked.returncode != 0 or (worktrees / 'unapproved').is_dir(), 'Unexpected unapproved Worktrunk outcome')
        if (worktrees / 'unapproved').is_dir():
            run('worktrunk-remove-unapproved', [wt, 'remove', '--foreground', '--no-hooks', 'unapproved'])
        run('worktrunk-create-cold', [*sw, '--yes', 'approved'])
        require(self.fixture.requests.count(self.engine_route) == before + 1, 'Cold Worktrunk hook did not fetch the exact engine')
        approved = worktrees / 'approved'
        info = self.run('worktrunk-cold-lease', ['make', 'info', 'json=1'], env=env, cwd=approved)
        worktree = json.loads(info.stdout)['result']
        require(worktree['standState'] == 'stopped' and len(worktree['standPorts']) == 1, 'Cold Worktrunk reservation was not available on return')
        run('worktrunk-remove-cold', [wt, 'remove', '--foreground', '--yes', 'approved'])
        require(not approved.exists(), 'Worktrunk did not complete removal')
    def verify_node_recovery(self):
        target = self.scratch / 'genuine-node-migration'
        env = self.environment('genuine-node-migration')
        sdk_env = {key: value for key, value in env.items() if key not in ('HTTPS_PROXY', 'SSL_CERT_FILE', 'CURL_CA_BUNDLE')}
        self.run('prime-genuine-node-pnpm', [self.engine, 'internal', 'ensure-pnpm', '--pin-file', self.payload / 'runtime/pnpm.env'], env=sdk_env, timeout=150)
        self.run('genuine-node-init', [self.engine, 'init', target, '--to', self.old_version,
                                     '--app', 'web=react-web', '--package-source', 'local', '--non-interactive'], env=env, timeout=360)
        saved = self.scratch / 'genuine-node-before'
        shutil.copytree(target, saved)
        loader = '.devkit/runtime/devkit.sh'
        projection = '.devkit/runtime/devkit.mk'
        manifest = '.devkit/manifest.json'
        self.run('genuine-node-upgrade', ['sh', loader, 'cli', 'update', '.', '--to', self.version, '--non-interactive'],
                 cwd=target, env=env, timeout=360)
        old = json.loads((saved / manifest).read_bytes())
        new = json.loads((target / manifest).read_bytes())
        require(old['version'] == self.old_version and new['version'] == self.version, 'Unexpected Node migration fixture pins')
        paths = sorted(set(old['managed']) | set(new['managed']) | {
            'package.json', 'apps/web/package.json', 'package-lock.json', 'pnpm-lock.yaml', 'pnpm-workspace.yaml', '.npmrc'}) + [manifest]
        def capture(root, path):
            absolute = root / path
            if absolute.is_file():
                return absolute.read_bytes(), stat.S_IMODE(absolute.stat().st_mode)
            return None
        before = {path: capture(saved, path) for path in paths}
        after = {path: capture(target, path) for path in paths}
        require(before['pnpm-lock.yaml'] is not None and after['pnpm-lock.yaml'] is not None
                and before['package-lock.json'] is None and after['package-lock.json'] is None,
                'Expected a genuine pnpm-to-pnpm upgrade')
        def install(root, path, recorded):
            absolute = root / path
            if recorded is None:
                absolute.unlink(missing_ok=True)
                return None
            data, mode = recorded
            absolute.parent.mkdir(parents=True, exist_ok=True)
            absolute.write_bytes(data)
            absolute.chmod(mode)
            return {'sha256': digest(data), 'mode': mode}
        for variant, new_make, new_loader in [('old-old', False, False), ('new-old', True, False), ('new-new', True, True)]:
            root = self.scratch / ('mixed-node-' + variant)
            shutil.copytree(target, root)
            stage = '.devkit/state/update-' + str(uuid.uuid4())
            journal = {'schemaVersion': 2, 'stage': stage, 'files': [], 'createdDirectories': [], 'createdParents': []}
            for path in paths:
                if before[path] == after[path]:
                    continue
                i = len(journal['files'])
                backup = f'{stage}/backup/{i}' if before[path] else None
                staged = f'{stage}/staged/{i}' if after[path] else None
                row = {'path': path, 'before': install(root, backup, before[path]) if backup else None,
                       'after': install(root, staged, after[path]) if staged else None, 'backup': backup, 'staged': staged}
                journal['files'].append(row)
            if not new_make:
                install(root, projection, before[projection])
            if not new_loader:
                install(root, loader, before[loader])
            (root / '.devkit/update-pending.json').write_text(json.dumps(journal))
            if new_loader:
                self.run('make-recover-mixed-node-' + variant, ['make', 'recover'], cwd=root, env=env)
            else:
                self.boot('bootstrap-recover-mixed-node-' + variant, 'recover', root, env=env)
            require(all(capture(root, path) == before[path] for path in paths), 'Mixed-state rollback changed original Node files')
            require(not (root / '.devkit/update-pending.json').exists(), 'Mixed-state rollback left a pending journal')
    def execute(self):
        self.run('download-versioned-loader', ['curl', '--disable', '--fail', '--silent', '--show-error', '--output', self.script,
            f'https://github.com{self.base}/v{self.version}/install.sh'])
        require(self.script.read_bytes() == (self.build_set / 'install.sh').read_bytes(), 'Downloaded loader bytes differ')
        self.run('shell-syntax', ['sh', '-n', self.script])
        self.verify_concurrent_bootstrap()
        target = self.scratch / 'product $(touch injected) with spaces'
        env = self.environment('native-current')
        self.boot('raw-ELF-bootstrap-init', 'init', target, '--app', 'api=rust-api', '--release-root', self.payload,
            '--package-source', 'local', '--non-interactive', env=env)
        state = json.loads((target / '.devkit/manifest.json').read_text())
        require(state['version'] == self.version and not (self.scratch / 'injected').exists(), 'Wrong accepted version or shell expansion')
        self.boot('cached-bootstrap-offline-metadata', 'describe', '--root', target, env=dict(env, HTTPS_PROXY='http://127.0.0.1:1'))
        self.boot('recover-local-journal-no-cache', 'recover', target, env=self.environment('empty'))
        self.verify_cold_worktree(target)
        if host_target() == self.old_index['target']:
            self.verify_baseline_upgrade()
        else:
            before = len(self.fixture.requests)
            self.cold_init('refuse-unavailable-baseline-target', self.old_version, expected=host_target())
            require(self.fixture.requests[before:] == [self.old_index_route],
                    'An incompatible release must be refused from its index before downloading binaries')
        self.verify_version_floor()

    def verify_baseline_upgrade(self):
        self.cold_init('genuine-baseline-http-init', self.old_version)
        baseline = self.scratch / 'genuine-baseline-http-init'
        source = baseline / 'apps/api/src/main.rs'
        source.write_text(source.read_text() + '\n// Preserve the product change.\n')
        (baseline / '.env').write_text('PRODUCT_SENTINEL=preserve\n')
        before = {p: p.read_bytes() for p in (source, baseline / '.env', baseline / 'Makefile', baseline / 'compose.yaml', baseline / 'AGENTS.md', baseline / '.config/wt.toml')}
        self.run('native-accepted-baseline-add', [self.engine, 'add-app', baseline, '--name', 'worker', '--template', 'rust-api', '--non-interactive'], env=self.environment('genuine-baseline-http-init'))
        bridge_env = self.environment('genuine-baseline-http-init')
        old_loader = baseline / '.devkit/runtime/devkit.sh'
        old_bytes = old_loader.read_bytes()
        bridge = ['sh', old_loader, 'cli', 'update', '.', '--to', self.version, '--non-interactive']
        self.run('genuine-project-loader-preview', [*bridge, '--preview'], cwd=baseline, env=bridge_env)
        require(old_loader.read_bytes() == old_bytes, 'Preview changed the checked-in loader')
        self.run('genuine-project-loader-update', bridge, cwd=baseline, env=bridge_env)
        require(all(p.read_bytes() == data for p, data in before.items()), 'Migration changed product inputs')
        require(json.loads((baseline / '.devkit/manifest.json').read_text())['version'] == self.version, 'Migration version differs')
        info = self.run('upgraded-make-info', ['make', 'info', 'json=1'], cwd=baseline, env=bridge_env)
        require(json.loads(info.stdout)['result']['version'] == self.version, 'Upgraded Make entry point selected another version')
        self.verify_node_recovery()

    def verify_version_floor(self):
        pre_050 = self.scratch / 'pre-050-checkout'
        (pre_050 / '.devkit').mkdir(parents=True)
        (pre_050 / '.devkit/manifest.json').write_text(json.dumps({
            'schemaVersion': 2,
            'version': '0.4.1',
            'releaseIdentity': '0' * 64,
            'generation': 'gen-0',
            'packageMode': 'local',
            'managed': {},
            'components': {},
        }))
        pre_050_before = {str(p.relative_to(pre_050)): (p.is_dir(), p.read_bytes() if p.is_file() else b'') for p in sorted(pre_050.rglob('*'))}
        requests_before = len(self.fixture.requests)
        self.run('refuse-pre-050-project-zero-mutation', [self.engine, 'status', '--root', pre_050], expected='0.5.0')
        self.run('refuse-pre-050-update-zero-mutation', [self.engine, 'update', pre_050, '--to', self.old_version, '--non-interactive'], expected='0.5.1')
        require(len(self.fixture.requests) == requests_before, 'Pre-0.5.0 refusal issued unexpected release HTTP requests')
        pre_050_after = {str(p.relative_to(pre_050)): (p.is_dir(), p.read_bytes() if p.is_file() else b'') for p in sorted(pre_050.rglob('*'))}
        require(pre_050_after == pre_050_before, 'Pre-0.5.0 refusal mutated project checkout')
        require(not (self.scratch / 'cache refuse-pre-050-project-zero-mutation').exists(), 'Pre-0.5.0 status refusal created cache directory')
        require(not (self.scratch / 'cache refuse-pre-050-update-zero-mutation').exists(), 'Pre-0.5.0 update refusal created cache directory')
    def finish(self, passed):
        self.fixture.close()
        require(self.source_before == {str(p.relative_to(self.build_set)): digest(p.read_bytes()) for p in self.build_set.rglob('*') if p.is_file()}, 'Modified source build-set')
        require(self.baseline_before == {name: digest((self.baseline / name).read_bytes()) for name in self.baseline_before}, 'Modified baseline set')
        (self.logs / 'receipt.json').write_text(json.dumps(dict(passed=passed, cases=self.results, hostSdkAttempts=0,
            sourceCommit=self.index['sourceCommit'], sourceTree=self.index['sourceTree'], releaseIdentity=self.index['releaseIdentity'],
            baselineArchiveSha256=BASELINE_ARCHIVE_SHA256, engineSha256=digest(self.engine.read_bytes()),
            limits=['Temporary local CA/proxy; immutable input bytes. Public transport is a separate release gate.']), indent=2) + '\n')
        self.temp.cleanup()
