"""Authenticate producer handoffs before invoking the native assembler."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def sha256(path):
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


TARGETS = {
    "amd64": "x86_64-unknown-linux-musl",
    "arm64": "aarch64-unknown-linux-musl",
    "darwin": "aarch64-apple-darwin",
}
requested = sys.argv[1:]
if not requested or len(set(requested)) != len(requested) or any(p not in TARGETS for p in requested):
    raise SystemExit("Usage: check-contributions.py <amd64|arm64|darwin>...")
commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
tree = subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], text=True).strip()
version = Path("VERSION").read_text().strip()
run_prefix = f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}/attempts/"
for platform in requested:
    target = TARGETS[platform]
    handoff = json.loads(os.environ[platform.upper()])
    path = f"repos/{os.environ['GITHUB_REPOSITORY']}/actions/artifacts/{int(handoff['artifact_id'])}"
    artifact = json.loads(subprocess.check_output(["gh", "api", path], timeout=60))
    if (artifact["digest"] != "sha256:" + handoff["artifact_digest"]
            or artifact["name"] != handoff["contribution_artifact"]
            or artifact["workflow_run"]["id"] != int(os.environ["GITHUB_RUN_ID"])):
        raise SystemExit(f"Wrong {platform} producer artifact")
    directory = Path(".artifacts", platform)
    receipt = directory / "producer.json"
    if sha256(receipt) != handoff["contribution_sha256"]:
        raise SystemExit(f"Wrong {platform} producer receipt")
    producer = json.loads(receipt.read_bytes())
    info = producer["buildInfo"]
    attempt = re.fullmatch(re.escape(run_prefix) + r"([1-9][0-9]*)", producer["buildRun"])
    if (producer.get("verified") is not True or info.get("sourceVerified") is not True
            or (info["sourceCommit"], info["sourceTree"], info["version"], info["target"]) != (commit, tree, version, target)
            or attempt is None or int(attempt[1]) > int(os.environ["GITHUB_RUN_ATTEMPT"])):
        raise SystemExit(f"Wrong {platform} source or build run")
    engine = directory / ("devkit-" + target)
    if producer["target"] != target or producer["binary"] != engine.name or sha256(engine) != producer["binarySha256"]:
        raise SystemExit(f"Wrong {platform} executable")
    engine.chmod(0o755)
    if platform == "amd64" and sha256(Path(".artifacts/npm-inputs/inputs.json")) != handoff["npm_inputs_sha256"]:
        raise SystemExit("Wrong npm inputs")
