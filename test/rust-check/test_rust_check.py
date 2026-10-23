#!/usr/bin/env python3
"""Native Rust helper regressions; host Python/Docker, no host Rust or registry."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import uuid

REPO = Path(__file__).resolve().parents[2]
HELPER = Path(os.environ.get("DEVKIT_TEST_ENGINE", REPO / ".artifacts/native/target/x86_64-unknown-linux-musl/debug/devkit")).resolve()
RECIPE = REPO / "runtime/rust.Dockerfile"
LABEL = "io.docouno.devkit.invocation"
RECIPE_LABEL = "io.docouno.devkit.rust-recipe"
VERSION = "1.99.1"
DEFAULT_SOURCE = """/// Returns the fixture value.
///
/// ```
/// assert_eq!(devkit_probe::value(), 1);
/// ```
pub fn value() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn value_is_one() {
        assert_eq!(super::value(), 1);
    }
}
"""


def run(*args, timeout=120):
    return subprocess.run(args, capture_output=True, text=True, timeout=timeout)


def checked(*args, timeout=120):
    result = run(*args, timeout=timeout)
    if result.returncode:
        raise AssertionError(f"{args!r}: {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def source_hashes(root):
    result = {}
    for path in sorted(root.rglob("*")):
        if "target" in path.relative_to(root).parts:
            continue
        if path.is_symlink():
            result[str(path.relative_to(root))] = "symlink:" + os.readlink(path)
        elif path.is_file():
            result[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


class RustCheckTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.token = uuid.uuid4().hex
        cls.artifacts = REPO / ".artifacts/rust-check" / cls.token
        cls.artifacts.mkdir(parents=True)
        cls.temp = tempfile.TemporaryDirectory(prefix="devkit-rust-check-")
        cls.base = Path(cls.temp.name)
        cls.commands = []
        cls.recipe_sha = hashlib.sha256(RECIPE.read_bytes()).hexdigest()
        cls.image = None
        # Acquisition is explicit and uses the actual shipping recipe. Docker may
        # reuse build cache; all subsequent native probes have network disabled.
        iidfile = cls.artifacts / "rust-image.id"
        build_args = ["docker", "build", "--file", str(RECIPE), "--iidfile", str(iidfile),
                      "--build-arg", "DEVKIT_RECIPE_SHA=" + cls.recipe_sha,
                      "--label", LABEL + "=" + cls.token, str(REPO / "runtime")]
        build = run(*build_args, timeout=300)
        (cls.artifacts / "build.stdout").write_text(build.stdout)
        (cls.artifacts / "build.stderr").write_text(build.stderr)
        if build.returncode:
            cls.temp.cleanup()
            raise AssertionError(f"shipping Rust image build failed: {build.stderr}")
        cls.image = iidfile.read_text().strip()
        image = json.loads(checked("docker", "image", "inspect", cls.image).stdout)[0]
        if image["Id"] != cls.image or image["Config"]["Labels"].get(RECIPE_LABEL) != cls.recipe_sha:
            raise AssertionError("derived image does not match the shipping recipe identity")
        cls.baseline = cls.base / "baseline"
        (cls.baseline / "src").mkdir(parents=True)
        (cls.baseline / "Cargo.toml").write_text(
            '[package]\nname = "devkit-probe"\nversion = "0.1.0"\n'
            'edition = "2024"\nrust-version = "1.99.1"\npublish = false\n')
        (cls.baseline / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.99.1"\ncomponents = ["rustfmt", "clippy"]\n')
        (cls.baseline / "rustfmt.toml").write_text('edition = "2024"\nstyle_edition = "2024"\n')
        (cls.baseline / "src/lib.rs").write_text(DEFAULT_SOURCE)
        result = cls.native(cls.baseline, ["cargo", "generate-lockfile", "--offline"], write=True)
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        cls.write_receipt()
        print(f"Rust helper evidence: {cls.artifacts}", flush=True)

    @classmethod
    def write_receipt(cls):
        paths = [HELPER, RECIPE, Path(__file__), REPO / "templates/rust-api/README.md"]
        receipt = {"imageId": cls.image, "recipeSha256": cls.recipe_sha,
                   "sourceSha256": {str(path.relative_to(REPO)): hashlib.sha256(path.read_bytes()).hexdigest()
                                    for path in paths}, "commands": cls.commands}
        (cls.artifacts / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")

    @classmethod
    def native(cls, fixture, command, *, write=False, extra_env=None, cwd="/app"):
        index = len(cls.commands)
        name = "devkit-rust-check-" + cls.token + "-" + str(index)
        cache = cls.base / ("cache-" + str(index))
        cache.mkdir()
        (fixture / "target").mkdir(exist_ok=True)
        before = source_hashes(fixture)
        args = ["docker", "run", "--rm", "--name", name, "--label", LABEL + "=" + cls.token,
                "--network", "none", "--read-only", "--tmpfs", "/tmp:mode=1777,exec",
                "--user", f"{os.getuid()}:{os.getgid()}",
                "--env", "DEVKIT_RUST_VERSION=" + VERSION, "--env", "RUSTUP_AUTO_INSTALL=0",
                "--env", "CARGO_HOME=/cache/cargo", "--env", "HOME=/tmp/devkit-home",
                "--env", "CARGO_BUILD_JOBS=2", "--volume",
                f"{fixture}:/app:" + ("rw,z" if write else "ro,z"),
                "--volume", f"{HELPER}:/runtime/devkit:ro,z",
                "--volume", f"{cache}:/cache/cargo:rw,z",
                "--volume", f"{fixture / 'target'}:/app/target:rw,z", "--workdir", cwd]
        for key, value in (extra_env or {}).items():
            args += ["--env", key + "=" + value]
        args += [cls.image, *command]
        result = None
        leaked = False
        try:
            result = run(*args)
        finally:
            # Query failure is visible; only the exact name carrying this run's
            # label may ever be removed, including a timed-out Docker client.
            present = checked("docker", "container", "ls", "--all", "--filter", "name=^/" + name + "$",
                              "--format", "{{.ID}}").stdout.strip()
            if present:
                leaked = True
                found = json.loads(checked("docker", "container", "inspect", name).stdout)[0]
                if found["Config"]["Labels"].get(LABEL) != cls.token:
                    raise AssertionError("refusing cleanup of container with unexpected ownership: " + name)
                checked("docker", "container", "rm", "--force", name)
                if checked("docker", "container", "ls", "--all", "--filter", "name=^/" + name + "$",
                           "--format", "{{.ID}}").stdout.strip():
                    raise AssertionError("owned container remains after cleanup: " + name)
        if result is None:
            raise AssertionError("native command did not complete")
        stem = f"{index:02d}-" + fixture.name + "-" + command[-1]
        stdout_file, stderr_file = stem + ".stdout", stem + ".stderr"
        (cls.artifacts / stdout_file).write_text(result.stdout)
        (cls.artifacts / stderr_file).write_text(result.stderr)
        after = source_hashes(fixture)
        cls.commands.append({"argv": args, "exit": result.returncode, "stdout": stdout_file,
                             "stderr": stderr_file, "sourceBefore": before, "sourceAfter": after,
                             "containerAbsentBeforeOuterCleanup": not leaked})
        cls.write_receipt()
        if leaked:
            raise AssertionError("--rm container remained before outer cleanup: " + name)
        if not write and before != after:
            raise AssertionError("read-only native command changed fixture source/config/lock")
        return result

    @classmethod
    def tearDownClass(cls):
        cls.write_receipt()
        cls.temp.cleanup()

    def setUp(self):
        self.app = self.base / self._testMethodName
        shutil.copytree(self.baseline, self.app, ignore=shutil.ignore_patterns("target"))

    def helper(self, command="test", **kwargs):
        return self.native(self.app, ["/runtime/devkit", "internal", "rust", command], **kwargs)

    def source(self, text):
        (self.app / "src/lib.rs").write_text(text)

    def assert_failure(self, result, diagnostic):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(diagnostic, result.stdout + result.stderr)

    def test_native_unit_and_doctest_execute(self):
        result = self.helper("check")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2 non-ignored, 0 ignored; discovery may include compile-only doctests", result.stdout)
        self.assertIn("tests::value_is_one ... ok", result.stdout)
        self.assertIn("Doc-tests devkit_probe", result.stderr)

    def test_no_run_only_is_compile_validation_without_execution_claim(self):
        self.source('//! ```no_run\n//! panic!("This must never execute");\n//! ```\n')
        result = self.helper()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 non-ignored, 0 ignored; discovery may include compile-only doctests", result.stdout)
        self.assertIn(" - compile", result.stdout)
        self.assertNotIn("runnable", result.stdout + result.stderr)

    def test_empty_and_all_ignored_native_cases_fail(self):
        for source in ("", "#[test]\n#[ignore]\nfn ignored() {}\n",
                       '//! ```ignore\n//! panic!("ignored");\n//! ```\n'):
            with self.subTest(source=source):
                self.source(source)
                self.assert_failure(self.helper(), "no non-ignored native Rust test cases found")

    def test_failed_native_unit_and_doctest_stay_red(self):
        for source, diagnostic in (("#[test]\nfn broken() { panic!(\"unit regression\"); }\n", "broken ... FAILED"),
                                   ('//! ```\n//! panic!("doctest regression");\n//! ```\n', "doctest failed")):
            with self.subTest(diagnostic=diagnostic):
                self.source(source)
                result = self.helper()
                self.assert_failure(result, diagnostic)
                self.assertIn("1 non-ignored", result.stdout)

    def test_format_configuration_cannot_silently_disable_or_ignore_policy(self):
        for config, diagnostic in (("disable_all_formatting = true\n", "disables mandatory formatting"),
                                   ("unknown_devkit_option = true\n", "ignored a configuration option"),
                                   ("unstable_features = true\n", "ignored a configuration option")):
            with self.subTest(config=config):
                self.source("pub fn value( )->u8{1}\n")
                (self.app / "rustfmt.toml").write_text(config)
                before = source_hashes(self.app)
                self.assert_failure(self.helper("fix", write=True), diagnostic)
                self.assertEqual(source_hashes(self.app), before, "invalid config mutated source before refusal")

    def test_hidden_format_configuration_is_an_explicit_conflict(self):
        # A restrictive intended config and permissive hidden config must not be
        # resolved by rustfmt's silent .rustfmt.toml precedence.
        (self.app / ".rustfmt.toml").write_text("disable_all_formatting = true\n")
        self.assert_failure(self.helper("check"), ".rustfmt.toml conflicts with the required rustfmt.toml")

    def test_missing_config_wrong_cwd_and_toolchain_overrides_fail(self):
        self.assert_failure(self.helper("check", cwd="/"), "missing Cargo.toml")
        config = self.app / "rustfmt.toml"
        saved = config.read_text()
        config.unlink()
        self.assert_failure(self.helper("check"), "missing rustfmt.toml")
        config.write_text(saved)
        self.assert_failure(self.helper("check", extra_env={"RUSTUP_TOOLCHAIN": VERSION}), "masks rust-toolchain.toml")
        (self.app / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.0.0"\n')
        self.assert_failure(self.helper("check"), "the app toolchain is unavailable")

    def test_outer_workspace_cannot_hide_the_accepted_component_lock(self):
        member = self.app / "member"
        (member / "src").mkdir(parents=True)
        (member / "Cargo.toml").write_text('[package]\nname="workspace-member"\nversion="0.1.0"\nedition="2024"\n')
        (member / "src/lib.rs").write_text("pub fn value() -> u8 { 1 }\n")
        for name in ("Cargo.lock", "rust-toolchain.toml", "rustfmt.toml"):
            shutil.copy2(self.app / name, member / name)
        with (self.app / "Cargo.toml").open("a") as manifest:
            manifest.write('\n[workspace]\nmembers=["member"]\n')
        result = self.helper("build", cwd="/app/member")
        self.assert_failure(result, "Unsupported outer Cargo workspace/lock authority")

    def test_clippy_checks_test_targets(self):
        self.source("""#[cfg(test)]
mod tests {
    #[test]
    fn bool_comparison() {
        assert_eq!(std::env::var("DEVKIT_PROBE").is_ok(), false);
    }
}
""")
        self.assert_failure(self.helper("check"), "clippy::bool-assert-comparison")

    def test_locked_build_refuses_unaccepted_path_dependency(self):
        with (self.app / "Cargo.toml").open("a") as manifest:
            manifest.write('\n[dependencies]\nlock-probe = { path = "lock-probe" }\n')
        dependency = self.app / "lock-probe"
        (dependency / "src").mkdir(parents=True)
        (dependency / "Cargo.toml").write_text(
            '[package]\nname = "lock-probe"\nversion = "0.1.0"\nedition = "2024"\n')
        (dependency / "src/lib.rs").write_text("pub fn dependency_value() -> u8 { 2 }\n")
        result = self.helper("build")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("--locked", result.stderr)
        self.assertRegex(result.stderr.lower(), r"lock ?file|lockfile")


if __name__ == "__main__":
    unittest.main(verbosity=2)
