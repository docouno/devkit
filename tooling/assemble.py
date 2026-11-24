"""Pass frozen Make data to the release assembler without shell evaluation."""
import hashlib
import os
from pathlib import Path
import shlex
import subprocess

env = os.environ
root = Path(env["DEVKIT_AUTHOR_ROOT"])
local = env["DEVKIT_ASSEMBLE_LOCAL"] == "--local"
inputs = shlex.split(env["DEVKIT_TARGET_INPUTS"])
if not inputs:
    if not local:
        raise SystemExit('Set target_inputs="triple:directory:sha256 ..." for every release target.')
    directory = root / ".artifacts/author" / (env["DEVKIT_BUILD_ID"] + "-contribution")
    sha = hashlib.sha256((directory / "producer.json").read_bytes()).hexdigest()
    inputs = [f'{env["DEVKIT_AUTHOR_TRIPLE"]}:{directory}:{sha}']


def revision(ref):
    return subprocess.check_output(
        ["git", "-C", root, "rev-parse", ref], env={"PATH": env["PATH"]}, text=True
    ).strip()


engine = env["DEVKIT_AUTHOR_ENGINE"]
argv = [engine, "internal", "assemble", "--root", str(root), "--source-commit", revision("HEAD"),
        "--source-tree", revision("HEAD^{tree}"), "--output", env["DEVKIT_ASSEMBLY_OUTPUT"],
        "--npm-inputs", env["DEVKIT_NPM_INPUTS"], "--build-run", env["DEVKIT_BUILD_RUN"]]
if local:
    argv.append("--local")
for item in inputs:
    argv.extend(["--target-input", item])
os.execv(engine, argv)
