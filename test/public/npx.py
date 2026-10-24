#!/usr/bin/env python3
"""Anonymous real registry npx admission, under normal/restrictive umasks."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import uuid

REPO = Path(__file__).resolve().parents[2]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def file_map(root):
    result = {}
    for path in sorted(root.rglob("*")):
        require(not path.is_symlink(), f"Unexpected package link: {path}")
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {
                "sha256": sha256(path.read_bytes()), "mode": path.stat().st_mode & 0o777}
    return result


SCRIPT = r'''
set -eu
test ! -e /var/run/docker.sock
umask "$1"; shift
spec=$1; shift
phase=$1; shift
if test "$phase" = acquire; then
  npm exec --yes --ignore-scripts --package "$spec" -- devkit --version
  npm exec --yes --offline --ignore-scripts --package "$spec" -- devkit --help > /proof/help.txt
  exit
fi
# The online phase above acquires the actual registry package. Every request
# during same-version native import is now denied and recorded independently.
node -e 'const fs=require("node:fs"),net=require("node:net");const server=net.createServer(s=>{fs.appendFileSync("/proof/network.log","connect\n");s.end("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")});server.listen(0,"127.0.0.1",()=>fs.writeFileSync("/proof/proxy.port",String(server.address().port)))' &
proxy=$!
trap 'kill "$proxy" 2>/dev/null || :; wait "$proxy" 2>/dev/null || :' 0
i=0
while test ! -s /proof/proxy.port; do i=$((i+1)); test "$i" -lt 100; sleep .02; done
HTTPS_PROXY=http://127.0.0.1:$(cat /proof/proxy.port)
HTTP_PROXY=$HTTPS_PROXY
export HTTPS_PROXY HTTP_PROXY
npm exec --yes --offline --ignore-scripts --package "$spec" -- devkit init /proof/project --app api=rust-api --name project --package-source local --json > /proof/init.json
set +e
npm exec --yes --offline --ignore-scripts --package "$spec" -- devkit --json > /proof/bare.json 2>/proof/bare.stderr
bare=$?
set -e
test "$bare" -ne 0
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-set", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--selector-mode", choices=("pinned", "latest"), default="pinned")
    parser.add_argument("--transport-output", type=Path,
                        help="Also verify and time the actual public cold curl/native entry")
    args = parser.parse_args()
    build_set = args.build_set.resolve()
    raw_index = (build_set / "release-index.json").read_bytes()
    index = json.loads(raw_index)
    require(index["publicationEligible"] is True, "Public npx requires the frozen eligible build")
    require(re.fullmatch(r"\d+\.\d+\.\d+", index["version"]), "Exact stable version required")
    engine = build_set / index["engine"]
    require(sha256(engine.read_bytes()) == index["files"][index["engine"]]["sha256"], "Engine bytes differ")
    engine.chmod(0o755)
    manifest_bytes = (build_set / "release.json").read_bytes()
    require(sha256(manifest_bytes) == index["releaseIdentity"], "Manifest differs")
    manifest = json.loads(manifest_bytes)
    expected = dict(manifest["files"])
    expected["release.json"] = {"sha256": index["releaseIdentity"], "mode": 0o644}
    pins = dict(line.split("=", 1) for line in (REPO / "toolchains.env").read_text().splitlines()
                if line and not line.startswith("#"))
    spec = "@docouno/devkit" + ("@" + index["version"] if args.selector_mode == "pinned" else "")
    cases = []
    cold = []
    success = False
    error = None
    started = time.monotonic()
    # Retain diagnostics and npm receipts alongside the output, not in source.
    args.output.parent.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="npx-public-", dir=args.output.parent)).resolve()
    try:
        for mask in ("022", "077"):
            output = scratch / mask
            output.mkdir()
            def docker_command(phase):
                token = uuid.uuid4().hex
                name = "devkit-public-npx-" + token
                command = [engine, "internal", "guard", name, token, "--", "docker", "run",
                       "--pull", "never", "--rm", "--init", "--name", name,
                       "--label", f"io.docouno.devkit.invocation={token}",
                       "--network", "bridge" if phase == "acquire" else "none", "--read-only", "--memory", "768m", "--cpus", "2",
                       "--user", f"{os.getuid()}:{os.getgid()}", "--cap-drop", "ALL",
                       "--security-opt", "no-new-privileges:true",
                       "--tmpfs", "/tmp:rw,exec,mode=1777,size=268435456",
                       "--mount", f"type=bind,source={output},target=/proof", "--workdir", "/proof"]
                for value in ("HOME=/tmp/home", "CI=true", "DEVKIT_CACHE_DIR=/proof/native-cache",
                          "npm_config_cache=/proof/npm-cache", "npm_config_userconfig=/tmp/user.npmrc",
                          "npm_config_globalconfig=/tmp/global.npmrc", "npm_config_update_notifier=false",
                          "npm_config_audit=false", "npm_config_fund=false",
                          "npm_config_registry=https://registry.npmjs.org/"):
                    command.extend(("--env", value))
                command.extend((pins["DEVKIT_NODE_IMAGE"], "sh", "-c", SCRIPT, "sh", mask, spec, phase))
                return command
            environment = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "DOCKER_HOST") if key in os.environ}
            for phase in ("acquire", "offline-import"):
                phase_started = time.monotonic()
                result = subprocess.run([str(v) for v in docker_command(phase)], cwd=scratch, env=environment,
                                        capture_output=True, text=True, timeout=240)
                phase_seconds = time.monotonic() - phase_started
                (output / f"{phase}.stdout.log").write_text(result.stdout)
                (output / f"{phase}.stderr.log").write_text(result.stderr)
                require(result.returncode == 0, f"Public npx {phase} failed under umask {mask}: {result.stderr[-6000:]}")
                if phase == "acquire":
                    require(result.stdout.strip() == "devkit " + index["version"], "Npx selector resolved another release")
                    cold.append({"transport": "npx", "umask": mask, "seconds": phase_seconds,
                                 "cacheState": "empty", "includes": "Docker transport startup and actual public registry npx --version"})
            installed = list((output / "npm-cache/_npx").glob("*/node_modules/@docouno/devkit"))
            require(len(installed) == 1, "Expected one registry-installed CLI")
            delivered = file_map(installed[0])
            require(set(delivered) == set(expected), "Npx package file set differs")
            for path, identity in expected.items():
                require(delivered[path]["sha256"] == identity["sha256"] and
                        bool(delivered[path]["mode"] & 0o111) == bool(identity["mode"] & 0o111),
                        f"Npx regular bytes/executable class differ: {path}")
            init = json.loads((output / "init.json").read_text())
            require(init["ok"] is True and init["result"]["releaseIdentity"] == index["releaseIdentity"], "Npx init differs")
            canonical = output / "native-cache/releases" / index["version"] / index["releaseIdentity"]
            require(file_map(canonical) == expected, "Npx import did not normalize canonical modes")
            require(file_map(installed[0]) == delivered, "Native import mutated npm source tree")
            require(not (output / "network.log").exists(), "Same-version native import attempted network access")
            bare = json.loads((output / "bare.json").read_text())
            require(bare["ok"] is False, "Bare non-TTY command must report missing arguments")
            require("devkit init" in (output / "help.txt").read_text(), "Public npm help missing usage")
            cases.append({"umask": mask, "registrySpec": spec, "files": len(delivered),
                          "engineSha256": delivered["devkit"]["sha256"], "sameVersionRequests": 0,
                          "canonicalReleaseIdentity": index["releaseIdentity"], "bareCiDiagnostic": True,
                          "nativeImportNetwork": "none"})
        if args.transport_output:
            require(args.selector_mode == "pinned", "Cold release proof must use a pinned entry")
            native = scratch / "curl-native"
            native.mkdir()
            blockers = native / "no-host-sdks"
            blockers.mkdir()
            forbidden = native / "host-sdk-attempts"
            for name in ("node", "npm", "npx", "cargo", "rustc", "rustup", "tsc", "eslint"):
                path = blockers / name
                path.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$DEVKIT_FORBIDDEN_HOST_SDK_LOG"\nexit 97\n')
                path.chmod(0o755)
            environment = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ}
            environment.update(PATH=str(blockers) + os.pathsep + environment["PATH"],
                               HOME=str(native / "home"), CI="true",
                               DEVKIT_CACHE_DIR=str(native / "empty-cache"),
                               DEVKIT_FORBIDDEN_HOST_SDK_LOG=str(forbidden))
            started_curl = time.monotonic()
            script = native / "install.sh"
            subprocess.run(["curl", "-q", "--fail", "--silent", "--show-error", "--location",
                            "--proto", "=https", "--proto-redir", "=https", "--max-time", "180",
                            "--max-filesize", "131072", "--output", str(script),
                            f'https://github.com/docouno/devkit/releases/download/v{index["version"]}/install.sh'],
                           check=True, env=environment, capture_output=True, timeout=190)
            require(sha256(script.read_bytes()) == index["files"]["install.sh"]["sha256"], "Actual public bootstrap differs from frozen bytes")
            result = subprocess.run(["sh", str(script), "init", str(native / "project"),
                                     "--app", "api=rust-api", "--name", "project", "--package-source", "local", "--json"],
                                    env=environment, capture_output=True, text=True, timeout=240)
            seconds = time.monotonic() - started_curl
            (native / "stdout.log").write_text(result.stdout)
            (native / "stderr.log").write_text(result.stderr)
            require(result.returncode == 0, f"Actual public cold curl init failed: {result.stderr[-6000:]}")
            value = json.loads(result.stdout)
            require(value["ok"] is True and value["result"]["releaseIdentity"] == index["releaseIdentity"], "Public curl/native release differs")
            require(not forbidden.exists(), "Curl/native attempted a host language SDK")
            cold.append({"transport": "curl", "seconds": seconds, "cacheState": "empty",
                         "includes": "actual public install.sh download, raw ELF acquisition, payload acquisition and Rust project init",
                         "hostLanguageSDKAttempted": False, "releaseIdentity": index["releaseIdentity"]})
        success = True
    except BaseException as failure:
        error = str(failure)
        raise
    finally:
        receipt = {"schemaVersion": 1, "success": success, "error": error,
                   "version": index["version"], "sourceCommit": index["sourceCommit"],
                   "sourceTree": index["sourceTree"], "indexSha256": sha256(raw_index),
                   "selectorMode": args.selector_mode, "ignoreScripts": True,
                   "authenticationUsed": False, "umasks": ["022", "077"],
                   "cases": cases, "durationSeconds": time.monotonic() - started,
                   "harnessSha256": sha256(Path(__file__).read_bytes()), "diagnostics": str(scratch)}
        args.output.write_text(json.dumps(receipt, indent=2) + "\n")
        if args.transport_output:
            args.transport_output.parent.mkdir(parents=True, exist_ok=True)
            args.transport_output.write_text(json.dumps({
                "schemaVersion": 1, "success": success, "error": error,
                "version": index["version"], "sourceCommit": index["sourceCommit"],
                "sourceTree": index["sourceTree"], "indexSha256": sha256(raw_index),
                "authenticationUsed": False, "measurements": cold,
                "definition": "Uninstrumented actual public entry, empty private caches; npx includes its isolated Node transport container startup. These are observations, not a warm latency baseline.",
                "harnessSha256": sha256(Path(__file__).read_bytes()),
            }, indent=2) + "\n")
        print(f"Public npx receipt: {args.output}", flush=True)


if __name__ == "__main__":
    main()
