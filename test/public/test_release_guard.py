"""Execute the real pre-checkout workflow admission with only gh reads stubbed."""
from __future__ import annotations

import json
import os
import runpy
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

REPO = Path(__file__).resolve().parents[2]
SOURCE = "a" * 40
NEWER_EVENT = "b" * 40


def admission_script():
    lines = (REPO / ".github/workflows/release.yml").read_text().splitlines()
    start = lines.index("      - name: Admit reviewed main source before executing checkout code")
    checkout = next(i for i, line in enumerate(lines) if "uses: actions/checkout@" in line)
    if start >= checkout:
        raise AssertionError("Source admission must run before checkout code")
    run = next(i for i in range(start, checkout) if lines[i] == "        run: |")
    shell = textwrap.dedent("\n".join(lines[run + 1:checkout]))
    python = shell.split("python3 - <<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
    compile(python, "release.yml admission", "exec")
    return python


def artifact_uploads(source):
    """Read upload input blocks without adding a YAML dependency to early guards."""
    lines = source.splitlines()
    for start, line in enumerate(lines):
        if not re.match(r"\s*(?:- )?uses: actions/upload-artifact@", line):
            continue
        indent = len(line) - len(line.lstrip()) + (2 if line.lstrip().startswith("- ") else 0)
        end = next((i for i in range(start + 1, len(lines))
                    if lines[i].strip() and len(lines[i]) - len(lines[i].lstrip()) < indent), len(lines))
        block = lines[start + 1:end]
        with_line = " " * indent + "with:"
        if with_line not in block:
            raise AssertionError("Artifact upload has no explicit inputs")
        inputs = {}
        for i in range(block.index(with_line) + 1, len(block)):
            match = re.fullmatch(" " * (indent + 2) + r"([a-z-]+):\s*(.*)", block[i])
            if match is None:
                continue
            key, value = match.groups()
            if value in ("|", ">"):
                children = []
                for child in block[i + 1:]:
                    if child.strip() and len(child) - len(child.lstrip()) <= indent + 2:
                        break
                    children.append(child.strip())
                value = "\n".join(children)
            inputs[key] = value.strip("'\"")
        if not inputs.get("path"):
            raise AssertionError("Artifact upload has no explicit path allowlist")
        yield inputs


def missing_hidden_upload_flags(source):
    missing = []
    for inputs in artifact_uploads(source):
        paths = [line.strip().strip("'\"") for line in inputs["path"].splitlines()
                 if line.strip() and not line.lstrip().startswith(("!", "#"))]
        hidden = any(part.startswith(".") and part not in (".", "..")
                     for path in paths for part in re.split(r"[/\\]", path))
        if hidden and inputs.get("include-hidden-files") != "true":
            missing.append(inputs["path"])
    return missing


class HiddenArtifactGuard(unittest.TestCase):
    def test_every_hidden_upload_in_workflows_and_composites_explicitly_opts_in(self):
        uploads = 0
        for path in (REPO / ".github").rglob("*.y*ml"):
            source = path.read_text()
            uploads += len(list(artifact_uploads(source)))
            self.assertEqual(missing_hidden_upload_flags(source), [], str(path))
        self.assertGreater(uploads, 0, "Guard must inspect actual artifact uploads")

    def test_removing_each_actual_upload_opt_in_is_detected(self):
        for path in (REPO / ".github").rglob("*.y*ml"):
            source = path.read_text()
            flags = list(re.finditer(r"(?m)^\s+include-hidden-files: true\n", source))
            for flag in flags:
                mutated = source[:flag.start()] + source[flag.end():]
                self.assertEqual(len(missing_hidden_upload_flags(mutated)), 1, str(path))

    def test_hidden_parent_or_basename_requires_opt_in(self):
        template = "  - uses: actions/upload-artifact@pinned\n    with:\n      path: {}\n"
        for path in (".artifacts/frozen/*.json", "reports/.hidden/receipt.json", ".coverage"):
            self.assertEqual(missing_hidden_upload_flags(template.format(path)), [path])
        self.assertEqual(missing_hidden_upload_flags(template.format("reports/receipt.json")), [])
        self.assertEqual(missing_hidden_upload_flags(template.format("|\n        reports/receipt.json\n        !.private/**")), [])


class NativeProducerGuard(unittest.TestCase):
    def test_fresh_checkout_enters_native_build_with_default_darwin_output(self):
        with tempfile.TemporaryDirectory(prefix="devkit-fresh-producer-") as temporary:
            base = Path(temporary)
            source = base / "source"
            (source / "tooling").mkdir(parents=True)
            for rel in ("tooling/frozen-producer.py", "tooling/darwin-producer.sh", "rust-toolchain.toml"):
                shutil.copy2(REPO / rel, source / rel)
            (source / ".gitignore").write_text(".artifacts/\n")
            subprocess.run(["git", "init", "--quiet"], cwd=source, check=True)
            subprocess.run(["git", "add", "."], cwd=source, check=True)
            subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                            "commit", "--quiet", "-m", "fixture"], cwd=source, check=True)

            # Exercise the hosted shell entry on either OS. Stop at the pinned
            # toolchain boundary after the real producer has made its checkout.
            shims = base / "bin"
            shims.mkdir()
            uname = shims / "uname"
            uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; *) exec /usr/bin/uname "$@";; esac\n')
            uname.chmod(0o755)
            for name in ("rustup", "make"):
                native = shims / name
                native.write_text('#!/bin/sh\npwd > "$PROBE_NATIVE_CWD"\nexit 43\n')
                native.chmod(0o755)
            marker = base / "native-cwd"
            env = dict(os.environ, PATH=str(shims) + os.pathsep + os.environ["PATH"],
                       PROBE_NATIVE_CWD=str(marker), DEVKIT_AUTHOR_CARGO_HOME=str(base / "cache"))
            output = source / ".artifacts/contribution"
            self.assertFalse(output.parent.exists())
            result = subprocess.run(["tooling/darwin-producer.sh", output], cwd=source, env=env,
                                    text=True, capture_output=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(marker.is_file(), result.stderr)
            snapshot = Path(marker.read_text().strip())
            self.assertEqual(snapshot.name, "source")
            self.assertEqual(snapshot.parent.parent.resolve(), output.parent.resolve())
            self.assertFalse(output.exists())

    def test_verified_checkout_ignores_local_artifacts_and_rejects_hidden_source_changes(self):
        producer = runpy.run_path(str(REPO / "tooling/frozen-producer.py"))
        with tempfile.TemporaryDirectory(prefix="devkit-producer-") as temporary:
            base = Path(temporary)
            source = base / "source"
            source.mkdir()

            def git(*args):
                return subprocess.check_output(["git", "-C", source, *args], text=True).strip()

            git("init", "--quiet")
            (source / "engine.rs").write_text("committed")
            (source / ".gitignore").write_text(".artifacts/\n")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "commit", "--quiet", "-m", "fixture")
            artifacts = source / ".artifacts"
            artifacts.mkdir()
            (artifacts / "stale-compiler-output").write_text("unverified")
            stage = artifacts / "snapshot"
            self.assertEqual(producer["checkout"](source, stage), (git("rev-parse", "HEAD"), git("rev-parse", "HEAD^{tree}")))
            self.assertFalse((stage / ".artifacts").exists())
            for name, flag in [("assumed", "assume-unchanged"), ("skipped", "skip-worktree")]:
                git("update-index", "--" + flag, "engine.rs")
                (source / "engine.rs").write_text("different")
                self.assertEqual(git("status", "--porcelain"), "")
                with self.assertRaisesRegex(SystemExit, "Tracked producer source differs"):
                    producer["checkout"](source, artifacts / name)
                self.assertEqual((stage / "engine.rs").read_text(), "committed")
                (source / "engine.rs").write_text("committed")
                git("update-index", "--no-" + flag, "engine.rs")


