#!/usr/bin/env python3
"""Run the principal public README flow, using real npm and accepted SDKs."""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import time

REPO = Path(__file__).resolve().parents[2]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def bare_wizard(npx, environment, directory, log):
    master, slave = pty.openpty()
    interactive = dict(environment)
    interactive.pop("CI", None)
    process = subprocess.Popen(npx, cwd=directory, env=interactive, stdin=slave,
                               stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    output = bytearray()
    deadline = time.monotonic() + 180
    confirmed = False
    try:
        while b"Project directory" not in output:
            require(time.monotonic() < deadline and process.poll() is None,
                    "Actual public bare npx did not reach the project wizard")
            if select.select([master], [], [], 0.2)[0]:
                output.extend(os.read(master, 16384))
                require(len(output) < 1024 * 1024, "Wizard output exceeded its budget")
                if b"Ok to proceed?" in output and not confirmed:
                    os.write(master, b"y\n")
                    confirmed = True
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=15)
        require(process.returncode != 0, "Cancelling an incomplete wizard must fail")
        require(not (directory / "my-project").exists(), "Cancelled wizard wrote a product")
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=10)
        os.close(master)
        log.write_bytes(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-set", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    build_set = args.build_set.resolve()
    raw_index = (build_set / "release-index.json").read_bytes()
    index = json.loads(raw_index)
    require(index["publicationEligible"] is True, "Public README gate requires the eligible frozen set")
    readme = (REPO / "README.md").read_text()
    package = "@docouno/devkit@" + index["version"]
    for example in (f"npx {package} init my-product --app web=react-web",
                    f"npx {package} exec --component apps/web --write --network bridge -- npm install zod"):
        require(example in readme, "Principal README commands differ; update the named executable examples")
    spec = importlib.util.spec_from_file_location("readme_packaged_helper", REPO / "test/packaged/verify.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    helper = module.Verification(build_set / "payload", package_source="registry")
    products = helper.scratch / "readme"
    products.mkdir()
    home = helper.scratch / "anonymous-home"
    home.mkdir()
    environment = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ}
    environment.update(HOME=str(home), CI="true", npm_config_cache=str(home / "npm-cache"),
                       npm_config_userconfig=str(home / "user.npmrc"),
                       npm_config_globalconfig=str(home / "global.npmrc"),
                       npm_config_registry="https://registry.npmjs.org/",
                       npm_config_update_notifier="false", npm_config_audit="false",
                       npm_config_fund="false", DEVKIT_CACHE_DIR=str(home / "native-cache"))
    # Make/native commands retain the existing helper's host-SDK blockers.
    helper.environment = {key: helper.environment[key] for key in
                          ("PATH", "LANG", "LC_ALL", "DEVKIT_FORBIDDEN_HOST_SDK_LOG")
                          if key in helper.environment}
    helper.environment.update(HOME=str(home), CI="true", DEVKIT_CACHE_DIR=str(home / "native-cache"))
    npx = ["npx", package]
    root = products / "my-product"
    success = False
    error = None
    started = time.monotonic()
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def cli(label, *arguments, cwd=root):
        return helper.run(label, [*npx, *arguments], cwd=cwd, environment=environment)

    try:
        bare_wizard(npx, environment, products, helper.receipt / "bare-pty.log")
        cli("readme-init-web", "init", "my-product", "--app", "web=react-web", cwd=products)
        helper.projects.append(root)
        state = json.loads((root / ".devkit/manifest.json").read_text())
        require(state["version"] == index["version"] and set(state["components"]) == {"apps/web"}, "README web init accepted a different graph")
        before = module.source_snapshot(root)
        helper.make(root, "setup", label="readme-first-setup")
        helper.make(root, "check", label="readme-first-check")
        require(module.source_snapshot(root) == before, "Initial README setup/check changed product inputs")
        helper.make(root, "dev", "app=web", label="readme-first-dev")
        web = helper.service(root, "web")
        helper.http(helper.port(web, 5173), "/", '<div id="root">')
        helper.make(root, "down", label="readme-first-down")
        cli("readme-add-api", "add-app", ".", "--name", "api", "--template", "rust-api")
        cli("readme-add-client", "add-package", ".", "--name", "client", "--template", "react-client")
        cli("readme-add-desktop", "add-app", ".", "--name", "desktop", "--template", "electron-react", "--use", "client")
        helper.make(root, "setup", label="readme-added-components-setup")
        cli("readme-register-help", "register", "--help")
        cli("readme-native-dependency-edit", "exec", "--component", "apps/web", "--write", "--network", "bridge", "--", "npm", "install", "zod")
        web_package = json.loads((root / "apps/web/package.json").read_text())
        require("zod" in web_package["dependencies"], "README native dependency edit did not update the web package")
        before = module.source_snapshot(root)
        helper.make(root, "setup", label="readme-after-dependency-setup")
        helper.make(root, "check", label="readme-after-dependency-check")
        helper.make(root, "test", label="readme-test")
        helper.make(root, "build", label="readme-build")
        require(module.source_snapshot(root) == before, "README finite commands changed source/locks")
        helper.make(root, "fix", label="readme-explicit-fix")
        helper.make(root, "check", label="readme-after-fix-check")
        helper.make(root, "dev", "app=web", label="readme-web-hmr-dev")
        web = helper.service(root, "web")
        # Reuse the real Electron/DevTools driver. The explicit mode asserts
        # standalone UI has no API controls and still proves live React state.
        browser = helper.helper(root, "standalone-web", web, "A small app, ready to change.", "readme-web-hmr", hmr="README live HMR")
        helper.helper_ready(browser)
        source = root / "apps/web/src/ClientApp.tsx"
        original = source.read_text()
        require("A small app, ready to change." in original, "Named standalone HMR source fixture changed")
        with module.changed_files({source: original.replace("A small app, ready to change.", "README live HMR")}):
            helper.helper_finish(browser, "readme-web-hmr")
        helper.make(root, "status", label="readme-status")
        helper.make(root, "down", label="readme-down")
        preset = products / "fullstack-product"
        cli("readme-preset-init", "init", "fullstack-product", "--preset", "fullstack-desktop", cwd=products)
        helper.projects.append(preset)
        preset_state = json.loads((preset / ".devkit/manifest.json").read_text())
        require(set(preset_state["components"]) == {"apps/api", "apps/web", "apps/desktop", "packages/client"}, "README preset graph differs")
        helper.run("readme-product-git-init", ["git", "init", "--quiet"], cwd=root)
        helper.run("readme-update-branch", ["git", "switch", "-c", "chore/devkit-" + index["version"]], cwd=root)
        before = module.source_snapshot(root)
        cli("readme-update-preview", "update", ".", "--to", index["version"], "--preview")
        cli("readme-update-apply", "update", ".", "--to", index["version"])
        recovery = cli("readme-clean-recover", "recover", ".")
        require("No pending" in recovery or "no pending" in recovery, "Clean recovery did not report its no-pending state")
        helper.make(root, "setup", label="readme-after-update-setup")
        helper.make(root, "check", label="readme-after-update-check")
        require(module.source_snapshot(root) == before, "Same-version README update/recovery changed product inputs")
        require(not helper.forbidden_log.exists(), "README Make/native flow attempted a host language SDK")
        helper.run("readme-author-help", ["make", "help"], cwd=REPO)
        success = True
    except BaseException as failure:
        error = str(failure)
        raise
    finally:
        try:
            helper.cleanup()
        except BaseException as failure:
            success = False
            error = f"{error or ''}; cleanup: {failure}"
            raise
        finally:
            receipt = {"schemaVersion": 1, "success": success, "error": error,
                       "version": index["version"], "sourceCommit": index["sourceCommit"],
                       "sourceTree": index["sourceTree"], "indexSha256": digest(raw_index),
                       "authenticationUsed": False, "publicNpx": True, "barePtyCancelled": True,
                       "standaloneWebStatePreservingHmr": success,
                       "hostLanguageSDKAttemptedForNativeCommands": helper.forbidden_log.exists(),
                       "readmeSha256": digest(readme.encode()), "results": helper.results,
                       "seconds": time.monotonic() - started, "diagnostics": str(helper.receipt),
                       "limits": ["Clean recover is a no-op; actual interrupted recovery and packaged ELF/ASAR are proven by the separate full public matrices"]}
            args.output.write_text(json.dumps(receipt, indent=2) + "\n")
            print(f"Public README receipt: {args.output}", flush=True)


if __name__ == "__main__":
    main()
