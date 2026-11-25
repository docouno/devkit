#!/usr/bin/env python3
"""Native loader/download/archive bounds and cancellation regressions."""
from __future__ import annotations
import copy
import gzip
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import tarfile
import time
from support import Harness, digest, require
from release_targets import engine_asset, host_target


class Safety(Harness):
    def archive(self, entries):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w:gz', format=tarfile.GNU_FORMAT) as archive:
            for name, kind, mode, data in entries:
                item = tarfile.TarInfo(name)
                item.type, item.mode, item.size = kind, mode, len(data)
                archive.addfile(item, io.BytesIO(data))
        return stream.getvalue()
    def index_for_archive(self, *, sha256, size):
        patched = copy.deepcopy(self.transport_index)
        patched['files'][self.transport_index['archive']] = {'sha256': sha256, 'size': size}
        return json.dumps(patched).encode()
    def reject_archive(self, name, data):
        original_archive = self.fixture.routes[self.transport_archive_route]
        original_index = self.fixture.routes[self.transport_index_route]
        self.fixture.routes[self.transport_archive_route] = data
        self.fixture.routes[self.transport_index_route] = self.index_for_archive(sha256=digest(data), size=len(data))
        try:
            before = len(self.fixture.requests)
            result = self.cold_init(name, self.transport_version, expected='devkit:')
            require(self.fixture.requests[before:] == [self.transport_index_route, self.transport_archive_route],
                    f'{name} did not reach the selected archive')
            require(
                'Downloaded archive checksum mismatch' not in result.stderr
                and 'Invalid release asset' not in result.stderr
                and 'Release index' not in result.stderr
                and 'Downloaded payload differs' not in result.stderr,
                f'Archive attack stopped outside archive admission: {result.stderr}',
            )
        finally:
            self.fixture.routes[self.transport_archive_route] = original_archive
            self.fixture.routes[self.transport_index_route] = original_index
    def cancel(self, name, argv, route):
        self.fixture.ready.clear(); self.fixture.resume.clear()
        original = self.fixture.routes.get(route)
        self.fixture.routes[route] = self.fixture.stall
        env = self.environment(name)
        process = subprocess.Popen([str(v) for v in argv], env=env, cwd=self.scratch, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        children = []
        try:
            require(self.fixture.ready.wait(10), 'Download never reached its real TLS endpoint')
            leaf = Path(f'/proc/{process.pid}/task/{process.pid}/children')
            children = [int(v) for v in leaf.read_text().split()]
            require(children, 'No real supervised download child')
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=8)
            require(process.returncode == 143, f'Cancellation exit {process.returncode}: {stdout}{stderr}')
            require(all(not Path(f'/proc/{pid}').exists() for pid in children), 'A supervised download child survived')
            cache = Path(env['DEVKIT_CACHE_DIR'])
            require(not list(cache.rglob('.engine.*')) and not list(cache.glob('.acquire-*')), 'Cancelled acquisition left partial files')
            require(not (self.scratch / name).exists(), 'Cancelled operation wrote a project')
            self.results.append(dict(name=name, passed=True, exitCode=process.returncode, childrenReaped=children))
            print('PASS ' + name, flush=True)
        finally:
            self.fixture.resume.set()
            if process.poll() is None:
                process.kill(); process.communicate(timeout=5)
            if original is None: self.fixture.routes.pop(route, None)
            else: self.fixture.routes[route] = original
    def execute(self):
        self.script.write_bytes((self.build_set / 'install.sh').read_bytes())
        real_engine = self.fixture.routes[self.engine_route]
        for name, body, expected in [('missing-native-ELF', None, '404'), ('corrupt-native-ELF', real_engine[:-1] + bytes([real_engine[-1] ^ 1]), 'checksum differs'), ('truncated-native-ELF', real_engine[:128], 'checksum differs')]:
            self.fixture.routes[self.engine_route] = body
            self.boot(name, '--version', expected=expected)
            cache = self.scratch / ('cache ' + name)
            require(not list(cache.rglob('devkit')) and not list(cache.rglob('.engine.*')), 'Bad ELF became a cached engine')
        self.fixture.routes[self.engine_route] = real_engine
        self.cancel('cancel-initial-curl', ['sh', self.script, '--version'], self.engine_route)
        self.cancel('cancel-native-HTTPS-worker', [self.engine, 'init', self.scratch / 'cancel-native-HTTPS-worker', '--to', self.version, '--app', 'api=rust-api', '--non-interactive'], self.index_route)
        # The native HTTP reader has one bounded path for known/unknown lengths.
        def unknown(handler):
            handler.send_response(200); handler.send_header('Connection', 'close'); handler.end_headers()
            try: handler.wfile.write(b'x' * 4096); handler.wfile.flush()
            except (BrokenPipeError, ConnectionResetError): pass
            handler.close_connection = True
        self.fixture.routes['/unknown-length'] = unknown
        output = self.scratch / 'fetch-output'
        self.run('unknown-length-native-byte-limit', [self.engine, 'internal', 'fetch', '--url', 'https://github.com/unknown-length', '--output', output, '--max-bytes', '128'], expected='byte budget')
        require(not output.exists(), 'Rejected download left partial output')
        def redirect(handler):
            handler.send_response(302); handler.send_header('Location', 'http://127.0.0.1:1/denied'); handler.send_header('Content-Length', '0'); handler.end_headers()
        self.fixture.routes['/downgrade'] = redirect
        self.run('deny-HTTP-redirect', [self.engine, 'internal', 'fetch', '--url', 'https://github.com/downgrade', '--output', output, '--max-bytes', '128'], expected='devkit:')
        require(not output.exists(), 'Downgrade published a file')
        self.fixture.routes['/deadline'] = self.fixture.stall
        started = time.monotonic()
        self.run('native-reader-deadline', [self.engine, 'internal', 'fetch', '--url', 'https://github.com/deadline', '--output', output, '--max-bytes', '128'], expected='devkit:', timeout=16)
        require(time.monotonic() - started < 15 and not output.exists(), 'Stalled reader outlived its bound')
        self.fixture.resume.set()
        self.verify_archive_transport()
        # Oversized metadata is refused before any project write, lock, or SDK process.
        project = self.scratch / 'oversized manifest'
        (project / '.devkit').mkdir(parents=True)
        (project / '.devkit/manifest.json').write_bytes(b'{"schemaVersion":2,"pad":"' + b'a' * (4 * 1024 * 1024) + b'"}')
        self.run('manifest-input-bound', [self.engine, 'register', project, '--app', 'worker', '--profile', 'rust', '--non-interactive'], expected='4 MiB byte budget')
        require(not (project / '.devkit/state').exists(), 'Oversized manifest created state or lock directory')
        require(not self.forbidden.exists(), 'Safety gate invoked a host SDK')

    def verify_archive_transport(self):
        host = host_target()
        index = self.index
        if not index['publicationEligible'] and host == self.old_index['target']:
            index = self.old_index
        self.transport_version = index['version']
        base = f'{self.base}/v{self.transport_version}'
        self.transport_archive_route = f'{base}/{index["archive"]}'
        self.transport_index_route = f'{base}/release-index.json'
        original = self.fixture.routes[self.transport_index_route]
        self.transport_index = index
        if not index['publicationEligible']:
            # The historical release has no engine for this host. Only the
            # private archive test server advertises the host-compatible bytes.
            require(engine_asset(index, host) in index['files'], f'Missing preview engine for {host}')
            self.transport_index = dict(index, publicationEligible=True, buildRun='loopback-archive-test')
            self.fixture.routes[self.transport_index_route] = json.dumps(self.transport_index).encode()
        try:
            self.verify_archives()
        finally:
            self.fixture.routes[self.transport_index_route] = original

    def verify_archives(self):
        original_archive = self.fixture.routes[self.transport_archive_route]
        original_index = self.fixture.routes[self.transport_index_route]
        try:
            self.fixture.routes[self.transport_archive_route] = None
            self.cold_init('missing-release-archive', self.transport_version, expected='devkit:')
            def partial(handler):
                handler.send_response(200); handler.send_header('Content-Length', str(len(original_archive))); handler.end_headers()
                handler.wfile.write(original_archive[:128]); handler.wfile.flush(); handler.close_connection = True
            self.fixture.routes[self.transport_archive_route] = partial
            self.cold_init('truncated-HTTP-archive', self.transport_version, expected='devkit:')
            self.fixture.routes[self.transport_archive_route] = original_archive
            self.fixture.routes[self.transport_index_route] = self.index_for_archive(sha256='0' * 64, size=len(original_archive))
            self.cold_init('archive-checksum-mismatch', self.transport_version, expected='Downloaded archive checksum mismatch')
        finally:
            self.fixture.routes[self.transport_archive_route] = original_archive
            self.fixture.routes[self.transport_index_route] = original_index
        prefix = f'devkit-{self.transport_version}/'
        base = [(prefix, tarfile.DIRTYPE, 0o755, b'')]
        for name, path, kind, mode in (
            ('traversal', prefix + '../escape', tarfile.REGTYPE, 0o644),
            ('absolute', '/tmp/escape', tarfile.REGTYPE, 0o644),
            ('newline', prefix + 'a\nb', tarfile.REGTYPE, 0o644),
            ('symlink', prefix + 'link', tarfile.SYMTYPE, 0o755),
            ('hardlink', prefix + 'link', tarfile.LNKTYPE, 0o644),
            ('fifo', prefix + 'pipe', tarfile.FIFOTYPE, 0o644),
            ('device', prefix + 'device', tarfile.CHRTYPE, 0o644),
            ('setuid', prefix + 'mode', tarfile.REGTYPE, 0o4755),
            ('wrong-root', 'other/file', tarfile.REGTYPE, 0o644)):
            self.reject_archive(name, self.archive(base + [(path, kind, mode, b'')]))
        item = (prefix + 'duplicate', tarfile.REGTYPE, 0o644, b'a')
        self.reject_archive('duplicate-entry', self.archive(base + [item, item]))
        self.reject_archive('native-entry-budget', self.archive(base + [(prefix + str(i), tarfile.REGTYPE, 0o644, b'') for i in range(16385)]))
        # Giant declared sizes exercise admission without creating giant fixture files.
        for name, kind, size in [('native-file-budget', tarfile.REGTYPE, 64 * 1024 * 1024 + 1), ('extended-header-bound', tarfile.GNUTYPE_LONGNAME, 1024 * 1024 * 1024)]:
            header = tarfile.TarInfo(prefix + 'giant')
            header.type, header.mode, header.size = kind, 0o644, size
            self.reject_archive(name, gzip.compress(header.tobuf(format=tarfile.GNU_FORMAT) + bytes(10240)))
        long_path = prefix + ('a' * 120 + '/') * 36 + 'file'
        self.reject_archive('metadata-path-budget', self.archive(base + [(long_path, tarfile.REGTYPE, 0o644, b'')]))
        class Zeros:
            def read(self, size):
                return bytes(size)
        expanded = io.BytesIO()
        with tarfile.open(fileobj=expanded, mode='w:gz', format=tarfile.USTAR_FORMAT) as archive:
            for index in range(5):
                item = tarfile.TarInfo(prefix + f'large-{index}')
                item.mode, item.size = 0o644, 60 * 1024 * 1024
                archive.addfile(item, Zeros())
        expanded_bytes = expanded.getvalue()
        self.reject_archive('native-expanded-budget', expanded_bytes)
        # Interrupt the actual in-process archive reader, with no external tar.
        # Mount the 300 MiB zero-filled archive so extraction streams actively on disk
        # and cannot finish or exec into the target binary before SIGTERM arrives.
        env = self.environment('cancel-native-extraction')
        target = self.scratch / 'cancel-native-extraction'
        self.fixture.routes[self.transport_archive_route] = expanded_bytes
        self.fixture.routes[self.transport_index_route] = self.index_for_archive(sha256=digest(expanded_bytes), size=len(expanded_bytes))
        process = subprocess.Popen([str(self.engine), 'init', str(target), '--to', self.transport_version,
            '--app', 'api=rust-api', '--non-interactive'], cwd=self.scratch, env=env,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 10
            cache = Path(env['DEVKIT_CACHE_DIR'])
            while not list(cache.glob('.acquire-*/.devkit-extract-*/payload')) and process.poll() is None and time.monotonic() < deadline:
                time.sleep(.001)
            require(list(cache.glob('.acquire-*/.devkit-extract-*/payload')), 'Native extraction was never observed')
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=8)
            require(process.returncode == 143, f'Archive cancellation exit {process.returncode}: {stdout}{stderr}')
            require(not target.exists() and not list(cache.glob('.acquire-*')), 'Cancelled extraction left files')
            self.results.append(dict(name='cancel-native-extraction', passed=True, exitCode=143))
            print('PASS cancel-native-extraction', flush=True)
        finally:
            if process.poll() is None: process.kill(); process.communicate(timeout=5)
            self.fixture.routes[self.transport_archive_route] = original_archive
            self.fixture.routes[self.transport_index_route] = original_index
        valid = original_archive
        # CRC/truncation reach the actual native reader with a matching transport hash.
        changed = bytearray(valid); changed[-8] ^= 1
        self.reject_archive('native-CRC', bytes(changed))
        self.reject_archive('native-truncated-gzip', valid[:-1])
