#!/usr/bin/env python3
"""Run the Apple Silicon Docker acceptance gate on an unpublished three-target set."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

from verify import Verification, require
from release_targets import DARWIN_ARM64, LINUX_ARM64, LINUX_X64, executable_header_matches, host_target, payload_engine


def check_vm(test, context, daemon, bundle):
    require(test.run("vm-worktrunk-version", ["wt", "--version"]).split()[1] == "0.74.0",
            "Use Worktrunk 0.74.0 for the generated hook proof")
    test.run("vm-compose-version", ["docker", "compose", "version"])
    test.run("vm-system-make", ["make", "--version"])
    project = test.create("fullstack", "vm-desktop")
    # Force the store selector to exercise its bounded-common-ancestor fallback.
    # These siblings all belong to the disposable harness.
    for n in range(257):
        (test.scratch / f"sibling-{n}").mkdir()
    (project / ".npmrc").unlink(missing_ok=True)
    npmrc = project / ".NPMRC"
    npmrc.write_text("@vm-proof:registry=https://registry.npmjs.org/\n")
    if not (project / ".npmrc").exists():
        (project / ".npmrc").write_bytes(npmrc.read_bytes())
    ca = test.scratch / "fixture-ca.pem"
    test.run("vm-fixture-ca", ["/usr/bin/openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-subj", "/CN=VM Acceptance Fixture", "-keyout", test.scratch / "fixture.key", "-out", ca])
    test.environment["DEVKIT_CA_CERT"] = str(ca)
    probe = ("const f=require('node:fs'),a=require('node:assert/strict');"
             "for(const n of ['.npmrc','.NPMRC'])a.equal(f.readFileSync('/workspace/'+n,'utf8'),'');"
             "a.match(f.readFileSync('/devkit/user.npmrc','utf8'),/@vm-proof:registry=/);"
             "a.match(f.readFileSync('/devkit/ca.pem','utf8'),/BEGIN CERTIFICATE/)")
    test.run("vm-userconfig-ca-and-masks", [payload_engine(test.manager), "exec", "--root", project,
             "--component", "apps/web", "--network", "bridge", "--prepare", "--", "node", "-e", probe])
    lower = project / ".npmrc"
    accepted = lower.read_bytes()
    try:
        lower.write_text("registry=http://127.0.0.1:8080/\n")
        test.make(project, "setup", expected="macOS host loopback", label="vm-loopback-registry-refusal")
    finally:
        lower.write_bytes(accepted)
    (project / ".env").write_text("VM_ACCEPTANCE=yes\n")
    (project / "compose.yaml").write_text(
        'services:\n  web:\n    environment:\n      VM_ACCEPTANCE: "${VM_ACCEPTANCE:?}"\n')
    test.healthy(project, "vm-desktop")
    store = project / ".devkit/cache/pnpm-store"
    local = project / "apps/desktop/node_modules/electron/dist/icudtl.dat"
    candidates = list(store.glob("electron-dist/*/dist/icudtl.dat"))
    require(any(p.samefile(local) for p in candidates), "Checkout-local fallback did not hardlink Electron")
    doctor = json.loads(test.run("vm-doctor", [payload_engine(test.manager), "doctor", project, "--json"]))
    checks = {c["id"]: c["details"] for c in doctor["result"]["checks"]}
    docker = checks["docker_compose"]
    require(docker["hostTarget"] == DARWIN_ARM64 and docker["daemonPlatform"] == "linux/" + test.daemon_arch
            and docker["dockerContext"] == context, "Doctor selected another VM")
    budget = checks["resource_budget"]["budget"]
    require(0 < budget["effectiveMillicores"] <= daemon["NCPU"] * 1000
            and 0 < budget["effectiveMemBytes"] <= daemon["MemTotal"], "Budget exceeds VM resources")
    require(checks["checkout_state"]["helperReady"] and
            checks["checkout_state"]["preparedPlatform"] == docker["daemonPlatform"], "VM receipts disagree")
    test.make(project, "package", "app=desktop", label="vm-electron-package")
    with tarfile.open(project / "apps/desktop/release/app-linux.tar.gz") as archive:
        executable = next(item for item in archive if item.isfile() and Path(item.name) == Path("app"))
        require(executable_header_matches(archive.extractfile(executable).read(64), test.linux_target),
                "Packaged Electron executable differs from daemon architecture")
    try:
        test.make(project, "up", "headless=1", label="vm-headless-up")
        for name in ("desktop", "web", "api"):
            require(test.inspect("container", test.service(project, name))["State"]["Running"],
                    f"VM stand service {name} did not start")
        web = test.inspect("container", test.service(project, "web"))
        require("VM_ACCEPTANCE=yes" in web["Config"]["Env"], "Compose lost project-owned .env")
        test.make(project, "status", label="vm-stand-status")
    finally:
        test.make(project, "down", label="vm-stand-down")
    imported = test.cross_host_checkout(imported=bundle)
    test.run("vm-worktree-create", ["wt", "switch", "--create", "--no-cd", "--yes", "vm-proof"], cwd=imported)
    linked = imported / ".worktrees/vm-proof"
    try:
        info = json.loads(test.make(linked, "info", "json=1", label="vm-worktree-info"))["result"]
        require(info["worktreeKind"] == "linked" and info["standPorts"], "Worktree hook did not reserve ports")
    finally:
        test.run("vm-worktree-remove", ["wt", "remove", "--foreground", "--yes", "vm-proof"], cwd=imported)
    commit = test.run("vm-imported-commit", ["git", "rev-parse", "HEAD"], cwd=imported).strip()
    return {"hostTarget": DARWIN_ARM64, "daemonPlatform": docker["daemonPlatform"], "dockerContext": context,
            "budget": budget, "store": str(store), "crossHostCommit": commit}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-set", type=Path, required=True)
    parser.add_argument("--expected-index-sha256", required=True)
    parser.add_argument("--checkout-bundle", type=Path, required=True)
    parser.add_argument("--docker-context", required=True)
    args = parser.parse_args()
    require(host_target() == DARWIN_ARM64, "Run on Apple Silicon")
    require(args.docker_context != "default" and not os.environ.get("DOCKER_HOST"),
            "Use a named local Docker context without a DOCKER_HOST override")
    raw = (args.build_set / "release-index.json").read_bytes()
    require(hashlib.sha256(raw).hexdigest() == args.expected_index_sha256, "Frozen index differs")
    index = json.loads(raw)
    require(set(index["engines"]) == {LINUX_X64, LINUX_ARM64, DARWIN_ARM64}, "Expected all three targets")
    release = args.build_set / "payload"
    require(hashlib.sha256((release / "release.json").read_bytes()).hexdigest() == index["releaseIdentity"],
            "Staged payload differs from the frozen identity")
    endpoint = subprocess.check_output(["docker", "context", "inspect", "--format", "{{.Endpoints.docker.Host}}",
                                        args.docker_context], text=True, timeout=30).strip()
    require(endpoint.startswith("unix:///"), "Docker context must refer to a local Linux VM Unix socket")
    daemon = json.loads(subprocess.check_output(["docker", "--context", args.docker_context, "info",
                                                 "--format", "{{json .}}"], text=True, timeout=30))
    require(daemon["OSType"] == "linux", "Expected a Linux Docker VM")
    arch = {"amd64": "amd64", "x86_64": "amd64", "arm64": "arm64", "aarch64": "arm64"}[daemon["Architecture"]]
    helper = Verification(release, daemon_arch=arch, scratch_parent=Path.home())
    (helper.scratch / "without-host-sdks/make").symlink_to("/usr/bin/make")
    home = helper.scratch / "test home"
    home.mkdir()
    helper.environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                              DOCKER_CONFIG=os.environ.get("DOCKER_CONFIG", str(Path.home() / ".docker")),
                              DOCKER_CONTEXT=args.docker_context, WORKTRUNK_WORKTREE_PATH="{{ repo_path }}/.worktrees/{{ branch | sanitize }}")
    evidence = {"passed": False, "indexSha256": args.expected_index_sha256, "releaseIdentity": index["releaseIdentity"],
                "macOS": subprocess.check_output(["sw_vers", "-productVersion"], text=True).strip(),
                "dockerBackend": daemon.get("OperatingSystem"), "dockerVersion": daemon.get("ServerVersion")}
    try:
        evidence.update(check_vm(helper, args.docker_context, daemon, args.checkout_bundle.resolve(strict=True)))
        require(not helper.forbidden_log.exists(), "Attempted to use a host language SDK")
        evidence["passed"] = True
    finally:
        try:
            helper.cleanup()
        except BaseException:
            evidence["passed"] = False
            raise
        finally:
            evidence["results"] = helper.results
            (helper.receipt / "darwin-vm.json").write_text(json.dumps(evidence, indent=2) + "\n")
            print("Apple Silicon VM evidence:", helper.receipt / "darwin-vm.json", flush=True)
            if evidence["passed"]:
                shutil.rmtree(helper.scratch)


if __name__ == "__main__":
    main()
