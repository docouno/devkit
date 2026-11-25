#!/usr/bin/env python3
"""Native Rust helper regressions; host Python/Docker, no host Rust or registry."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "test"))
from release_targets import host_target, linux_arch  # noqa: E402
HELPER = Path(os.environ.get("DEVKIT_TEST_ENGINE", REPO / ".artifacts/author/target" / host_target() / "debug/devkit")).resolve()
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
        platform = "linux/" + linux_arch()
        platform_label = "io.docouno.devkit.platform"
        derived_tag = f"devkit-derived-rust:{linux_arch()}-{cls.recipe_sha}"
        inspected = run("docker", "image", "inspect", derived_tag)
        if inspected.returncode == 0:
            loaded = json.loads(inspected.stdout)
            if (loaded and loaded[0].get("Os") == "linux" and loaded[0].get("Architecture") == linux_arch()
                    and loaded[0]["Config"]["Labels"].get(platform_label) == platform
                    and loaded[0]["Config"]["Labels"].get(RECIPE_LABEL) == cls.recipe_sha):
                cls.image = loaded[0]["Id"]
                (cls.artifacts / "rust-image.id").write_text(cls.image + "\n")
                (cls.artifacts / "build.stdout").write_text(f"reused {derived_tag} -> {cls.image}\n")
                (cls.artifacts / "build.stderr").write_text("")
        if cls.image is None:
            # Acquisition is explicit and uses the actual shipping recipe. Docker may
            # reuse build cache; all subsequent native probes have network disabled.
            iidfile = cls.artifacts / "rust-image.id"
            build_args = ["docker", "build", "--platform", platform, "--label", f"{platform_label}={platform}", "--file", str(RECIPE), "--iidfile", str(iidfile),
                          "--tag", derived_tag,
                          "--build-arg", "DEVKIT_RECIPE_SHA=" + cls.recipe_sha,
                          str(REPO / "runtime")]
            build = run(*build_args, timeout=300)
            (cls.artifacts / "build.stdout").write_text(build.stdout)
            (cls.artifacts / "build.stderr").write_text(build.stderr)
            if build.returncode:
                cls.temp.cleanup()
                raise AssertionError(f"shipping Rust image build failed: {build.stderr}")
            cls.image = iidfile.read_text().strip()
        image = json.loads(checked("docker", "image", "inspect", cls.image).stdout)[0]
        if (image["Id"] != cls.image or image["Os"] != "linux" or image["Architecture"] != linux_arch()
                or image["Config"]["Labels"].get(RECIPE_LABEL) != cls.recipe_sha
                or image["Config"]["Labels"].get(platform_label) != platform):
            raise AssertionError("derived image does not match the shipping recipe identity")
        cls.baseline = cls.base / "baseline"
        app = cls.baseline / "apps/probe"
        sibling = cls.baseline / "apps/sibling"
        (app / "src").mkdir(parents=True)
        (sibling / "src").mkdir(parents=True)
        (cls.baseline / "Cargo.toml").write_text(
            '[workspace]\nmembers=["apps/probe","apps/sibling"]\nresolver="3"\n')
        (sibling / "Cargo.toml").write_text(
            '[package]\nname="sibling"\nversion="0.1.0"\nedition="2024"\n')
        (sibling / "src/lib.rs").write_text(
            '#[test]\nfn independent() { panic!("ran tests from a sibling component"); }\n')
        (app / "Cargo.toml").write_text(
            '[package]\nname = "devkit-probe"\nversion = "0.1.0"\n'
            'edition = "2024"\nrust-version = "1.99.1"\npublish = false\n')
        (cls.baseline / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.99.1"\ncomponents = ["rustfmt", "clippy"]\n')
        (cls.baseline / "rustfmt.toml").write_text('edition = "2024"\nstyle_edition = "2024"\n')
        (app / "src/lib.rs").write_text(DEFAULT_SOURCE)
        result = cls.native(cls.baseline, ["cargo", "generate-lockfile", "--offline"], write=True)
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        cls.write_receipt()
        print(f"Rust helper evidence: {cls.artifacts}", flush=True)

    @classmethod
    def write_receipt(cls):
        paths = [RECIPE, Path(__file__), REPO / "crates/devkit/src/rust_gate.rs"]
        receipt = {"imageId": cls.image, "recipeSha256": cls.recipe_sha,
                   "engine": {"path": str(HELPER), "sha256": hashlib.sha256(HELPER.read_bytes()).hexdigest()},
                   "sourceSha256": {str(path.relative_to(REPO)): hashlib.sha256(path.read_bytes()).hexdigest()
                                    for path in paths}, "commands": cls.commands}
        (cls.artifacts / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")

    @classmethod
    def native(cls, fixture, command, *, write=False, write_component=False, extra_env=None, cwd="/workspace"):
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
                f"{fixture}:/workspace:" + ("rw,z" if write else "ro,z"),
                "--volume", f"{HELPER}:/runtime/devkit:ro,z",
                "--volume", f"{cache}:/cache/cargo:rw,z",
                "--volume", f"{fixture / 'target'}:/workspace/target:rw,z", "--workdir", cwd]
        if write_component:
            args += ["--volume", f"{fixture / 'apps/probe'}:/workspace/apps/probe:rw,z"]
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
        stem = f"{index:02d}-{fixture.name}"
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
        checked_paths = set(before) | set(after)
        if write_component:
            checked_paths = {p for p in checked_paths if not p.startswith("apps/probe/")}
        if not write and any(before.get(p) != after.get(p) for p in checked_paths):
            raise AssertionError("Rust command changed a read-only source/config/lock file")
        return result

    @classmethod
    def tearDownClass(cls):
        cls.write_receipt()
        cls.temp.cleanup()

    def setUp(self):
        self.app = self.base / self._testMethodName
        shutil.copytree(self.baseline, self.app, ignore=shutil.ignore_patterns("target"))

    def helper(self, command="test", members=(("apps/probe", "devkit-probe"),), **kwargs):
        args = ["/runtime/devkit", "internal", "rust", command, "--workspace-root", "/workspace"]
        if command != "setup":
            args += ["--component", "apps/probe"]
            for path, name in members:
                args += ["--member", path, "--fmt-package", name, "--package",
                         f"path+file:///workspace/{path}#{name}@0.1.0"]
        return self.native(self.app, args, write_component=command == "fix", **kwargs)

    def source(self, text):
        (self.app / "apps/probe/src/lib.rs").write_text(text)

    def assert_failure(self, result, diagnostic):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(diagnostic, result.stdout + result.stderr)

    def test_native_unit_and_doctest_execute(self):
        result = self.helper("test")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2 non-ignored, 0 ignored; discovery may include compile-only doctests", result.stdout)
        self.assertIn("tests::value_is_one ... ok", result.stdout)
        self.assertIn("Doc-tests devkit_probe", result.stderr)

        # check validates fmt, clippy, and test --list without executing test bodies.
        self.source(
            'pub fn value() -> u8 {\n    1\n}\n\n'
            '#[cfg(test)]\nmod tests {\n    #[test]\n    fn panics_in_test_mode() {\n'
            '        panic!("only in test");\n    }\n}\n'
        )
        check_result = self.helper("check")
        self.assertEqual(check_result.returncode, 0, check_result.stdout + check_result.stderr)
        self.assertIn("1 non-ignored, 0 ignored; discovery may include compile-only doctests", check_result.stdout)
        self.assertNotIn("panics_in_test_mode ... FAILED", check_result.stdout + check_result.stderr)
        self.assert_failure(self.helper("test"), "only in test")

    def test_no_run_only_is_compile_validation_without_execution_claim(self):
        self.source('//! ```no_run\n//! panic!("This must never execute");\n//! ```\n')
        result = self.helper()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 non-ignored, 0 ignored; discovery may include compile-only doctests", result.stdout)
        self.assertIn(" - compile", result.stdout)
        self.assertNotIn("runnable", result.stdout + result.stderr)

    def test_empty_and_all_ignored_native_cases_fail(self):
        for source in ("\n", "#[test]\n#[ignore]\nfn ignored() {}\n",
                       '//! ```ignore\n//! panic!("ignored");\n//! ```\n'):
            with self.subTest(source=source):
                self.source(source)
                self.assert_failure(self.helper("check"), "no non-ignored native Rust test cases found")
                self.assert_failure(self.helper("test"), "no non-ignored native Rust test cases found")

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
                self.assert_failure(self.helper("fix"), diagnostic)
                self.assertEqual(source_hashes(self.app), before, "invalid config mutated source before refusal")

    def test_hidden_format_configuration_is_an_explicit_conflict(self):
        # A restrictive intended config and permissive hidden config must not be
        # resolved by rustfmt's silent .rustfmt.toml precedence.
        (self.app / ".rustfmt.toml").write_text("disable_all_formatting = true\n")
        self.assert_failure(self.helper("check"), ".rustfmt.toml conflicts with the required rustfmt.toml")

    def test_missing_config_wrong_cwd_and_toolchain_overrides_fail(self):
        self.assert_failure(self.helper("check", cwd="/"), "must run from /workspace")
        config = self.app / "rustfmt.toml"
        saved = config.read_text()
        config.unlink()
        self.assert_failure(self.helper("check"), "missing rustfmt.toml")
        config.write_text(saved)
        self.assert_failure(self.helper("check", extra_env={"RUSTUP_TOOLCHAIN": VERSION}), "masks rust-toolchain.toml")
        (self.app / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.0.0"\n')
        self.assert_failure(self.helper("check"), "the workspace toolchain is unavailable")

    def test_component_and_ancestor_shadow_settings_fail_before_fix(self):
        self.source("pub fn value( )->u8{1}\n")
        for rel in ["apps/Cargo.lock", "apps/probe/.cargo/config", "apps/probe/rust-toolchain",
                    "apps/probe/rustfmt.toml", "apps/.rustfmt.toml"]:
            with self.subTest(path=rel):
                path = self.app / rel
                path.parent.mkdir(exist_ok=True)
                path.write_text("")
                before = source_hashes(self.app)
                self.assert_failure(self.helper("fix"), rel)
                self.assertEqual(source_hashes(self.app), before)
                path.unlink()

    def test_group_gate_checks_every_member_even_outside_default_members(self):
        second = self.app / "apps/probe/extra"
        (second / "src").mkdir(parents=True)
        (second / "Cargo.toml").write_text(
            '[package]\nname="extra"\nversion="0.1.0"\nedition="2024"\n')
        (second / "src/lib.rs").write_text('#[test]\nfn second_member() { panic!("group member ran"); }\n')
        (self.app / "Cargo.toml").write_text(
            '[workspace]\nmembers=["apps/probe","apps/probe/extra","apps/sibling"]\n'
            'default-members=["apps/sibling"]\nresolver="3"\n')
        lock = self.native(self.app, ["cargo", "generate-lockfile", "--offline"], write=True)
        self.assertEqual(lock.returncode, 0, lock.stdout + lock.stderr)
        result = self.helper("test", members=(("apps/probe", "devkit-probe"), ("apps/probe/extra", "extra")))
        self.assert_failure(result, "group member ran")
        self.assertNotIn("ran tests from a sibling component", result.stdout + result.stderr)

    def test_fix_changes_only_selected_app(self):
        source = self.app / "apps/probe/src/lib.rs"
        sibling = self.app / "apps/sibling/src/lib.rs"
        self.source("pub fn value( )->u8{1}\n")
        sibling.write_text("pub fn sibling( )->u8{2}\n")
        before = source.read_bytes()
        sibling_before = sibling.read_bytes()
        result = self.helper("fix")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotEqual(source.read_bytes(), before)
        self.assertEqual(sibling.read_bytes(), sibling_before)

    def test_custom_harness_false_test_not_executed_during_check(self):
        manifest = self.app / "apps/probe/Cargo.toml"
        manifest.write_text(
            manifest.read_text()
            + '\n[[test]]\nname = "custom_harness"\nharness = false\n'
        )
        tests_dir = self.app / "apps/probe/tests"
        tests_dir.mkdir(parents=True, exist_ok=True)
        (tests_dir / "custom_harness.rs").write_text(
            'fn main() {\n'
            '    if std::env::args().any(|a| a == "--list") {\n'
            '        panic!("custom harness binary must not execute during make check");\n'
            '    }\n'
            '    println!("custom harness ran");\n'
            '}\n'
        )
        self.source("pub fn value() -> u8 {\n    1\n}\n")
        check = self.helper("check")
        self.assertEqual(check.returncode, 0, check.stdout + check.stderr)
        self.assertNotIn("custom harness binary must not execute", check.stdout + check.stderr)
        self.assertNotIn("custom harness ran", check.stdout + check.stderr)
        test = self.helper("test")
        self.assertEqual(test.returncode, 0, test.stdout + test.stderr)
        self.assertIn("custom harness ran", test.stdout + test.stderr)

    def test_required_features_targets_with_custom_harness(self):
        manifest = self.app / "apps/probe/Cargo.toml"
        base = manifest.read_text()
        tests_dir = self.app / "apps/probe/tests"
        tests_dir.mkdir(parents=True, exist_ok=True)
        (tests_dir / "custom_harness.rs").write_text('fn main() {\n    println!("custom ok");\n}\n')
        bin_dir = self.app / "apps/probe/src/bin"
        bin_dir.mkdir(parents=True, exist_ok=True)
        (bin_dir / "gated.rs").write_text('fn main() {}\n')
        self.source("pub fn value() -> u8 {\n    1\n}\n")

        # Valid: active custom-harness test + optional feature-gated bin.
        manifest.write_text(
            base
            + '\n[features]\nextra = []\n'
            + '\n[[bin]]\nname = "gated"\nrequired-features = ["extra"]\n'
            + '\n[[test]]\nname = "custom_harness"\nharness = false\n'
        )
        check = self.helper("check")
        self.assertEqual(check.returncode, 0, check.stdout + check.stderr)
        test = self.helper("test")
        self.assertEqual(test.returncode, 0, test.stdout + test.stderr)
        self.assertIn("custom ok", test.stdout + test.stderr)

        # Unmet required-features on the only harness=false test must fail check (0 runnable tests).
        manifest.write_text(
            base
            + '\n[features]\nextra = []\n'
            + '\n[[test]]\nname = "custom_harness"\nharness = false\nrequired-features = ["extra"]\n'
        )
        self.assert_failure(self.helper("check"), "non-ignored")

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
        with (self.app / "apps/probe/Cargo.toml").open("a") as manifest:
            manifest.write('\n[dependencies]\nlock-probe = { path = "lock-probe" }\n')
        dependency = self.app / "apps/probe/lock-probe"
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
