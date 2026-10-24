"""Execute the real pre-checkout workflow admission with only gh reads stubbed."""
from __future__ import annotations

import json
from pathlib import Path
import re
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
