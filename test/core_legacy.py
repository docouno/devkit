#!/usr/bin/env python3
"""Run genuine-public compatibility oracles as static test binaries on the host."""
from pathlib import Path
import json
import os
import subprocess
import uuid

ROOT = Path(__file__).resolve().parents[1]


def main():
    for name in ('DEVKIT_TEST_LEGACY_RELEASE', 'DEVKIT_TEST_LEGACY_CONSUMER', 'DEVKIT_TEST_RUST_IMAGE'):
        assert os.environ.get(name), f'Set explicit {name}; no floating fixture/image fallback'
    deps = ROOT / '.artifacts/author/target/x86_64-unknown-linux-musl/debug/deps'
    binaries = [p for p in deps.glob('core-*') if p.is_file() and os.access(p, os.X_OK) and p.read_bytes()[:4] == b'\x7fELF']
    assert len(binaries) == 1, f'Expected one freshly compiled core executable, got {binaries}'
    proof = ROOT / '.artifacts/author/legacy' / uuid.uuid4().hex
    proof.mkdir(parents=True)
    rows = []
    for name in ('genuine_accepted_legacy_add_passes_old_runtime_doctor', 'legacy_library_patch_registration_add_and_modern_migration_preserve_native_ownership'):
        with (proof / (name + '.log')).open('w') as output:
            result = subprocess.run([str(binaries[0]), '--exact', name, '--ignored', '--nocapture', '--test-threads=1'],
                cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, timeout=240)
        rows.append(dict(test=name, exitCode=result.returncode))
        assert result.returncode == 0, (proof / (name + '.log')).read_text()
    (proof / 'receipt.json').write_text(json.dumps(dict(passed=True, cases=rows,
        rustImage=os.environ['DEVKIT_TEST_RUST_IMAGE'], limits=['Requires genuine public release and consumer fixtures; never fabricates versions.']), indent=2) + '\n')
    print(f'PASS genuine public native compatibility oracles; {proof}')


if __name__ == '__main__':
    main()
