"""Authenticate producer handoffs before invoking the native assembler."""
import hashlib
import json
import os
from pathlib import Path
import subprocess


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


targets = {
    "amd64": "x86_64-unknown-linux-musl",
    "arm64": "aarch64-unknown-linux-musl",
    "darwin": "aarch64-apple-darwin",
}
for platform, target in targets.items():
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
    engine = directory / ("devkit-" + target)
    if producer["target"] != target or producer["binary"] != engine.name or sha256(engine) != producer["binarySha256"]:
        raise SystemExit(f"Wrong {platform} executable")
    engine.chmod(0o755)
    if platform == "amd64" and sha256(Path(".artifacts/npm-inputs/inputs.json")) != handoff["npm_inputs_sha256"]:
        raise SystemExit("Wrong npm inputs")
