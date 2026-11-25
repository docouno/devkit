#!/usr/bin/env python3
"""Exercise the actual author Make glue in a path containing literal Make syntax."""
from __future__ import annotations
import json
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import shlex
import signal
import sys
import subprocess
import tempfile
import time
import uuid

from release_targets import engine_asset, executable_header_matches, host_target

ROOT = Path(__file__).resolve().parents[1]


def verify_assembler_handoff(scratch):
    """A clean assembler runner receives argv and needs no author SDK."""
    capture = scratch / "assembler with spaces"
    capture.write_text(f"#!{sys.executable}\nimport json, sys\nprint(json.dumps(sys.argv[1:]))\n")
    capture.chmod(0o755)
    inputs = [f"x86_64-unknown-linux-musl:{scratch}/native x64:{'a' * 64}",
              f"aarch64-unknown-linux-musl:{scratch}/native arm:{'b' * 64}"]
    npm = scratch / "npm handoff"
    output = scratch / "new set"
    for target in ("pack", "pack-preview"):
        result = subprocess.run([
            "make", "--no-print-directory", target, f"DEVKIT_AUTHOR_ENGINE={capture}",
            f"output={output}", f"npm_inputs={npm}", "target_inputs=" + shlex.join(inputs),
        ], cwd=ROOT, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stdout + result.stderr
        args = json.loads(result.stdout)
        assert args[:2] == ["internal", "assemble"]
        assert args[args.index("--npm-inputs") + 1] == str(npm)
        assert args[args.index("--output") + 1] == str(output)
        assert [args[i + 1] for i, arg in enumerate(args) if arg == "--target-input"] == inputs
        assert "--local" not in args
        assert ("--verified-preview" in args) == (target == "pack-preview")


def verify_baseline_selection(root):
    """Run the documented verification entry with native suite entry points recorded."""
    wrappers = root / "baseline-probes"
    wrappers.mkdir()
    uname = wrappers / "uname"
    uname.write_text('#!/bin/sh\nif test "$1" = -m; then echo "$PROBE_ARCH"; else exec /usr/bin/uname "$@"; fi\n')
    uname.chmod(0o755)
    for name in ("python3", "bash"):
        probe = wrappers / name
        probe.write_text(f"#!{sys.executable}\n" +
                         "import json, os, sys\nwith open(os.environ['PROBE_LOG'], 'a') as f:\n"
                         "    print(json.dumps([sys.argv[1:], os.environ.get('DEVKIT_VERIFY_NEXT_RELEASE')]), file=f)\n")
        probe.chmod(0o755)
    for arch in ("x86_64", "aarch64"):
        log = wrappers / (arch + ".jsonl")
        env = dict(os.environ, PATH=str(wrappers) + os.pathsep + os.environ["PATH"],
                   PROBE_ARCH=arch, PROBE_LOG=str(log), DEVKIT_VERIFY_NEXT_RELEASE="stale")
        argv = ["make", "--no-print-directory"]
        for other in ("verify-source-gates", "verify-runtime-gates", "verify-delivery", "verify-engine", "build-fixtures"):
            argv.extend(["-o", other])
        argv.extend(["verify", "build_set=/new", "release=/new/payload", "previous=/v0.5.1/payload", "baseline_set=/v0.5.1"])
        result = subprocess.run(argv, cwd=root, env=env, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stderr + result.stdout
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert calls[0][0] == ["test/bootstrap/verify.py", "--build-set", "/new", "--baseline-set", "/v0.5.1"], calls
        if arch == "x86_64":
            assert calls[1] == [["test/packaged/verify.py", "/v0.5.1/payload"], "/new/payload"], calls
            assert calls[2][0] == ["test/verify-registry.sh", "/v0.5.1/payload", "/new/payload"], calls
        else:
            assert calls[1] == [["test/packaged/verify.py", "/new/payload"], None], calls
            assert calls[2][0] == ["test/verify-registry.sh", "/new/payload"], calls
        assert len(calls) == 3, calls


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--data-only", action="store_true")
    modes.add_argument("--engine-only", action="store_true")
    args = parser.parse_args()
    if args.engine_only:
        engine = Path(os.environ['DEVKIT_VERIFY_ENGINE'])
        assert engine.is_file() and not engine.is_symlink() and os.access(engine, os.X_OK), 'Expected a regular executable black-box engine'
        data = engine.read_bytes()
        assert executable_header_matches(data, host_target()), 'Black-box tests require the matching native binary'
        sha = hashlib.sha256(data).hexdigest()
        if os.environ.get('DEVKIT_BUILD_SET'):
            build_set = Path(os.environ['DEVKIT_BUILD_SET'])
            index = json.loads((build_set / 'release-index.json').read_text())
            identity = index['files'][engine_asset(index)]
            assert sha == identity['sha256'] and len(data) == identity['size'], 'Selected black-box engine differs from the frozen build set'
            assert hashlib.sha256((engine.parent / 'release.json').read_bytes()).hexdigest() == index['releaseIdentity'], 'Selected release differs from the frozen build set'
            assert hashlib.sha256((build_set / engine_asset(index)).read_bytes()).hexdigest() == sha, 'Frozen engine asset differs from its index'
        print('Black-box engine: ' + json.dumps(dict(path=str(engine.resolve()), sha256=sha)))
        return
    engine = Path(os.environ.get('DEVKIT_AUTHOR_ENGINE', ROOT / '.artifacts/author/target' / host_target() / 'debug/devkit')).resolve()
    assert engine.is_file(), 'Run make setup'
    proof = ROOT / '.artifacts/author/probes' / uuid.uuid4().hex
    proof.mkdir(parents=True)
    rows = []
    with tempfile.TemporaryDirectory(prefix='devkit author ') as directory:
        root = Path(directory) / 'literal $(not-a-function)'
        (root / 'tooling').mkdir(parents=True)
        for name in ('Makefile', 'toolchains.env', 'VERSION', 'pnpm-workspace.yaml', 'tooling/author.mk', 'tooling/native.Dockerfile'):
            shutil.copy2(ROOT / name, root / name)
        # GNU Make command-line values are recursive by default. Treat every
        # operator path/argument as data, including literal Make function syntax.
        fields = {'target': 'DEVKIT_INIT_TARGET', 'output': 'DEVKIT_ASSEMBLY_OUTPUT',
            'build_set': 'DEVKIT_BUILD_SET', 'previous': 'DEVKIT_VERIFY_PREVIOUS',
            'release': 'DEVKIT_VERIFY_RELEASE', 'baseline_set': 'DEVKIT_BASELINE_SET',
            'fixtures': 'DEVKIT_AUTHOR_FIXTURES', 'target_inputs': 'DEVKIT_TARGET_INPUTS',
            'npm_inputs': 'DEVKIT_NPM_INPUTS', 'DEVKIT_AUTHOR_ENGINE': 'DEVKIT_AUTHOR_ENGINE',
            'template': 'DEVKIT_INIT_TEMPLATE', 'version': 'DEVKIT_RELEASE_VERSION',
            'from_version': 'DEVKIT_FROM_VERSION', 'build_id': 'DEVKIT_BUILD_ID',
            'build_run': 'DEVKIT_BUILD_RUN', 'expected_index_sha256': 'DEVKIT_EXPECTED_INDEX_SHA256',
            'DEVKIT_AUTHOR_CARGO_HOME': 'DEVKIT_AUTHOR_CARGO_HOME',
            'DEVKIT_AUTHOR_RUST_IMAGE': 'DEVKIT_AUTHOR_RUST_IMAGE',
            'DEVKIT_AUTHOR_PNPM_STORE': 'DEVKIT_AUTHOR_PNPM_STORE',
            'DEVKIT_AUTHOR_PNPM_CACHE': 'DEVKIT_AUTHOR_PNPM_CACHE'}
        program = 'import json,os; print(json.dumps({k:os.environ[k] for k in ' + repr(list(fields.values())) + '}))'
        with (root / 'Makefile').open('a') as makefile:
            makefile.write("\n.PHONY: inspect-author-data\ninspect-author-data:\n\t@python3 -c " + "'" + program.replace("'", "'\"'\"'") + "'\n")
        marker = Path('/tmp') / ('devkit-make-eval-' + uuid.uuid4().hex)
        values = {key: '/tmp/literal $(literal)-$(shell touch ' + str(marker) + ')-tail ' for key in fields}
        try:
            result = subprocess.run(['make', '--no-print-directory', 'inspect-author-data', *[key + '=' + value for key, value in values.items()]],
                cwd=root, capture_output=True, text=True, timeout=10)
            assert result.returncode == 0, result.stdout + result.stderr
            actual = json.loads(result.stdout)
            assert actual == {env: values[key] for key, env in fields.items()}, actual
            assert not marker.exists(), 'GNU Make evaluated a data argument'
            rows.append(dict(name='literal-command-line-data', passed=True, fields=fields))
        finally:
            marker.unlink(missing_ok=True)
        verify_assembler_handoff(root)
        rows.append(dict(name='assemble-with-native-contributions', passed=True))
        verify_baseline_selection(root)
        rows.append(dict(name='baseline-routing-on-both-linux-hosts', passed=True))
        if args.data_only:
            (proof / 'receipt.json').write_text(json.dumps(dict(passed=True, cases=rows, implicitMakeEvaluation=False), indent=2) + '\n')
            print(f'PASS literal Make operator data; {proof}')
            return
        local_engine = root / '.artifacts/author/target' / host_target() / 'debug/devkit'
        local_engine.parent.mkdir(parents=True)
        shutil.copy2(engine, local_engine)
        (root / 'packages/env').mkdir(parents=True)
        (root / 'packages/env/package.json').write_text(json.dumps({'name': '@docouno/env', 'version': '0.5.0', 'private': True}))
        (root / 'source.txt').write_text('readonly source\n')
        script = """import fs from 'node:fs';
import assert from 'node:assert/strict';
assert.equal(process.env.DEVKIT_HOST_SECRET, undefined);
assert.equal(fs.existsSync('/var/run/docker.sock'), false);
assert.equal(process.getuid(), Number(process.env.EXPECTED_UID || process.getuid()));
assert.throws(() => fs.writeFileSync('/workspace/source.txt', 'changed'), /EROFS|EACCES/);
fs.writeFileSync('/workspace/.artifacts/probe.txt', 'writable output');
if (process.argv[2] === 'failure') process.exit(42);
if (process.argv[2] === 'cancel') { console.log('READY'); setInterval(() => {}, 1000); }
"""
        (root / 'probe.mjs').write_text(script.replace('Number(process.env.EXPECTED_UID || process.getuid())', str(os.getuid())))
        wrappers = root / 'host PATH'
        wrappers.mkdir()
        docker_log = proof / 'docker.jsonl'
        real_docker = shutil.which('docker')
        (wrappers / 'docker').write_text('#!/usr/bin/python3\nimport json,os,sys\n'
            f'with open({str(docker_log)!r}, "a") as f: f.write(json.dumps(sys.argv[1:])+"\\n")\n'
            f'os.execv({real_docker!r},[{real_docker!r},*sys.argv[1:]])\n')
        (wrappers / 'docker').chmod(0o755)
        for name in ('pnpm', 'pnpx', 'corepack'):
            (wrappers / name).write_text('#!/bin/sh\nexit 97\n')
            (wrappers / name).chmod(0o755)
        env = dict(os.environ, PATH=str(wrappers) + os.pathsep + os.environ['PATH'], DEVKIT_HOST_SECRET='must-not-enter-sdk')
        for mode, expected in [('success', 0), ('failure', 2)]:
            (root / 'package.json').write_text(json.dumps({'private': True, 'type': 'module', 'scripts': {'check': f'node probe.mjs {mode}'}}))
            result = subprocess.run(['make', 'check-env'], cwd=root, env=env, capture_output=True, text=True, timeout=25)
            (proof / f'{mode}.log').write_text(result.stdout + result.stderr)
            assert result.returncode == expected, result.stdout + result.stderr
            if mode == 'failure': assert '42' in (result.stdout + result.stderr)
            rows.append(dict(name=mode, makeExit=result.returncode, nativeExit=42 if mode == 'failure' else 0))
        (root / 'package.json').write_text(json.dumps({'private': True, 'type': 'module', 'scripts': {'check': 'node probe.mjs cancel'}}))
        with (proof / 'cancel.log').open('w') as output:
            process = subprocess.Popen(['make', 'check-env'], cwd=root, env=env, stdout=output, stderr=output, start_new_session=True)
            try:
                until = time.monotonic() + 15
                while 'READY' not in (proof / 'cancel.log').read_text() and process.poll() is None and time.monotonic() < until:
                    time.sleep(.05)
                assert 'READY' in (proof / 'cancel.log').read_text(), (proof / 'cancel.log').read_text()
                os.killpg(process.pid, signal.SIGTERM)
                code = process.wait(timeout=15)
                assert code != 0, 'Cancelled Make reported success'
                rows.append(dict(name='cancel', makeExit=code))
            finally:
                if process.poll() is None: os.killpg(process.pid, signal.SIGKILL); process.wait(timeout=5)
        assert (root / 'source.txt').read_text() == 'readonly source\n'
        assert (root / '.artifacts/probe.txt').read_text() == 'writable output'
        calls = [json.loads(line) for line in docker_log.read_text().splitlines()]
        names = [args[args.index('--name') + 1] for args in calls if args[:1] == ['run'] and '--name' in args]
        assert len(names) == 3, names
        for name in names:
            result = subprocess.run([real_docker, 'container', 'ls', '--all', '--filter', f'name=^/{name}$', '--format', '{{.Names}}'], capture_output=True, text=True, timeout=15)
            assert result.returncode == 0 and not result.stdout.strip(), f'Owned container survived: {name}: {result.stderr}'
        assert all('--pull' in args and args[args.index('--pull') + 1] == 'never' for args in calls if args[:1] == ['run'])
        (proof / 'receipt.json').write_text(json.dumps(dict(passed=True, cases=rows, containersAbsent=names,
            sourceReadonly=True, outputWritable=True, hostSecretAbsent=True, dockerSocketAbsent=True,
            literalMakePathPreserved=True, implicitPulls=0), indent=2) + '\n')
    print(f'PASS author Make → native guard → pinned Node SDK; {proof}')


if __name__ == '__main__':
    main()