class RequiredNativeCheckGuard(unittest.TestCase):
    def job(self, name, workflow="verify"):
        source = (REPO / ".github/workflows" / (workflow + ".yml")).read_text()
        return re.search(r"(?ms)^  " + name + r":\n(.*?)(?=^  [\w-]+:|\Z)", source).group(1)

    def test_only_an_explicit_successful_assembly_may_start_workers(self):
        ci, release = self.job("verify", "ci"), self.job("verify", "release")
        for caller in (ci, release):
            self.assertIn("assembly_result: ${{ needs.assemble.result }}", caller)
            self.assertIn("source_amd64_result: ${{ needs.source-linux-amd64.result }}", caller)
            self.assertIn("source_arm64_result: ${{ needs.source-linux-arm64.result }}", caller)
            self.assertIn("frozen: ${{ toJSON(needs.assemble.outputs) }}", caller)
        # Keep the caller job key unadorned so Ruleset 25804809 required checks stay "verify / <gate>".
        self.assertNotIn("name:", ci)
        self.assertIn("if: '!cancelled()'", ci)
        self.assertIn("state: fresh", ci)
        self.assertIn("state: ${{ needs.admit-and-inspect.outputs.state }}", release)
        handoff = self.job("handoff")
        for env, arg in (("STATE", "state"), ("ASSEMBLY_RESULT", "assembly_result"), ("FROZEN", "frozen")):
            self.assertIn(env + ": ${{ inputs." + arg + " }}", handoff)
        script = textwrap.dedent(
            handoff.split("        run: |\n", 1)[1].split("      - uses: actions/checkout@", 1)[0]
        )
        frozen = dict(version="0.6.0", source_commit="a" * 40, source_tree="b" * 40,
                      index_sha256="c" * 64, frozen_set_artifact="devkit-frozen-set-2")
        preview = dict(preview_artifact="devkit-darwin-preview-2", preview_index_sha256="d" * 64)
        complete = {**frozen, **preview}
        cases = [
            ("success", "fresh", complete, True),
            ("success", "restored", frozen, True),
            ("success", "incomplete", frozen, True),
            ("success", "fresh", frozen, False),
            ("success", "fresh", {**complete, "preview_artifact": ""}, False),
            ("success", "fresh", {**complete, "preview_artifact": "devkit-darwin-preview-3"}, False),
            ("success", "fresh", {**complete, "preview_index_sha256": ""}, False),
            ("success", "fresh", {**complete, "preview_index_sha256": "z" * 64}, False),
            ("success", "restored", {}, False),
            ("success", "unknown", complete, False),
            # Failure during preview assembly leaves only the Linux outputs.
            ("failure", "fresh", frozen, False),
        ]
        # An upload may fail after both sets of step outputs have been written.
        for result in ("failure", "skipped", "cancelled"):
            cases.append((result, "fresh", complete, False))
        for result, state, outputs, valid in cases:
            env = dict(os.environ, STATE=state, ASSEMBLY_RESULT=result, FROZEN=json.dumps(outputs),
                       GITHUB_RUN_ATTEMPT="3")
            with self.subTest(result=result, state=state, outputs=outputs):
                run = subprocess.run(["bash", "-c", script], env=env, capture_output=True, text=True, timeout=5)
                self.assertEqual(run.returncode == 0, valid, run.stderr)

        self.assertIn("legacy_checkout_sha256: ${{ steps.legacy.outputs.sha256 }}", handoff)
        darwin = self.job("darwin-host")
        self.assertNotIn("if: env.PREVIEW_ARTIFACT", darwin)
        self.assertIn("LEGACY_SHA256: ${{ needs.handoff.outputs.legacy_checkout_sha256 }}", darwin)
        self.assertIn('test "$(shasum -a 256 .artifacts/legacy-handoff/legacy.bundle | cut -d \' \' -f 1)" = "$LEGACY_SHA256"', darwin)
        self.assertIn("if: inputs.state == 'fresh'\n        run: |", darwin)
        self.assertIn("if: inputs.state == 'restored' || inputs.state == 'incomplete'\n"
                      "        run: python3 test/bootstrap/portable.py", darwin)

    def test_each_required_check_rejects_an_unsuccessful_or_skipped_worker(self):
        workers = {"performance", "cross-host-checkout"}
        for group in ("source", "runtime", "delivery", "packaged", "registry"):
            job = self.job(group)
            self.assertIn(f"    name: {group}\n", job)
            self.assertIn("    if: '!cancelled()'\n", job)
            dependencies = re.search(r"(?m)^    needs: \[(.+)\]$", job).group(1).split(", ")
            if group == "source":
                expected = ["darwin-host"]
            elif group == "packaged":
                expected = ["cross-host-checkout", "packaged-linux-amd64", "packaged-linux-arm64"]
            else:
                expected = [group + "-linux-amd64", group + "-linux-arm64"]
            self.assertCountEqual(dependencies, ["handoff", *expected])
            workers.update(expected)
            if group == "source":
                predicate = textwrap.dedent(job.split("        run: |\n", 1)[1])
                for field in ("handoff", "darwin-host", "SOURCE_AMD64", "SOURCE_ARM64"):
                    for status in ("success", "failure", "cancelled", "skipped"):
                        results = {name: {"result": "success"} for name in dependencies}
                        env = {"RESULTS": json.dumps(results), "SOURCE_AMD64": "success", "SOURCE_ARM64": "success"}
                        if field in results:
                            results[field]["result"] = status
                            env["RESULTS"] = json.dumps(results)
                        else:
                            env[field] = status
                        probe = subprocess.run(["bash", "-c", predicate], env=env,
                                               capture_output=True, text=True, timeout=5)
                        self.assertEqual(probe.returncode == 0, status == "success", (group, field, status, probe.stderr))
            else:
                predicate = re.search(r"(?m)^        run: (.+)$", job).group(1)
                for worker in ("handoff", expected[-1]):
                    for status in ("success", "failure", "cancelled", "skipped"):
                        results = {name: {"result": "success"} for name in dependencies}
                        results[worker]["result"] = status
                        probe = subprocess.run(["bash", "-c", predicate], env={"RESULTS": json.dumps(results)},
                                               capture_output=True, text=True, timeout=5)
                        self.assertEqual(probe.returncode == 0, status == "success", (group, worker, status, probe.stderr))
        for name in workers:
            # The cross-host consumers inherit the gate through their export worker.
            worker = self.job(name)
            self.assertNotRegex(worker, r"(?m)^    if: .*(?:always|!cancelled)\(")
            parent = re.search(r"(?m)^    needs: ([\w-]+)$", worker).group(1)
            if parent != "handoff":
                self.assertIn("    needs: handoff\n", self.job(parent), name)

    def test_release_workflow_admission_needs_and_sha256_handoffs(self):
        admit = self.job("admit-and-inspect", "release")
        self.assertIn("if: github.repository == 'docouno/devkit' && github.ref == 'refs/heads/main'", admit)
        verify = self.job("verify", "release")
        verify_needs = [item.strip() for item in re.search(r"(?ms)^    needs:\s*\[([^\]]+)\]", verify).group(1).split(",") if item.strip()]
        self.assertCountEqual(
            verify_needs,
            ["admit-and-inspect", "assemble", "build-linux-amd64", "build-linux-arm64",
             "source-linux-amd64", "source-linux-arm64"],
        )
        self.assertIn("run_performance: true", verify)
        publish = self.job("publish", "release")
        publish_needs = [item.strip() for item in re.search(r"(?ms)^    needs:\s*\[([^\]]+)\]", publish).group(1).split(",") if item.strip()]
        self.assertCountEqual(publish_needs, ["assemble", "build-linux-amd64", "verify"])
        for digest_env in ("INDEX_SHA256", "CONTROLLER_SHA256", "PERFORMANCE_SHA256"):
            self.assertIn(digest_env + ":", publish)
            self.assertIn(f'"${digest_env}"', publish)
        verify_group = (REPO / ".github/workflows/verify-group.yml").read_text()
        self.assertIn('"$LEGACY_SHA256"', verify_group)
        self.assertIn('"$CHECKOUT_SHA256"', verify_group)


