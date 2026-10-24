"""Execute the real pre-checkout workflow admission with only gh reads stubbed."""
from __future__ import annotations

import json
from pathlib import Path
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


class EventSourceGuard(unittest.TestCase):
    def probe(self, event, status="identical"):
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
                    print(json.dumps({'default_branch': 'main'}))
                else:
                    assert sys.argv[2] == 'repos/docouno/devkit/compare/' + os.environ['SOURCE_COMMIT'] + '...main'
                    print(json.dumps({'status': os.environ['MAIN_STATUS']}))
                '''))
            gh.chmod(0o755)
            environment = {"PATH": str(root), "SOURCE_COMMIT": SOURCE,
                           "VERSION": "0.3.0", "GUARD_CALLS": str(calls),
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

    def test_equal_event_and_source_on_main_are_admitted(self):
        result, calls = self.probe(SOURCE)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(json.loads(calls[1])[1], f"repos/docouno/devkit/compare/{SOURCE}...main")

    def test_original_run_rerun_remains_admitted_after_main_moves(self):
        result, calls = self.probe(SOURCE, "ahead")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)


if __name__ == "__main__":
    unittest.main()
