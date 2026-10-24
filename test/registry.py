#!/usr/bin/env python3
"""Native core + native npm registry proof, with no host language SDK.

The Rust example supplies a local registry through the existing library test seam.
Public CLI/registry publication is verified separately against published assets.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
PINS = dict(line.split('=', 1) for line in (ROOT / 'toolchains.env').read_text().splitlines())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('previous', type=Path)
    parser.add_argument('release', type=Path)
    args = parser.parse_args()
    releases = [args.previous.resolve(), args.release.resolve()]
    manifests = [json.loads((p / 'release.json').read_text()) for p in releases]
    assert manifests[0]['version'] != manifests[1]['version'], 'Provide two genuinely distinct releases'
    target_dir = ROOT / '.artifacts/author/target/x86_64-unknown-linux-musl/debug'
    engine = Path(os.environ.get('DEVKIT_AUTHOR_ENGINE', target_dir / 'devkit')).resolve()
    fixture = Path(os.environ.get('DEVKIT_REGISTRY_FIXTURE', target_dir / 'examples/registry_fixture')).resolve()
    assert engine.is_file() and fixture.is_file(), 'Run make setup build-fixtures'
    before = [{str(p.relative_to(root)): digest(p) for p in root.rglob('*') if p.is_file()} for root in releases]
    proof = ROOT / '.artifacts/registry' / uuid.uuid4().hex
    proof.mkdir(parents=True)
    rows = []
    owned = []
    server = None
    passed = False
    with tempfile.TemporaryDirectory(prefix='devkit registry ') as scratch:
        scratch = Path(scratch)
        blocker = scratch / 'without host SDK'
        blocker.mkdir()
        forbidden = scratch / 'host-sdk-attempts'
        for name in ('node', 'npm', 'npx', 'cargo', 'rustc', 'rustup', 'tsc', 'eslint'):
            path = blocker / name
            path.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$DEVKIT_FORBIDDEN_HOST_SDK_LOG"\nexit 97\n')
            path.chmod(0o755)
        env = {k: v for k, v in os.environ.items() if k in ('PATH', 'LANG', 'LC_ALL', 'TERM')}
        env.update(PATH=str(blocker) + os.pathsep + env['PATH'], HOME=str(scratch / 'home'),
                   DEVKIT_FORBIDDEN_HOST_SDK_LOG=str(forbidden), CI='true')
        consumer = scratch / 'consumer'
        consumer.mkdir()
        def node(mode):
            token = str(uuid.uuid4())
            owned.append("devkit-registry-" + token)
            return [str(engine), 'internal', 'guard', 'devkit-registry-' + token, token, '--',
                    'docker', 'run', '--pull', 'never', '--rm', '--init', '--name', 'devkit-registry-' + token,
                    '--label', 'io.docouno.devkit.invocation=' + token, '--network', 'host',
                    '--user', f'{os.getuid()}:{os.getgid()}', '--read-only', '--memory', '768m', '--cpus', '2',
                    '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true', '--tmpfs', '/tmp:mode=1777',
                    '--volume', f'{ROOT}:/source:ro,z', '--volume', f'{engine}:/devkit/devkit:ro,z',
                    '--volume', f'{releases[0]}:/release-v1:ro,z', '--volume', f'{releases[1]}:/release-v2:ro,z',
                    '--volume', f'{scratch}:/proof:z', '--volume', f'{consumer}:/consumer:z',
                    '--workdir', '/source', '--env', 'HOME=/tmp/home', '--env', 'CI=true',
                    PINS['DEVKIT_NODE_IMAGE'], '/devkit/devkit', 'internal', 'node', PINS['DEVKIT_NODE_VERSION'],
                    '--', 'node', 'test/registry-consumer.mjs', mode]
        def run(name, argv):
            started = time.monotonic()
            result = subprocess.run(argv, cwd=scratch, env=env, capture_output=True, text=True, timeout=240)
            (proof / f'{name}.log').write_text(result.stdout + result.stderr)
            rows.append(dict(name=name, exitCode=result.returncode, seconds=time.monotonic() - started))
            assert result.returncode == 0, f'{name}: {result.stdout}\n{result.stderr}'
        try:
            with (proof / 'server.log').open('w') as log:
                server = subprocess.Popen(node('serve'), cwd=scratch, env=env, stdout=log, stderr=log)
                deadline = time.monotonic() + 20
                while not (scratch / 'registry-url').exists() and server.poll() is None and time.monotonic() < deadline:
                    time.sleep(.05)
                assert (scratch / 'registry-url').exists(), (proof / 'server.log').read_text()
                registry = (scratch / 'registry-url').read_text()
                run('native-init', [str(fixture), 'init', str(releases[0]), str(consumer), registry])
                run('first-npm', node('first'))
                source = consumer / 'apps/main/src/greeting.ts'
                source.write_text(source.read_text() + '\n// Preserve the product edit.\n')
                (consumer / '.env').write_text('PRODUCT_SENTINEL=preserve\n')
                preserved = {p: p.read_bytes() for p in (source, consumer / '.env', consumer / 'Makefile', consumer / 'compose.yaml')}
                run('native-update', [str(fixture), 'update', str(releases[1]), str(consumer), registry])
                assert all(p.read_bytes() == data for p, data in preserved.items())
                assert json.loads((consumer / '.devkit/manifest.json').read_text())['version'] == manifests[1]['version']
                run('second-npm', node('second'))
                run('integrity-missing-version-negatives', node('negative'))
                (proof / 'requests.json').write_bytes((scratch / 'requests.json').read_bytes())
                assert not forbidden.exists(), 'Host SDK attempted'
                passed = True
        finally:
            if server is not None:
                server.terminate()
                try:
                    server.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    server.kill(); server.wait(timeout=5)
            for name in owned:
                present = subprocess.run(['docker', 'container', 'ls', '--all', '--filter', f'name=^/{name}$', '--format', '{{.Names}}'],
                    capture_output=True, text=True, timeout=20)
                assert present.returncode == 0 and not present.stdout.strip(), f'Cannot establish exact owned cleanup: {name}: {present.stderr}'
            after = [{str(p.relative_to(root)): digest(p) for p in root.rglob('*') if p.is_file()} for root in releases]
            assert after == before, 'Registry proof modified a source release'
            (proof / 'receipt.json').write_text(json.dumps(dict(passed=passed, cases=rows,
                releases=[dict(version=m['version'], releaseIdentity=digest(p / 'release.json')) for p, m in zip(releases, manifests)],
                engineSha256=digest(engine), fixtureSha256=digest(fixture), hostSdkAttempts=0, containersAbsent=owned,
                limits=['Local unpublished registry uses the author-only native library fixture; public CLI registry proof is separate.']), indent=2) + '\n')
    print(f'PASS native registry init/update + npm exports/gates/integrity; {proof}')


if __name__ == '__main__':
    main()