class EventSourceGuard(unittest.TestCase):
    def probe(self, event, status="identical", *, default_branch="main", version="0.3.0"):
        with tempfile.TemporaryDirectory(prefix="release-admission-") as directory:
            root = Path(directory)
            calls = root / "calls.jsonl"
            gh = root / "gh"
            gh.write_text(f"#!{sys.executable}\n" + textwrap.dedent('''\
                import json, os, sys
                from pathlib import Path
                with open(os.environ['GUARD_CALLS'], 'a') as log:
                    log.write(json.dumps(sys.argv[1:]) + '\\n')
                assert sys.argv[1] == 'api'
                if sys.argv[2] == 'repos/docouno/devkit':
                    print(json.dumps({'default_branch': os.environ['DEFAULT_BRANCH']}))
                else:
                    assert sys.argv[2] == 'repos/docouno/devkit/compare/' + os.environ['SOURCE_COMMIT'] + '...main'
                    print(json.dumps({'status': os.environ['MAIN_STATUS']}))
                '''))
            gh.chmod(0o755)
            environment = {"PATH": str(root), "SOURCE_COMMIT": SOURCE,
                           "VERSION": version, "GUARD_CALLS": str(calls),
                           "DEFAULT_BRANCH": default_branch,
                           "MAIN_STATUS": status, "PYTHONDONTWRITEBYTECODE": "1"}
            if event is not None:
                environment["GITHUB_SHA"] = event
            result = subprocess.run([sys.executable, "-c", admission_script()],
                                    env=environment, capture_output=True, text=True, timeout=10)
            return result, calls.read_text().splitlines() if calls.exists() else []

    def test_missing_event_sha_fails_before_any_provider_read(self):
        result, calls = self.probe(None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("authentic event GITHUB_SHA", result.stderr)
        self.assertEqual(calls, [])

    def test_new_dispatch_of_historical_source_fails_before_checkout(self):
        result, calls = self.probe(NEWER_EVENT, "ahead")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("gh run rerun ORIGINAL_RUN_ID --failed", result.stderr)
        self.assertEqual(calls, [])

    def test_prerelease_version_fails_before_any_provider_read(self):
        result, calls = self.probe(SOURCE, version="0.6.0-rc.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Exact stable version required", result.stderr)
        self.assertEqual(calls, [])

    def test_non_main_default_branch_is_rejected(self):
        result, calls = self.probe(SOURCE, default_branch="master")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Reviewed release branch must be main", result.stderr)
        self.assertEqual(len(calls), 1)

    def test_diverged_or_behind_source_is_rejected(self):
        for status in ("diverged", "behind"):
            with self.subTest(status=status):
                result, calls = self.probe(SOURCE, status)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Source must be reviewed main or its ancestor", result.stderr)
                self.assertEqual(len(calls), 2)

    def test_equal_event_and_source_on_main_are_admitted(self):
        result, calls = self.probe(SOURCE)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(json.loads(calls[1])[1], f"repos/docouno/devkit/compare/{SOURCE}...main")

    def test_original_run_rerun_remains_admitted_after_main_moves(self):
        result, calls = self.probe(SOURCE, "ahead")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)


