#!/usr/bin/env python3
"""Failure-oriented checks for the performance gate; no Docker is required."""
import argparse
from copy import deepcopy
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
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

    @unittest.skipUnless(sys.platform == "linux", "Owned process-group probe uses Linux child subreaping")
    def test_timeout_reaps_descendant_after_group_leader_exits(self):
        # Subreaping is confined to this helper process. Its finally block owns
        # and reaps the exact forked child even when the regression reproduces.
        probe = r'''
import ctypes,json,os,signal,subprocess,sys,time
from pathlib import Path
from measure import execute
libc=ctypes.CDLL(None,use_errno=True)
assert libc.prctl(36,1,0,0,0)==0, ctypes.get_errno()
pidfile=Path(sys.argv[1])
leader="""import os,sys,time
from pathlib import Path
child=os.fork()
if child:
    Path(sys.argv[1]).write_text(str(child))
    os._exit(0)
time.sleep(60)
"""
child=None
reaped=False
try:
    try:
        execute([sys.executable,'-c',leader,str(pidfile)],pidfile.parent,dict(os.environ),timeout=.2)
        raise AssertionError('expected inherited-pipe timeout')
    except subprocess.TimeoutExpired:
        pass
    child=int(pidfile.read_text())
    deadline=time.monotonic()+1
    while time.monotonic()<deadline:
        found,status=os.waitpid(child,os.WNOHANG)
        if found:
            reaped=True
            assert os.WIFSIGNALED(status), status
            break
        time.sleep(.01)
    assert reaped, 'owned descendant survived timeout after leader exit'
    print(json.dumps({'timeoutRaised':True,'descendantReaped':True}))
finally:
    if child is None and pidfile.exists():
        child=int(pidfile.read_text())
    if child is not None and not reaped:
        try: os.kill(child,signal.SIGKILL)
        except ProcessLookupError: pass
        os.waitpid(child,0)
'''
        with tempfile.TemporaryDirectory(prefix="devkit-perf-child-test-") as directory:
            environment = dict(os.environ, PYTHONPATH=str(Path(__file__).parent.resolve()))
            result = subprocess.run([sys.executable, "-c", probe, str(Path(directory) / "child.pid")],
                                    capture_output=True, text=True, env=environment, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(json.loads(result.stdout)["descendantReaped"])

    def traced_gate(self, records, operation="status"):
        args = argparse.Namespace(output=Path("unused.json"), warmups=2)
        gate = Gate(args)
        self.addCleanup(shutil.rmtree, gate.scratch)
        gate.logger = gate.scratch / "logger"
        gate.strace = ["synthetic-strace"]
        case = {"name": "probe", "root": str(gate.scratch), "argv": ["make", operation],
                "side": "native", "operation": operation, "componentCount": 1}
        def run(label, argv, **kwargs):
            if label.endswith("-docker-trace"):
                path = Path(kwargs["env"]["DEVKIT_PERF_DOCKER_LOG"])
                path.write_text("".join(json.dumps({"argv": command, "exitCode": 0, "seconds": .01}) + "\n"
                                        for command in records))
            else:
                Path(argv[argv.index("-o") + 1]).write_text(" 100.00 0.01 5 4 execve\n")
            return {"seconds": .1, "exitCode": 0, "stdout": "", "stderr": ""}
        gate.run = run
        return gate, case

    def test_metadata_trace_rejects_unrecognized_creation_and_mutation(self):
        probes = (
            [["create", "--name", "control-probe", "--label", "io.docouno.devkit.role=control", "control-image"],
             ["start", "--attach", "control-probe"]],
            [["container", "create", "image"]],
            [["container", "start", "control-probe"]],
            [["compose", "--file", "compose.yaml", "up", "--detach"]],
            [["compose", "--project-name", "probe", "create", "control"]],
            [["volume", "create", "unexpected"]],
            [["system", "prune", "--force"]],
        )
        for records in probes:
            with self.subTest(records=records):
                gate, case = self.traced_gate(records)
                with self.assertRaisesRegex(RuntimeError, "Unsupported Docker"):
                    gate.trace(case)
                self.assertFalse(gate.receipt["success"])

    def test_metadata_trace_keeps_readonly_queries_and_cleanup(self):
        records = [["compose", "--file", "compose.yaml", "version"],
                   ["info", "--format", "{{.OSType}}"],
                   ["image", "inspect", "accepted-image"],
                   ["container", "inspect", "owned"],
                   ["compose", "--project-name", "probe", "ps", "--format", "json"],
                   ["container", "stop", "owned"], ["container", "rm", "owned"]]
        gate, case = self.traced_gate(records)
        gate.trace(case)
        self.assertEqual(case["trace"]["dockerCliCalls"], len(records))
        self.assertEqual(case["trace"]["oneShotContainers"], 0)
        self.assertEqual([entry["argv"] for entry in case["trace"]["dockerRecords"]], records)
        self.assertEqual(case["trace"]["hostExecSummaryText"], " 100.00 0.01 5 4 execve\n")


if __name__ == "__main__":
    unittest.main()
