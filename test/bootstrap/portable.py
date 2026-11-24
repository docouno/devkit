#!/usr/bin/env python3
"""Exercise the project loader with the host's system sh, Make and userland."""
import hashlib
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile

AUTHOR = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(AUTHOR / "test"))
from release_targets import host_target, payload_name  # noqa: E402


def verify(directory=None):
    with tempfile.TemporaryDirectory(prefix="devkit loader ", dir=directory) as scratch:
        project = Path(scratch)
        runtime = project / ".devkit/runtime"
        runtime.mkdir(parents=True)
        (runtime / "devkit.mk").write_bytes((AUTHOR / "runtime/devkit.mk").read_bytes())
        manifest = b'{"schemaVersion":2}\n\n'
        (project / ".devkit/manifest.json").write_bytes(manifest)
        manifest_sha = hashlib.sha256(manifest).hexdigest()
        # A pinned fixture reports only the Make-to-engine protocol.
        recorder = project / "record invocation"
        recorder.write_text(
            '#!/bin/sh\nprintf "%s %s %s %s\\n" "${DEVKIT_MAKE_DAG-unset}" '
            '"${DEVKIT_MAKE_PLAN-unset}" "${DEVKIT_EXPECTED_MANIFEST-unset}" "$*"\n'
        )
        recorder.chmod(0o755)
        sha = hashlib.sha256(recorder.read_bytes()).hexdigest()
        target = host_target()
        pattern = f"{platform.system()}:{platform.machine()}"
        branch = f"{pattern}) target={target}; sha256={sha}; payload={payload_name(target)} ;;"
        loader = runtime / "devkit.sh"
        loader.write_text((AUTHOR / "runtime/devkit.sh").read_text()
                          .replace("@@DEVKIT_MODE@@", "project")
                          .replace("@@DEVKIT_VERSION@@", "0.6.0")
                          .replace("# @@DEVKIT_ENGINE_PINS@@", branch))
        env = {
            "HOME": scratch, "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "LC_ALL": "C",
            "DEVKIT_ENGINE_PATH": str(recorder), "DEVKIT_MAKE_DAG": "ambient",
            "DEVKIT_MAKE_PLAN": "ambient", "DEVKIT_EXPECTED_MANIFEST": "ambient",
            "DEVKIT_EXPECTED_PLAN": "ambient",
        }

        def run(*argv, ok=True, environment=env):
            proc = subprocess.run(argv, cwd=project, env=environment,
                                  capture_output=True, text=True, timeout=8)
            if (proc.returncode == 0) != ok:
                raise AssertionError(f"{argv}: {proc.returncode}\n{proc.stdout}{proc.stderr}")
            return proc.stdout + proc.stderr

        def make(*args, **kwargs):
            return run("/usr/bin/make", "--no-print-directory", *args, **kwargs)

        (project / "Makefile").write_text(
            "include .devkit/runtime/devkit.mk\n"
            "setup: local-prerequisite\n"
            "local-prerequisite:\n"
            '\t@test -z "$${DEVKIT_MAKE_DAG+x}$${DEVKIT_MAKE_PLAN+x}$${DEVKIT_EXPECTED_MANIFEST+x}"\n'
            "\t@$(MAKE) --no-print-directory info\n"
        )
        projection = project / ".devkit/project.mk"
        for apps, packages in [("api", ""), ("", "core"), ("", "")]:
            projection.write_text(f"DEVKIT_REGISTERED_APPS := {apps}\n"
                                  f"DEVKIT_REGISTERED_PACKAGES := {packages}\n"
                                  f"DEVKIT_EXPECTED_PLAN := {sha}\n")
            assert run("/bin/sh", loader, "make-manifest").strip() == manifest_sha
            output = make("setup")
            assert f"1 {sha} {manifest_sha} task snapshot --root" in output, output
            assert "unset unset unset --non-interactive info" in output, output
            assert "Registered apps:" in make("help", environment=dict(env, DEVKIT_ENGINE_PATH=""))
            for invalid in ("not hexadecimal", "f'ee", "1234"):
                # Even a syntactically valid CLI assignment has the wrong origin.
                assert "cannot read accepted Make projection" in make(
                    "setup", f"DEVKIT_EXPECTED_PLAN={invalid}", ok=False)
        projection.write_text("DEVKIT_EXPECTED_PLAN := not-a-digest\n")
        assert "cannot read accepted Make projection" in make("setup", ok=False)
        (project / ".devkit/manifest.json").unlink()
        os.mkfifo(project / ".devkit/manifest.json")
        run("/bin/sh", loader, "make-manifest", ok=False)
        cache = project / "native cache"
        cached = cache / "engines" / target / "0.6.0" / sha / "devkit"
        cached.parent.mkdir(parents=True)
        cached.write_bytes(recorder.read_bytes())
        cached.chmod(0o755)
        cached_env = dict(env, DEVKIT_ENGINE_PATH="", DEVKIT_CACHE_DIR=str(cache))
        cli = ("/bin/sh", loader, "cli", "--offline", "--version")
        run(*cli, environment=cached_env)
        for mode in (0o700, 0o775, 0o644):
            cached.chmod(mode)
            assert "cached engine integrity differs" in run(*cli, ok=False, environment=cached_env)
        recorder.chmod(0o700)
        run(*cli, environment=env)
        alias = project / "user cache alias"
        alias.symlink_to(cache, target_is_directory=True)
        assert "symbolic link" in run(*cli, ok=False, environment=dict(cached_env, DEVKIT_CACHE_DIR=str(alias)))
        if platform.system() == "Darwin":
            for name in ("tmp", "var", "etc"):
                canonical, mixed = Path("/") / name, Path("/") / name.capitalize()
                if mixed.is_symlink() and os.path.samestat(mixed.lstat(), canonical.lstat()):
                    absent_cache = mixed / ("devkit-absent-" + project.name)
                    assert "run make setup" in run(*cli, ok=False, environment=dict(cached_env, DEVKIT_CACHE_DIR=str(absent_cache)))
        print(f"PASS portable project loader and Make on {platform.system()} {platform.machine()}")


if __name__ == "__main__":
    verify()