REQUIRED_DOCS = (
    "README.md",
    "CHANGELOG.md",
    "runtime/GUIDE.md",
    "packages/env/README.md",
    "docs/architecture.md",
    "docs/workflows.md",
    "docs/publishing.md",
    "docs/ci.md",
    "docs/upgrades.md",
    "docs/migrations/index.json",
    "docs/migrations/0.5.0.md",
    "docs/migrations/0.6.0.md",
)


class DocumentationGuard(unittest.TestCase):
    def test_required_standalone_docs_exist_and_relative_links_resolve(self):
        for rel in REQUIRED_DOCS:
            self.assertTrue((REPO / rel).is_file(), f"Missing required documentation file: {rel}")

        link_re = re.compile(r"\[[^\]]+\]\(([^)]+)\)")
        for md in [REPO / "README.md", REPO / "runtime/GUIDE.md", REPO / "packages/env/README.md", *sorted((REPO / "docs").rglob("*.md"))]:
            # Strip fenced code blocks before scanning Markdown links.
            stripped = re.sub(r"(?ms)^```.*?^```", "", md.read_text())
            for raw_target in link_re.findall(stripped):
                target = raw_target.strip()
                if not target or target.startswith(("#", "http://", "https://", "mailto:")):
                    continue
                rel_path = target.split("#", 1)[0]
                if not rel_path:
                    continue
                resolved = (md.parent / rel_path).resolve()
                self.assertTrue(resolved.exists(), f"Broken relative link {raw_target!r} in {md.relative_to(REPO)}")


if __name__ == "__main__":
    unittest.main()
