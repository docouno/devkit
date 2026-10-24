#!/usr/bin/env python3
"""Failure-oriented checks for the performance gate; no Docker is required."""
import argparse
from copy import deepcopy
import json
from pathlib import Path
import tempfile
import unittest

from measure import docker_kind, exec_counts, paired, snapshot
from verify import Gate, atomic_json, validate_build_set


class HarnessTests(unittest.TestCase):
    def test_under_sampled_matrix_cannot_report_success(self):
        for samples, warmups in ((29, 2), (30, 1)):
            with self.assertRaisesRegex(RuntimeError, "At least 30"):
                paired([], samples, warmups, self.fail, self.fail)

    def test_malformed_matrix_never_launches_a_child(self):
        with tempfile.TemporaryDirectory() as directory:
            case = {"name": "bad", "root": directory, "argv": "make build"}
            with self.assertRaisesRegex(RuntimeError, "argv"):
                paired([case], 30, 2, self.fail, self.fail)
            case["argv"] = ["make", "build"]
            with self.assertRaisesRegex(RuntimeError, "unique"):
                paired([case, deepcopy(case)], 30, 2, self.fail, self.fail)

    def test_command_failure_preserves_partial_samples_and_cannot_finish(self):
        with tempfile.TemporaryDirectory() as directory:
            case = {"name": "failing", "root": directory, "argv": ["make", "build"]}
            def run(case, index):
                return {"seconds": .1, "exitCode": 1 if index == 3 else 0, "stderr": "compiler error"}
            with self.assertRaisesRegex(RuntimeError, "compiler error"):
                paired([case], 30, 2, run, lambda: None)
            self.assertEqual(case["samplesSeconds"], [.1])
            self.assertNotIn("medianSeconds", case)

    def test_snapshot_detects_transitive_untracked_bytes_and_modes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".devkit").mkdir()
            (root / ".devkit/manifest.json").write_text(json.dumps({"components": {"apps/api": {}}}))
            path = root / "apps/api/vendor/local/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text("pub fn value() {}\n")
            before = snapshot(root)
            path.chmod(0o600)
            self.assertNotEqual(before, snapshot(root))
            path.chmod(0o644)
            path.write_text("pub fn changed() {}\n")
            self.assertNotEqual(before, snapshot(root))
            path.write_text("pub fn value() {}\n")
            self.assertEqual(before, snapshot(root))
            (root / "apps/api/target").mkdir()
            (root / "apps/api/target/output").write_text("native output")
            self.assertEqual(before, snapshot(root))

    def test_index_bytes_not_a_self_consistent_changed_asset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "engine").write_bytes(b"original engine")
            index = {"schemaVersion": 1, "version": "0.3.0", "target": "x86_64-unknown-linux-musl",
                     "sourceCommit": "a" * 40, "sourceTree": "b" * 40, "publicationEligible": True,
                     "files": {"engine": {"size": 1, "sha256": "c" * 64}}}
            atomic_json(root / "release-index.json", index)
            with self.assertRaisesRegex(RuntimeError, "asset identity"):
                validate_build_set(root)

    def test_failed_receipt_is_materialized(self):
        with tempfile.TemporaryDirectory() as directory:
            args = argparse.Namespace(output=Path(directory) / "receipt.json", warmups=2)
            gate = Gate(args)
            try:
                gate.receipt["error"] = "deliberate failed measurement"
                gate.save()
                receipt = json.loads(args.output.read_text())
                self.assertFalse(receipt["success"])
                self.assertEqual(receipt["schemaVersion"], 1)
            finally:
                import shutil
                shutil.rmtree(gate.scratch)

    def test_exact_exec_scope_and_control_classification(self):
        summary = " 99.00 0.010 5 70 8 execve\n 1.00 0.001 1 2 execveat\n"
        self.assertEqual(exec_counts(summary), {"execSyscallAttempts": 72, "execSyscallErrors": 8, "successfulExecs": 64})
        self.assertEqual(docker_kind(["compose", "--file", "x", "run", "--name", "owned", "--rm", "control", "node"]), "control")
        self.assertEqual(docker_kind(["run", "--rm", "image", "/devkit/devkit", "internal", "rust", "build"]), "tool")
        self.assertIsNone(docker_kind(["container", "inspect", "owned"]))


if __name__ == "__main__":
    unittest.main()
