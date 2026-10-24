"""Verify a public native project keeps its accepted engine until an explicit update."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import shutil
import stat


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def stable_version(value):
    require(re.fullmatch(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)", value),
            "Native upgrade proof requires exact stable versions")
    return tuple(int(part) for part in value.split("."))


def validate_versions(from_version, to_version):
    require((0, 3, 0) <= stable_version(from_version) < stable_version(to_version),
            "Native upgrade proof requires an older native environment")


def read_json(path):
    return json.loads(path.read_text())


def accepted(root, version, identity=None):
    state = read_json(root / ".devkit/manifest.json")
    require(state["version"] == version, "The project accepted an unexpected environment version")
    require(re.fullmatch(r"[a-f0-9]{64}", state["releaseIdentity"]), "Accepted release identity is invalid")
    if identity is not None:
        require(state["releaseIdentity"] == identity, "Accepted release identity changed unexpectedly")
    pins = dict(line.split("=", 1) for line in (root / ".devkit/runtime/toolchains.env").read_text().splitlines()
                if line and not line.startswith("#"))
    require(pins["DEVKIT_VERSION"] == version, "Managed toolchains adopted another version")
    require(re.fullmatch(r"[a-f0-9]{64}", pins["DEVKIT_ENGINE_SHA256"]), "Engine SHA256 pin is invalid")
    loader = (root / ".devkit/runtime/devkit.sh").read_text().splitlines()
    for line in ("mode=project", "version=" + version,
                 "expected=" + pins["DEVKIT_ENGINE_SHA256"], "target=" + pins["DEVKIT_ENGINE_TARGET"]):
        require(line in loader, "The project loader and accepted toolchain pins differ")
    for path, component in state["components"].items():
        if component["toolchain"] == "node":
            package = read_json(root / path / "package.json")
            require(package["devDependencies"]["@docouno/env"] == version
                    and component["toolDependencies"]["@docouno/env"] == version,
                    "A component adopted an environment package outside the accepted version")
    return state, pins


def verify_engine(helper, root, version, pins, label):
    require("DEVKIT_ENGINE_PATH" not in helper.environment, "The native smoke must use the project's pinned engine")
    engine = (Path(helper.environment["DEVKIT_CACHE_DIR"]) / "engines" /
              pins["DEVKIT_ENGINE_TARGET"] / version / pins["DEVKIT_ENGINE_SHA256"] / "devkit")
    require(engine.is_file() and not engine.is_symlink(), "The accepted cached engine is absent")
    require(hashlib.sha256(engine.read_bytes()).hexdigest() == pins["DEVKIT_ENGINE_SHA256"],
            "The accepted cached engine differs from its SHA256 pin")
    result = json.loads(helper.run(label + "-engine-version", [engine, "version", "--json"], cwd=root))
    require(result["ok"] is True and result["result"]["version"] == version,
            "The actual pin-selected engine has another compiled version")
    # Project loaders dispatch tasks, not metadata commands. A real doctor also
    # exercises the runtime's compiled-version-versus-accepted-version admission.
    doctor = helper.make(root, "doctor", label=label + "-doctor")
    require(f"Devkit {version};" in doctor, "Make did not run the accepted native environment")


def verify(helper, module, index, from_version, environment, products):
    """Run through public npm transports; all Make commands keep SDK blockers."""
    to_version = index["version"]
    validate_versions(from_version, to_version)
    root = products / "native-upgrade"
    transport_blockers = helper.scratch / "native-transport-blockers"
    transport_blockers.mkdir()
    for name in ("cargo", "rustc", "rustup", "tsc", "eslint"):
        shutil.copy2(helper.scratch / "without-host-sdks" / name, transport_blockers / name)
    transport_environment = dict(environment)
    transport_environment["PATH"] = str(transport_blockers) + ":" + environment["PATH"]
    transport_environment["DEVKIT_FORBIDDEN_HOST_SDK_LOG"] = str(helper.forbidden_log)
    old_cli = ["npx", "--yes", "@docouno/devkit@" + from_version, "--non-interactive"]
    new_cli = ["npx", "--yes", "@docouno/devkit@" + to_version, "--non-interactive"]
    # Include a partially initialized project in exact owned-resource cleanup.
    helper.projects.append(root)
    helper.run("native-upgrade-baseline-init", [*old_cli, "init", root.name,
               "--app", "web=react-web", "--app", "api=rust-api"],
               cwd=products, environment=transport_environment)
    state, old_pins = accepted(root, from_version)
    from_identity = state["releaseIdentity"]
    require(from_identity != index["releaseIdentity"], "Native upgrade needs distinct release identities")
    require(set(state["components"]) == {"apps/web", "apps/api"}, "Unexpected native baseline graph")

    for name in (".env", "Makefile", "compose.yaml"):
        path = root / name
        if name == ".env" and not path.exists():
            path.touch(mode=0o600)
        path.write_bytes(path.read_bytes() + b"\n# Retained native-upgrade owner configuration.\n")
    web_source = root / "apps/web/src/ClientApp.tsx"
    source = web_source.read_text()
    require("A small app, ready to change." in source, "Named native web baseline source changed")
    web_source.write_text(source.replace("A small app, ready to change.", "Native upgrade owner interface."))
    rust_source = root / "apps/api/src/lib.rs"
    source = rust_source.read_text()
    require("\n#[cfg(test)]" in source, "Named Rust baseline source changed")
    rust_source.write_text(source.replace("\n#[cfg(test)]", "\npub const NATIVE_UPGRADE_MARKER: u32 = 42;\n\n#[cfg(test)]", 1))
    cargo = root / "apps/api/Cargo.toml"
    cargo.write_text(cargo.read_text() + '\n[package.metadata.native-upgrade]\nowner = "retained"\n')
    web_package = root / "apps/web/package.json"
    package = read_json(web_package)
    package["description"] = "Retained native-upgrade product metadata"
    package["scripts"]["consumer:retained"] = "node --version"
    web_package.write_text(json.dumps(package, indent=2) + "\n")
    state["nativeUpgradeOwner"] = {"keep": [True, "opaque"]}
    for path, component in state["components"].items():
        component["nativeUpgradeOwner"] = {"component": path}
        component["origin"]["nativeUpgradeOwner"] = {"origin": "retained"}
    (root / ".devkit/manifest.json").write_text(json.dumps(state, indent=2) + "\n")
    product_files = {path: path.read_bytes() for path in (root / ".env", root / "Makefile", root / "compose.yaml", cargo, root / "apps/api/Cargo.lock")}
    for source_dir in (root / "apps/web/src", root / "apps/api/src", root / "apps/api/tests"):
        if source_dir.is_dir():
            product_files.update({path: path.read_bytes() for path in source_dir.rglob("*") if path.is_file()})
    product_package = {key: value for key, value in package.items() if key != "devDependencies"}
    old_product_tools = {key: value for key, value in package["devDependencies"].items()
                         if key not in state["components"]["apps/web"]["toolDependencies"]}
    product_modes = {path: stat.S_IMODE(path.stat().st_mode) for path in product_files}
    loader_before_add = (root / ".devkit/runtime/devkit.sh").read_bytes()

    def cli(label, *arguments):
        return helper.run(label, [*new_cli, *arguments], cwd=root, environment=transport_environment)

    cli("native-upgrade-fresh-cli-add", "add-package", ".", "--name", "client", "--template", "react-client")
    after_add, pins = accepted(root, from_version, from_identity)
    require(set(after_add["components"]) == {"apps/web", "apps/api", "packages/client"},
            "Fresh CLI added an unexpected component graph")
    require(pins == old_pins and (root / ".devkit/runtime/devkit.sh").read_bytes() == loader_before_add,
            "Fresh CLI changed accepted engine/toolchain pins while adding a package")
    helper.make(root, "setup", label="native-upgrade-old-setup")
    verify_engine(helper, root, from_version, pins, "native-upgrade-old")
    old_inputs = module.source_snapshot(root)
    helper.make(root, "check", label="native-upgrade-old-check")
    helper.make(root, "build", label="native-upgrade-old-build")
    require(module.source_snapshot(root) == old_inputs, "Old native checks/build changed source or locks")

    helper.run("native-upgrade-git-init", ["git", "init", "--quiet"], cwd=root)
    helper.run("native-upgrade-branch", ["git", "switch", "-c", "chore/devkit-" + to_version], cwd=root)
    before = module.source_snapshot(root)
    cli("native-upgrade-preview", "update", ".", "--to", to_version, "--preview")
    require(module.source_snapshot(root) == before, "Native upgrade preview changed project inputs")
    cli("native-upgrade-apply", "update", ".", "--to", to_version)
    after, new_pins = accepted(root, to_version, index["releaseIdentity"])
    require(new_pins["DEVKIT_ENGINE_SHA256"] == index["files"][index["engine"]]["sha256"],
            "Updated project accepted another target engine")
    require(after["nativeUpgradeOwner"] == state["nativeUpgradeOwner"], "Native upgrade lost top-level owner metadata")
    for path, component in state["components"].items():
        require(after["components"][path]["nativeUpgradeOwner"] == component["nativeUpgradeOwner"]
                and after["components"][path]["origin"]["nativeUpgradeOwner"] == component["origin"]["nativeUpgradeOwner"],
                "Native upgrade lost component/origin owner metadata")
    require(set(after["components"]) == set(after_add["components"]), "Native upgrade changed the product graph")
    for path, content in product_files.items():
        require(path.read_bytes() == content and stat.S_IMODE(path.stat().st_mode) == product_modes[path],
                "Native upgrade overwrote product input bytes/mode " + str(path.relative_to(root)))
    current = read_json(web_package)
    require({key: value for key, value in current.items() if key != "devDependencies"} == product_package,
            "Native upgrade changed product package fields/dependencies/scripts")
    old_tools = set(state["components"]["apps/web"]["toolDependencies"])
    new_tools = set(after["components"]["apps/web"]["toolDependencies"])
    require({key: value for key, value in current["devDependencies"].items()
             if key not in old_tools | new_tools or key in old_product_tools} == old_product_tools,
            "Native upgrade changed consumer-owned development dependencies")
    expected = module.source_snapshot(root)
    helper.make(root, "setup", label="native-upgrade-new-setup")
    verify_engine(helper, root, to_version, new_pins, "native-upgrade-new")
    for task in ("check", "test", "build"):
        helper.make(root, task, label="native-upgrade-new-" + task)
    require(module.source_snapshot(root) == expected, "Updated native setup/check/test/build changed source or locks")
    cli("native-upgrade-identical-noop", "update", ".", "--to", to_version)
    recovery = cli("native-upgrade-clean-recover", "recover", ".")
    require("No pending" in recovery or "no pending" in recovery, "Native clean recovery did not report a no-op")
    require(module.source_snapshot(root) == expected, "Identical native update/recover changed project inputs")
    require(not helper.forbidden_log.exists(), "Native upgrade used a host language SDK")
    return {"fromVersion": from_version, "toVersion": to_version,
            "fromReleaseIdentity": from_identity, "toReleaseIdentity": after["releaseIdentity"],
            "acceptedOldAfterFreshCliAdd": True, "oldEngineVersionVerified": True,
            "newEngineVersionVerified": True, "productInputsPreserved": True,
            "previewReadonly": True, "checksPassed": True, "sameVersionNoop": True,
            "hostSdkAttempts": 0}
