#!/usr/bin/env python3
"""Exercise the unpublished three-target preview on a native Apple Silicon host."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile

TEST = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TEST))
sys.path.insert(0, str(TEST / "delivery"))
from bootstrap.support import Fixture, require  # noqa: E402
from test_delivery import Terminal, digest, file_map  # noqa: E402
from release_targets import (DARWIN_ARM64, LINUX_ARM64, LINUX_X64, executable_header_matches,
                             host_target, payload_name)  # noqa: E402

SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"


def run(argv, env, cwd, mask=0o022, success=True):
    result = subprocess.run(argv, cwd=cwd, env=env, umask=mask, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=90)
    require((result.returncode == 0) == success,
            f"{list(map(str, argv))}: {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def result_json(argv, env, cwd, mask=0o022):
    report = json.loads(run([*argv, "--json"], env, cwd, mask).stdout)
    require(report["ok"] is True, f"Unexpected Devkit result: {report}")
    return report["result"]


def verify_release(build_set, env, scratch):
    index = json.loads((build_set / "release-index.json").read_text())
    require(index["publicationEligible"] is False, "Darwin host verification requires an unpublished preview")
    require(set(index["engines"]) == {DARWIN_ARM64, LINUX_ARM64, LINUX_X64}, "Preview engine set differs")
    release = build_set / "payload"
    require(digest(release / "release.json") == index["releaseIdentity"], "Preview manifest digest differs")
    for name, identity in index["files"].items():
        path = build_set / name
        require(path.stat().st_size == identity["size"] and digest(path) == identity["sha256"],
                f"Frozen file differs: {name}")
    for target, asset in index["engines"].items():
        native = release / payload_name(target)
        with native.open("rb") as stream:
            require(executable_header_matches(stream.read(64), target), f"Unexpected executable format: {target}")
        require(digest(native) == index["files"][asset]["sha256"], f"Raw executable differs from payload: {target}")
    engine = release / payload_name(DARWIN_ARM64)
    run(["/usr/bin/codesign", "--verify", "--strict", engine], env, scratch)
    info = json.loads(run([engine, "internal", "build-info"], env, scratch).stdout)
    require(info["version"] == index["version"] and info["target"] == DARWIN_ARM64, "Native build-info differs")
    return index


def shell_entry(build_set, index, env, scratch, fixture):
    asset = index["engines"][DARWIN_ARM64]
    route = f'/docouno/devkit/releases/download/v{index["version"]}/{asset}'
    fixture.routes[route] = (build_set / asset).read_bytes()
    shell = ["/bin/sh", build_set / "install.sh"]
    with ThreadPoolExecutor(max_workers=3) as pool:
        versions = list(pool.map(lambda _: result_json([*shell, "--version"], env, scratch, 0o077), range(3)))
    require(all(result["target"] == DARWIN_ARM64 and result["version"] == index["version"] for result in versions),
            "Concurrent installers selected another engine")
    require(set(fixture.requests) == {route}, f"Cold acquisition requested unexpected assets: {fixture.requests}")
    requests = list(fixture.requests)
    project = scratch / "shell project"
    init = result_json([*shell, "init", project, "--app", "api=rust-api", "--name", "example",
                        "--release-root", build_set / "payload", "--offline", "--package-source", "local"], env, scratch)
    require(init["releaseIdentity"] == index["releaseIdentity"], "Shell init selected another release")
    require("Registered apps: api" in run(["/usr/bin/make", "help"], env, project).stdout, "Missing registered Rust app")
    info = json.loads(run(["/usr/bin/make", "--no-print-directory", "info", "json=1"], env, project).stdout)
    require(info["ok"] and info["result"]["hostTarget"] == DARWIN_ARM64, "Apple Make did not dispatch to Darwin")
    require(fixture.requests == requests, "Warm shell, Make or offline init requested a network asset")


def npm_entry(build_set, index, env, scratch, mask, node_bin, fixture):
    output = scratch / f"npm {mask:03o}"
    output.mkdir()
    env = dict(env, PATH=str(node_bin) + os.pathsep + SYSTEM_PATH,
               DEVKIT_CACHE_DIR=str(output / "native cache"), npm_config_cache=str(output / "npm cache"),
               npm_config_userconfig="/dev/null", npm_config_globalconfig=str(output / "global.npmrc"),
               npm_config_audit="false", npm_config_fund="false", npm_config_update_notifier="false")
    requests = list(fixture.requests)
    archive = build_set / index["packages"]["cli"]["archive"]
    argv = [node_bin / "npx", "--yes", "--offline", "--ignore-scripts", "--package", archive, "--", "devkit"]
    version = result_json([*argv, "--version"], env, output, mask)
    require(version["target"] == DARWIN_ARM64 and version["version"] == index["version"], "npm selected another engine")
    matches = list(Path(env["npm_config_cache"]).glob("_npx/*/node_modules/@docouno/devkit"))
    require(len(matches) == 1, "Expected one npm-installed Devkit payload")
    installed = matches[0]
    before = file_map(installed)
    if mask == 0o077:
        require(any(item["mode"] == 0o600 for item in before.values()), "npm ignored the restrictive umask")
    init = result_json([*argv, "init", output / "example", "--name", "example", "--app", "api=rust-api",
                        "--package-source", "local", "--offline"], env, output, mask)
    require(init["releaseIdentity"] == index["releaseIdentity"], "npm sibling import selected another release")
    canonical = Path(env["DEVKIT_CACHE_DIR"]) / "releases" / index["version"] / index["releaseIdentity"]
    require(file_map(canonical) == file_map(build_set / "payload"), "npm payload normalization changed its identity or mode")
    require(file_map(installed) == before, "Devkit modified npm's installation")
    require(requests == fixture.requests, "Offline npm import attempted acquisition")
    # A second symlink hop with a space exercises the launcher npm actually installed.
    launcher = output / "devkit cli"
    launcher.symlink_to(os.path.relpath(installed.parent.parent / ".bin/devkit", output))
    with Terminal([launcher, "init"], cwd=output, env=dict(env, CI="")) as terminal:
        terminal.until("Project directory")
        terminal.child.send_signal(signal.SIGTERM)
        status, transcript = terminal.wait(timeout=8)
        require(status == 143, f"Direct .bin launcher lost SIGTERM: {status}, {transcript}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-set", type=Path, required=True)
    build_set = parser.parse_args().build_set.resolve()
    require(host_target() == DARWIN_ARM64, "This gate runs on native Apple Silicon")
    # Keep /tmp's public spelling through all cache and checkout operations.
    with tempfile.TemporaryDirectory(dir="/tmp", prefix="devkit host preview ") as directory:
        scratch = Path(directory)
        env = {"PATH": SYSTEM_PATH, "HOME": str(scratch / "home"), "TMPDIR": directory, "LC_ALL": "C", "CI": "true",
               "DEVKIT_CACHE_DIR": str(scratch / "shell cache")}
        Path(env["HOME"]).mkdir()
        make_version = run(["/usr/bin/make", "--version"], env, scratch).stdout.splitlines()[0]
        require("GNU Make 3.81" in make_version, f"Expected the stock Apple Make: {make_version}")
        index = verify_release(build_set, env, scratch)
        node_bin = scratch / "node bin"
        node_bin.mkdir()
        for tool in ("node", "npx"):
            source = shutil.which(tool)
            require(source is not None, f"Missing npm entry dependency: {tool}")
            (node_bin / tool).symlink_to(Path(source).resolve())
        with closing(Fixture(scratch / "tls")) as fixture:
            proxy = f"http://127.0.0.1:{fixture.server.server_address[1]}"
            ca = str(fixture.cert.root / "ca.pem")
            env.update(HTTPS_PROXY=proxy, HTTP_PROXY=proxy, SSL_CERT_FILE=ca, CURL_CA_BUNDLE=ca)
            shell_entry(build_set, index, env, scratch, fixture)
            for mask in (0o022, 0o077):
                npm_entry(build_set, index, env, scratch, mask, node_bin, fixture)
        print(json.dumps({"gate": "Darwin host-only", "macOS": run(["/usr/bin/sw_vers", "-productVersion"], env, scratch).stdout.strip(),
                          "make": make_version, "version": index["version"], "releaseIdentity": index["releaseIdentity"],
                          "indexSha256": digest(build_set / "release-index.json"), "npmUmasks": ["022", "077"]}))


if __name__ == "__main__":
    main()
