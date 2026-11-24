"""Build a verified native contribution from committed files in a fresh Cargo target."""
import argparse
import filecmp
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import tempfile


def git(repo, *args):
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, GIT_NO_REPLACE_OBJECTS="1",
               GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0")
    return subprocess.check_output(["git", "-C", repo, "-c", "core.hooksPath=" + os.devnull,
                                    "-c", "core.fsmonitor=false", *args], env=env)


def require_clean(root):
    if git(root, "status", "--porcelain", "--untracked-files=all"):
        raise SystemExit("Verified contribution requires a clean tracked source tree")


def checkout(root, snapshot):
    require_clean(root)
    commit = git(root, "rev-parse", "HEAD").decode().strip()
    tree = git(root, "rev-parse", commit + "^{tree}").decode().strip()
    git(root, "clone", "--quiet", "--no-checkout", "--shared", "--", str(root), str(snapshot))
    git(snapshot, "checkout", "--quiet", "--detach", commit)
    # Fresh index entries have no skip-worktree or assume-unchanged state.
    # Compare the caller's actual bytes, including executable class, to this checkout.
    for name in git(snapshot, "ls-files", "-z").split(b"\0"):
        if not name:
            continue
        relative = os.fsdecode(name)
        source, frozen = root / relative, snapshot / relative
        source_mode, frozen_mode = source.lstat().st_mode, frozen.lstat().st_mode
        if (not stat.S_ISREG(source_mode) or not stat.S_ISREG(frozen_mode)
                or bool(source_mode & 0o111) != bool(frozen_mode & 0o111)
                or not filecmp.cmp(source, frozen, shallow=False)):
            raise SystemExit(f"Tracked producer source differs from HEAD: {relative}")
    return commit, tree


def linux_build(snapshot, env):
    # Seed only downloaded registry inputs. Re-extract crates against Cargo.lock and
    # compile into the new checkout's empty target directory.
    home = snapshot / ".artifacts/author/cargo-home"
    prepared = Path(env["DEVKIT_AUTHOR_CARGO_HOME"])
    for rel in ("registry/index", "registry/cache", "git/db"):
        if (prepared / rel).is_dir():
            shutil.copytree(prepared / rel, home / rel)
    env["DEVKIT_AUTHOR_CARGO_HOME"] = str(home)
    subprocess.run(["make", "--no-print-directory", "frozen-notices", "build_id=native"],
                   cwd=snapshot, env=env, check=True)
    work = snapshot / ".artifacts/author"
    engine = work / "target" / env["DEVKIT_AUTHOR_TRIPLE"] / "release/devkit"
    return engine, work / "native-notices", work / "native-notice-producer.json", []


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    root = Path.cwd()
    scratch = root / ".artifacts"
    scratch.mkdir(exist_ok=True)
    output = args.output.absolute()
    if output.exists() or output.is_symlink() or not output.parent.is_dir():
        raise SystemExit("Contribution output must be new and have an existing parent")
    with tempfile.TemporaryDirectory(prefix="native-producer-", dir=scratch) as temporary:
        snapshot = Path(temporary) / "source"
        commit, tree = checkout(root, snapshot)
        env = dict(os.environ, DEVKIT_FROZEN_COMMIT=commit, DEVKIT_FROZEN_TREE=tree)
        # Do not replay outer Make overrides against the isolated source/target.
        for key in ("MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES", "GNUMAKEFLAGS"):
            env.pop(key, None)
        if platform.system() == "Darwin":
            work = snapshot / ".artifacts/darwin-host"
            subprocess.run(["/bin/sh", "tooling/darwin-producer.sh", "--build-frozen"], cwd=snapshot, env=env, check=True)
            engine = work / "target/aarch64-apple-darwin/release/devkit"
            notices, receipt, extra = work / "notices", work / "notice-producer.json", ["--host-env", work / "host.json"]
        else:
            engine, notices, receipt, extra = linux_build(snapshot, env)
        sha = json.loads(receipt.read_text())["noticeSetSha256"]
        # The verified engine rechecks every original tracked blob against its own
        # embedded commit/tree before publishing the output with a no-replace rename.
        subprocess.run([engine, "internal", "contribute", "--verified", "--root", root,
                        "--engine", engine, "--notices", notices, "--notice-set-sha256", sha,
                        "--build-run", env.get("DEVKIT_BUILD_RUN") or env.get("BUILD_RUN", "local-author"),
                        "--output", output, *extra], cwd=snapshot, env=env, check=True)


if __name__ == "__main__":
    main()
