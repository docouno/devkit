#!/usr/bin/env python3
"""Run genuine-public compatibility oracles as static test binaries on the host."""
from pathlib import Path
import hashlib
import json
import os
import subprocess
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_021 = '7f9dea1455027e0a0637077c7b040dbd7420a7fbc554d83d4d80ca47e9e849e0'


def main():
    value = os.environ.get('DEVKIT_TEST_LEGACY_RELEASE') or os.environ.get('DEVKIT_VERIFY_PREVIOUS')
    assert value, 'Set previous= to the preserved public 0.2.1 payload or DEVKIT_TEST_LEGACY_RELEASE'
    release = Path(value).resolve()
    raw = (release / 'release.json').read_bytes()
    assert hashlib.sha256(raw).hexdigest() == PUBLIC_021, 'Expected the pinned public 0.2.1 manifest'
    manifest = json.loads(raw)
    assert manifest['version'] == '0.2.1'
    for name, expected in manifest['files'].items():
        path = release / name
        assert path.is_file() and not path.is_symlink(), f'Nonregular public fixture: {name}'
        assert hashlib.sha256(path.read_bytes()).hexdigest() == expected['sha256'], f'Changed public fixture: {name}'
        assert path.stat().st_mode & 0o777 == expected['mode'], f'Changed public fixture mode: {name}'
    assert os.environ.get('DEVKIT_TEST_RUST_IMAGE', '').startswith('sha256:'), 'Pass an exact author Rust image ID'
    deps = ROOT / '.artifacts/author/target/x86_64-unknown-linux-musl/debug/deps'
    binaries = [p for p in deps.glob('core-*') if p.is_file() and os.access(p, os.X_OK) and p.read_bytes()[:4] == b'\x7fELF']
    assert len(binaries) == 1, f'Expected one freshly compiled core executable, got {binaries}'
    proof = ROOT / '.artifacts/author/legacy' / uuid.uuid4().hex
    proof.mkdir(parents=True)
    rows = []
    with tempfile.TemporaryDirectory(prefix='devkit genuine legacy ') as temporary:
        consumer = os.environ.get('DEVKIT_TEST_LEGACY_CONSUMER')
        if not consumer:
            consumer = str(Path(temporary) / 'rust consumer')
            env = {k: v for k, v in os.environ.items() if k in ('PATH', 'LANG', 'LC_ALL', 'TERM', 'HOME')}
            with (proof / 'public-factory-init.log').open('w') as output:
                created = subprocess.run(['bash', str(release / 'devkit'), 'init', consumer,
                    '--app', 'api=rust-api', '--package-source', 'local'], env=env,
                    stdout=output, stderr=subprocess.STDOUT, timeout=180)
            assert created.returncode == 0, (proof / 'public-factory-init.log').read_text()
            # Product-owned input preserved by the existing compatibility oracle.
            (Path(consumer) / '.env').write_text('PRODUCT_OWNER_SENTINEL=preserve\n')
        state = json.loads((Path(consumer) / '.devkit/manifest.json').read_text())
        assert state['version'] == '0.2.1' and state['releaseIdentity'] == PUBLIC_021
        env = dict(os.environ, DEVKIT_TEST_LEGACY_RELEASE=str(release), DEVKIT_TEST_LEGACY_CONSUMER=consumer)
        for name in ('genuine_accepted_legacy_add_passes_old_runtime_doctor', 'legacy_library_patch_registration_add_and_modern_migration_preserve_native_ownership'):
            with (proof / (name + '.log')).open('w') as output:
                result = subprocess.run([str(binaries[0]), '--exact', name, '--ignored', '--nocapture', '--test-threads=1'],
                    cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, timeout=240)
            rows.append(dict(test=name, exitCode=result.returncode))
            assert result.returncode == 0, (proof / (name + '.log')).read_text()
    (proof / 'receipt.json').write_text(json.dumps(dict(passed=True, cases=rows,
        publicManifestSha256=PUBLIC_021, rustImage=os.environ['DEVKIT_TEST_RUST_IMAGE'],
        limits=['The preserved external 0.2.1 Bash factory is only a historical compatibility oracle.']), indent=2) + '\n')
    print(f'PASS genuine public native compatibility oracles; {proof}')


if __name__ == '__main__':
    main()
