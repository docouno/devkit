use devkit::{
    distribution::Target,
    model::ProjectState,
    projections::render_app_compose_value,
    runtime::{EnginePins, ToolPins, bind_mount, development_components, validate_compose_version},
};
use serde_json::json;
use std::path::Path;

fn fixture_platform() -> &'static str {
    match Target::host().unwrap() {
        Target::LinuxX64 => "linux/amd64",
        Target::LinuxArm64 | Target::DarwinArm64 => "linux/arm64",
    }
}

// Mock digests refer to the native fixture executable. Every target key still
// goes through the same parser and selection as a real release.
fn fixture_engine_pins(sha: &str) -> String {
    let mut lines = format!(
        "DEVKIT_ENGINE_TARGET={}\nDEVKIT_ENGINE_SHA256={sha}\n",
        Target::LinuxX64.triple()
    );
    let targets = Target::ALL;
    lines.push_str(&format!(
        "DEVKIT_ENGINE_TARGETS={}\n",
        targets.map(|target| target.triple()).join(",")
    ));
    for target in targets {
        lines.push_str(&format!(
            "DEVKIT_ENGINE_SHA256_{}={sha}\n",
            target.key_suffix()
        ));
    }
    lines
}

fn fixture_pnpm_pins(pin: &devkit::runtime::PnpmPin) -> String {
    let mut lines = pin.encode();
    lines.push_str("DEVKIT_PNPM_TARGETS=x86_64-unknown-linux-musl,aarch64-unknown-linux-musl\n");
    for (target, arch) in [(Target::LinuxX64, "x64"), (Target::LinuxArm64, "arm64")] {
        let suffix = target.key_suffix();
        lines.push_str(&format!(
            "DEVKIT_PNPM_PACKAGE_{suffix}=@pnpm/exe.linux-{arch}-musl\nDEVKIT_PNPM_SHA256_{suffix}={}\nDEVKIT_PNPM_TARBALL_SHA256_{suffix}={}\n",
            pin.binary_sha256, pin.tarball_sha256
        ));
    }
    lines
}

fn write_runtime_stub(root: &Path, path: &str) {
    let contents = if path.ends_with("/LICENSE") {
        include_bytes!("../../../LICENSE").to_vec()
    } else {
        b"{}\n".to_vec()
    };
    devkit::files::write_atomic(&root.join(path), &contents, 0o644).unwrap();
}

fn modernize_node_fixture(root: &Path, state: &ProjectState) {
    if !root.join("package-lock.json").is_file() {
        return;
    }
    let packages = devkit::native::package_lock(root).unwrap();
    devkit::project::sync_node_workspace(root, state, None).unwrap();
    devkit::pnpm::import_legacy_npm_lock_seed(root, &packages)
        .unwrap()
        .write(root)
        .unwrap();
    std::fs::remove_file(root.join("package-lock.json")).unwrap();
}
#[test]
fn compose_requires_fixed_stable_env_resolution_and_accepts_desktop_packaging() {
    for version in [
        "5.0.0",
        "v5.0.0\n",
        "5.6.0",
        "v5.6.0-desktop.1",
        "6.0.0+build.1",
    ] {
        validate_compose_version(version).unwrap();
    }
    for version in [
        "2.38.2",
        "v2.40.3",
        "5.0.0-rc.2",
        "5.6.0-beta.1",
        "5.0",
        "05.0.0",
        "5.0.0-desktop.01",
        "5.0.0 trailing",
        "5.0.0\n2.38.2",
        "snapshot-regression",
        "",
        "18446744073709551616.0.0",
    ] {
        assert!(
            validate_compose_version(version)
                .unwrap_err()
                .to_string()
                .contains("Compose >=5.0.0"),
            "{version}"
        );
    }
}
fn state(version: &str) -> ProjectState {
    serde_json::from_value(json!({"schemaVersion":2,"version":version,"releaseIdentity":"0".repeat(64),"generation":"00000000-0000-4000-8000-000000000000","packageMode":"registry","managed":{},"components":{
"apps/web":{"role":"app","toolchain":"node","toolingProfile":"web","origin":{"template":"react-web","version":version},"toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"compose","service":"web"}},
"apps/api":{"role":"app","toolchain":"rust","toolingProfile":"rust","origin":{"template":"rust-api","version":version},"toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"compose","service":"api"}},
"apps/desktop":{"role":"app","toolchain":"node","toolingProfile":"electron","origin":{"template":"electron-react","version":version},"toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"desktop","service":"desktop"}}}})).unwrap()
}
#[test]
fn compose_selected_profiles_and_optional_dependencies_match_native_reachability() {
    let state = state("0.5.0");
    let model = json!({"services":{"web":{"profiles":["client"],"depends_on":{"api":{"required":true},"extra":{"required":false}}},"api":{},"extra":{"profiles":["unused"]},"desktop":{}}});
    assert_eq!(
        development_components(&state, &model, "web").unwrap(),
        vec!["apps/api", "apps/web"]
    );
    let inactive = json!({"services":{"web":{"depends_on":{"api":{"required":true}}},"api":{"profiles":["unused"]}}});
    assert!(
        development_components(&state, &inactive, "web")
            .unwrap_err()
            .to_string()
            .contains("not active")
    );
}
#[test]
fn cargo_projection_uses_one_setup_and_shared_target_for_registered_apps_and_libraries() {
    use devkit::{
        native::{CargoMember, CargoPackageId, CargoWorkspaceModel},
        projections::render_project_make,
    };
    let mut state = state("0.6.0");
    let mut library = state.components["apps/api"].clone();
    library.role = devkit::model::Role::Library;
    library.development = None;
    state.components.insert("packages/core".into(), library);
    let make = render_project_make(&state);
    assert!(make.contains("setup-api: setup-rust-workspace\n"));
    assert!(make.contains("setup/packages/core: setup-rust-workspace\n"));
    assert!(make.contains("setup-web: setup-node-workspace\n"));

    let member = CargoMember {
        rel_dir: "apps/api/crates/$server".into(),
        id: CargoPackageId {
            name: "api-server".into(),
            version: "0.1.0".into(),
            source: None,
        },
        dependencies: vec![],
    };
    let mut workspace = CargoWorkspaceModel::default();
    workspace
        .components
        .insert("apps/api".into(), vec![member.clone()]);
    let rendered = render_app_compose_value(&state, Some(&workspace)).unwrap();
    let service = &rendered["services"]["api"];
    assert_eq!(
        service["environment"]["CARGO_TARGET_DIR"],
        "/workspace/target"
    );
    assert_eq!(service["working_dir"], "/workspace/apps/api");
    assert!(
        service["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["target"] == "/workspace/target"
                && v["source"] == "${DEVKIT_PROJECT_ROOT:?}/target"
                && v["read_only"] == false
                && v["bind"]["create_host_path"] == false)
    );
    let command = json!([
        "cargo",
        "run",
        "--manifest-path",
        "/workspace/Cargo.toml",
        "--locked",
        "--offline"
    ]);
    let mut selected = command.clone();
    assert_eq!(
        member.pkgid().unwrap(),
        "path+file:///workspace/apps/api/crates/$server#api-server@0.1.0"
    );
    selected.as_array_mut().unwrap().extend([
        json!("-p"),
        json!("path+file:///workspace/apps/api/crates/$$server#api-server@0.1.0"),
    ]);
    assert_eq!(service["command"], selected);

    let mut second = member.clone();
    second.id.name = "api-admin".into();
    second.rel_dir = "apps/api/admin".into();
    workspace
        .components
        .get_mut("apps/api")
        .unwrap()
        .push(second);
    assert_eq!(
        render_app_compose_value(&state, Some(&workspace)).unwrap()["services"]["api"]["command"],
        command
    );
    state.version = "0.5.1".into();
    assert!(!render_project_make(&state).contains("setup-rust-workspace"));
    let old = render_app_compose_value(&state, None).unwrap();
    assert_eq!(
        old["services"]["api"]["environment"]["CARGO_TARGET_DIR"],
        "/workspace/apps/api/target"
    );
    assert_eq!(
        old["services"]["api"]["command"],
        json!(["cargo", "run", "--locked", "--offline"])
    );
}
#[test]
fn desktop_projection_always_mounts_the_accepted_linux_helper_read_only() {
    for ver in ["0.4.1", "0.5.0", "0.6.0"] {
        let rendered = render_app_compose_value(&state(ver), None).unwrap();
        assert_eq!(
            rendered["services"]["desktop"]["command"],
            json!(["/devkit/devkit", "internal", "desktop"])
        );
        assert!(
            rendered["services"]["desktop"]["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["target"] == "/devkit/devkit"
                    && v["read_only"] == true
                    && v["source"] == "${DEVKIT_ENGINE_PATH:?}")
        );
    }
}
fn toolchains(ver: &str) -> String {
    let engine_pins = fixture_engine_pins(&"d".repeat(64));
    format!(
        "DEVKIT_VERSION={ver}\nDEVKIT_NODE_IMAGE=node:24@sha256:{}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={}\nDEVKIT_DESKTOP_RECIPE_HASH={}\n{engine_pins}",
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64)
    )
}

#[test]
fn dispatch_admits_engine_identity_and_checks_every_record_before_delegating() {
    let source = toolchains("0.5.0");
    let engine_only = source
        .lines()
        .filter(|line| line.starts_with("DEVKIT_ENGINE_") || line.starts_with("DEVKIT_VERSION="))
        .collect::<Vec<_>>()
        .join("\n");
    let pins = EnginePins::parse(engine_only.as_bytes()).unwrap();
    assert_eq!(
        pins.for_target(Target::host().unwrap()).unwrap().version,
        "0.5.0"
    );
    assert!(
        EnginePins::parse(
            format!("{engine_only}\nDEVKIT_FUTURE_PIN=1\nDEVKIT_DESKTOP_RECIPE_HASH=future-pin\n")
                .as_bytes()
        )
        .is_ok()
    );
    for invalid in [
        "UNTRUSTED_PIN=1",
        "DEVKIT_FUTURE_PIN=1\nDEVKIT_FUTURE_PIN=2",
        "DEVKIT_FUTURE_PIN=a b",
        "DEVKIT_FUTURE_PIN='quoted'",
        "DEVKIT_FUTURE_PIN=a\0b",
        "DEVKIT_VERSION=0.5.0",
    ] {
        assert!(
            EnginePins::parse(format!("{engine_only}\n{invalid}").as_bytes()).is_err(),
            "{invalid}"
        );
    }
    let without_engine = source
        .lines()
        .filter(|line| !line.starts_with("DEVKIT_ENGINE_"))
        .collect::<Vec<_>>()
        .join("\n");
    for ver in ["0.2.1", "0.4.1", "0.5.0", "0.6.0"] {
        assert!(
            EnginePins::parse(
                without_engine
                    .replace("DEVKIT_VERSION=0.5.0", &format!("DEVKIT_VERSION={ver}"))
                    .as_bytes()
            )
            .is_err()
        );
    }
    assert!(
        EnginePins::parse(
            source
                .replace(
                    "DEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl",
                    "DEVKIT_ENGINE_TARGET=aarch64-unknown-linux-musl",
                )
                .as_bytes()
        )
        .is_err()
    );
}

#[test]
fn release_admits_required_immutable_tool_pins_and_allows_future_keys() {
    let source = toolchains("0.6.0");
    let pins = ToolPins::parse_release(source.as_bytes()).unwrap();
    assert_eq!(pins.host_engine().unwrap().version, "0.6.0");
    assert_eq!(
        pins.desktop_recipe.as_deref(),
        Some("c".repeat(64).as_str())
    );
    assert!(ToolPins::parse_release(format!("{source}DEVKIT_FUTURE_PIN=1\n").as_bytes()).is_ok());
    assert!(ToolPins::parse(format!("{source}UNTRUSTED_PIN=1\n").as_bytes()).is_err());
    for invalid in [
        source.replace(&"c".repeat(64), "bad"),
        source.replace("node:24@sha256:", "$(touch/tmp/NO)@sha256:"),
        source.replace(&format!("@sha256:{}", "a".repeat(64)), ""),
        source.replace("DEVKIT_DESKTOP_RECIPE_HASH=", "DEVKIT_OTHER="),
    ] {
        assert!(ToolPins::parse_release(invalid.as_bytes()).is_err());
    }
}

#[test]
fn checkout_admits_only_the_desktop_pin_for_its_accepted_profile_and_version() {
    use devkit::{
        files::{identity, write_atomic},
        model::FileIdentity,
    };

    let checkout = tempfile::tempdir().unwrap();
    let root = checkout.path();
    let path = ".devkit/runtime/toolchains.env";
    for (ver, electron, with_pin, admitted) in [
        ("0.5.1", false, true, true),
        ("0.5.1", false, false, false),
        ("0.6.0", false, true, false),
        ("0.6.0", false, false, true),
        ("0.6.0-rc.1", false, false, true),
        ("0.6.0", true, true, true),
        ("0.6.0", true, false, false),
    ] {
        let mut state = state(ver);
        if electron {
            state.managed.insert(
                ".devkit/runtime/desktop.Dockerfile".into(),
                FileIdentity {
                    sha256: "c".repeat(64),
                    mode: 0o644,
                },
            );
        } else {
            state.components.remove("apps/desktop");
        }
        let mut pins = toolchains(ver);
        if !with_pin {
            pins = pins
                .lines()
                .filter(|line| !line.starts_with("DEVKIT_DESKTOP_RECIPE_HASH="))
                .collect::<Vec<_>>()
                .join("\n");
        }
        write_atomic(&root.join(path), pins.as_bytes(), 0o644).unwrap();
        state
            .managed
            .insert(path.into(), identity(&root.join(path)).unwrap().unwrap());
        assert_eq!(
            ToolPins::admit(root, &state).is_ok(),
            admitted,
            "{ver}, electron={electron}, with_pin={with_pin}"
        );
        if admitted && electron {
            state
                .managed
                .get_mut(".devkit/runtime/desktop.Dockerfile")
                .unwrap()
                .sha256 = "a".repeat(64);
            assert!(ToolPins::admit(root, &state).is_err());
        }
        write_atomic(
            &root.join(path),
            format!("{pins}\n# drift\n").as_bytes(),
            0o644,
        )
        .unwrap();
        assert!(ToolPins::admit(root, &state).is_err());
    }
}
#[test]
fn mount_paths_are_csv_quoted_without_shell_interpretation() {
    let mount = bind_mount(
        Path::new("/tmp/a,b \"x\" $(no)"),
        Path::new("/workspace"),
        true,
    )
    .unwrap();
    assert_eq!(
        mount,
        "type=bind,\"source=/tmp/a,b \"\"x\"\" $(no)\",\"target=/workspace\",readonly"
    );
    assert!(bind_mount(Path::new("/tmp/a\nb"), Path::new("/workspace"), true).is_err());
}
#[test]
fn node_snapshot_is_projected_before_cargo_but_full_recheck_retains_namespace_barrier() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use devkit::{files::write_json, native::admit_native_components};
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    let state = state("0.5.0");
    let root_manifest = json!({"name":"fixture","version":"1.0.0","private":true,"workspaces":["apps/desktop","apps/web"]});
    write_json(&root.join("package.json"), &root_manifest).unwrap();
    let mut lock = json!({"lockfileVersion":3,"packages":{"":root_manifest}});
    for path in ["apps/desktop", "apps/web"] {
        let name = path.rsplit('/').next().unwrap();
        let deps = if name == "web" {
            json!({"archive":"file:../../vendor/probe.tgz"})
        } else {
            json!({})
        };
        let manifest = json!({"name":name,"version":"1.0.0","dependencies":deps,"scripts":{"check":"true","fix":"true","test":"true","build":"true","dev":"true"}});
        write_json(&root.join(path).join("package.json"), &manifest).unwrap();
        lock["packages"][path] = manifest;
        lock["packages"][format!("node_modules/{name}")] = json!({"resolved":path,"link":true});
    }
    lock["packages"]["node_modules/archive"] = json!({"version":"1.0.0","resolved":"file:vendor/probe.tgz","integrity":format!("sha512-{}",STANDARD.encode([1;64]))});
    write_json(&root.join("package-lock.json"), &lock).unwrap();
    std::fs::create_dir_all(root.join("vendor")).unwrap();
    std::fs::write(root.join("vendor/probe.tgz"), "archive one").unwrap();
    std::fs::create_dir_all(root.join("apps/api")).unwrap();
    for (name, contents) in [
        (
            "Cargo.toml",
            "[package]\nname=\"api\"\nversion=\"1.0.0\"\nedition=\"2024\"\n",
        ),
        (
            "Cargo.lock",
            "version = 4\n\n[[package]]\nname = \"api\"\nversion = \"1.0.0\"\n",
        ),
        ("rust-toolchain.toml", "[toolchain]\nchannel=\"1.99.1\"\n"),
        ("rustfmt.toml", "edition=\"2024\"\n"),
    ] {
        std::fs::write(root.join("apps/api").join(name), contents).unwrap();
    }
    write_json(&root.join(".devkit/tooling-profiles.json"),&json!({"schemaVersion":1,"version":"0.5.0","packageMode":"registry","profiles":{"web":{},"electron":{},"rust":{}}})).unwrap();
    modernize_node_fixture(root, &state);
    let first = admit_native_components(root, &state).unwrap();
    std::fs::write(
        root.join("apps/api/rustfmt.toml"),
        "edition=\"2024\"\nmax_width=90\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("apps/api/.cargo")).unwrap();
    std::fs::write(
        root.join("apps/api/.cargo/config.toml"),
        "[build]\njobs=1\n",
    )
    .unwrap();
    let rust_changed = admit_native_components(root, &state).unwrap();
    assert_eq!(first.node_inputs, rust_changed.node_inputs);
    assert_ne!(first.inputs, rust_changed.inputs);
    assert!(first.recheck(root).is_err());
    std::fs::write(root.join(".npmrc"), "fund=false\n").unwrap();
    let npm_changed = admit_native_components(root, &state).unwrap();
    assert_ne!(rust_changed.node_inputs, npm_changed.node_inputs);
    std::fs::write(root.join("vendor/probe.tgz"), "archive two").unwrap();
    let archive_changed = admit_native_components(root, &state).unwrap();
    assert_ne!(npm_changed.node_inputs, archive_changed.node_inputs);
    std::fs::write(root.join("apps/api/package.json"), "{}").unwrap();
    assert!(archive_changed.recheck(root).is_err());
}

#[test]
fn reactive_retry_heals_remote_dind_cgroupv2_failure_by_stripping_limits_and_enabling_node_options()
{
    use devkit::{
        delivery::file_digest,
        files::{identity, write_atomic, write_json},
        state::{CHECKSUM_PATH, MANIFEST_PATH},
    };
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, process::Command};

    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir_all(root.join(".devkit/runtime")).unwrap();
    fs::create_dir_all(root.join("apps/web")).unwrap();

    let exe = Path::new(env!("CARGO_BIN_EXE_devkit"));
    let engine_sha = file_digest(exe).unwrap();
    let ver = env!("CARGO_PKG_VERSION");
    let recipe_hash = "b".repeat(64);
    let node_image = "node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205";

    let engine_pins = fixture_engine_pins(&engine_sha);
    let toolchains = format!(
        "DEVKIT_VERSION={ver}\n{engine_pins}DEVKIT_NODE_IMAGE={node_image}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(
        &root.join(".devkit/runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    let pnpm_pin = devkit::runtime::PnpmPin {
        package: "@pnpm/exe.linux-x64-musl".into(),
        version: "12.21.0".into(),
        tarball_sha256: "a".repeat(64),
        binary_sha256: engine_sha.clone(),
    };
    write_atomic(
        &root.join(".devkit/runtime/pnpm.env"),
        fixture_pnpm_pins(&pnpm_pin).as_bytes(),
        0o644,
    )
    .unwrap();
    for stub in [
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/tooling-profiles.json",
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/LICENSE",
    ] {
        write_runtime_stub(&root, stub);
    }
    write_json(
        &root.join("apps/web/package.json"),
        &json!({"name": "web", "version": "1.0.0", "private": true}),
    )
    .unwrap();

    let mut managed = BTreeMap::new();
    for path in [
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/tooling-profiles.json",
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/LICENSE",
        ".devkit/runtime/toolchains.env",
        ".devkit/runtime/pnpm.env",
    ] {
        managed.insert(
            path.to_string(),
            identity(&root.join(path)).unwrap().unwrap(),
        );
    }
    let checksum_body: String = managed
        .iter()
        .map(|(p, id)| format!("{}  {p}\n", id.sha256))
        .collect();
    write_atomic(&root.join(CHECKSUM_PATH), checksum_body.as_bytes(), 0o644).unwrap();
    managed.insert(
        CHECKSUM_PATH.to_string(),
        identity(&root.join(CHECKSUM_PATH)).unwrap().unwrap(),
    );

    let manifest = json!({
        "schemaVersion": 2,
        "version": ver,
        "releaseIdentity": "0".repeat(64),
        "generation": "00000000-0000-4000-8000-000000000000",
        "packageMode": "local",
        "managed": managed,
        "components": {
            "apps/web": {
                "role": "app",
                "toolchain": "node",
                "toolingProfile": "web",
                "origin": { "template": "react-web", "version": ver },
                "toolDependencies": {},
                "dependencies": [],
                "development": { "owner": "managed", "kind": "compose", "service": "web" }
            }
        }
    });
    write_json(&root.join(MANIFEST_PATH), &manifest).unwrap();

    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let run_log = temp.path().join("runs.jsonl");
    let docker_script = fake_bin.join("docker");
    let script = format!(
        r#"#!/bin/sh
set -eu
if [ "$1" = "compose" ] && [ "$2" = "version" ]; then
  printf 'v5.0.2\n'
  exit 0
fi
if [ "$1" = "info" ]; then
  printf '{{"OSType":"linux","Architecture":"{arch}","NCPU":4,"MemTotal":4294967296,"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUShares":true,"PidsLimit":true}}\n'
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
  printf 'linux/{arch}\n'
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "ls" ]; then
  exit 0
fi
if [ "$1" = "run" ]; then
  test -z "${{DOCKER_DEFAULT_PLATFORM:-}}"
  log="{log}"
  printf '%s\n' "$*" >> "$log"
  count=$(wc -l < "$log")
  if [ "$count" -eq 1 ]; then
    printf 'docker: Error response from daemon: cannot enter cgroupv2 "/sys/fs/cgroup/docker" with domain controllers -- it is in threaded mode.\n' >&2
    exit 125
  fi
  exit 0
fi
printf 'unexpected docker args: %s\n' "$*" >&2
exit 90
"#,
        arch = fixture_platform().strip_prefix("linux/").unwrap(),
        log = run_log.display()
    );
    fs::write(&docker_script, script).unwrap();
    fs::set_permissions(&docker_script, fs::Permissions::from_mode(0o755)).unwrap();

    let path_env = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    let out = Command::new(exe)
        .args([
            "exec",
            "--component",
            "apps/web",
            "--root",
            root.to_str().unwrap(),
            "--",
            "true",
        ])
        .env("PATH", path_env)
        .env("DEVKIT_PNPM_BIN_OVERRIDE", exe)
        .env("DOCKER_DEFAULT_PLATFORM", "linux/s390x")
        .env("DOCKER_HOST", "tcp://127.0.0.1:2375")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "expected reactive retry to succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr_str = String::from_utf8_lossy(&out.stderr);
    assert!(stderr_str.contains("retrying without container cgroup limits"));

    let lines: Vec<String> = fs::read_to_string(&run_log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 2, "expected 2 docker run attempts: {lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.contains(&format!("--platform {}", fixture_platform())))
    );
    assert!(lines[0].contains("--cpus ") && lines[0].contains("--memory "));
    assert!(!lines[0].contains("NODE_OPTIONS="));
    for flag in [
        "--cpus ",
        "--cpu-shares ",
        "--memory ",
        "--memory-swap ",
        "--pids-limit ",
    ] {
        assert!(
            !lines[1].contains(flag),
            "retry leaked cgroup flag {flag}: {}",
            lines[1]
        );
    }
    assert!(
        lines[1].contains("NODE_OPTIONS=--max-old-space-size="),
        "retry missing NODE_OPTIONS fallback: {}",
        lines[1]
    );
}

#[test]
fn compose_reactive_retry_heals_exit_1_dind_cgroupv2_failure() {
    use devkit::{
        delivery::file_digest,
        files::{identity, write_atomic, write_json},
        native::admit_native_components,
        projections::{render_app_compose_value, render_project_make},
        state::{CHECKSUM_PATH, MANIFEST_PATH},
    };
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, process::Command};

    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir_all(root.join(".devkit/runtime")).unwrap();
    fs::create_dir_all(root.join(".devkit/state")).unwrap();
    fs::create_dir_all(root.join("apps/web")).unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();

    let exe = Path::new(env!("CARGO_BIN_EXE_devkit"));
    let engine_sha = file_digest(exe).unwrap();
    let ver = env!("CARGO_PKG_VERSION");
    let recipe_hash = "b".repeat(64);
    let node_image = "node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205";

    let engine_pins = fixture_engine_pins(&engine_sha);
    let toolchains = format!(
        "DEVKIT_VERSION={ver}\n{engine_pins}DEVKIT_NODE_IMAGE={node_image}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(
        &root.join(".devkit/runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    let pnpm_pin = devkit::runtime::PnpmPin {
        package: "@pnpm/exe.linux-x64-musl".into(),
        version: "12.21.0".into(),
        tarball_sha256: "a".repeat(64),
        binary_sha256: engine_sha.clone(),
    };
    write_atomic(
        &root.join(".devkit/runtime/pnpm.env"),
        fixture_pnpm_pins(&pnpm_pin).as_bytes(),
        0o644,
    )
    .unwrap();
    for stub in [
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/LICENSE",
    ] {
        write_runtime_stub(&root, stub);
    }
    write_json(
        &root.join(".devkit/tooling-profiles.json"),
        &json!({"schemaVersion":1,"version":ver,"packageMode":"local","profiles":{"web":{}}}),
    )
    .unwrap();
    write_atomic(&root.join("compose.yaml"), b"services: {}\n", 0o644).unwrap();

    let root_manifest =
        json!({"name":"fixture","version":"1.0.0","private":true,"workspaces":["apps/web"]});
    write_json(&root.join("package.json"), &root_manifest).unwrap();
    let app_manifest = json!({
        "name":"web",
        "version":"1.0.0",
        "private":true,
        "scripts":{"check":"true","fix":"true","test":"true","build":"true","dev":"true"}
    });
    write_json(&root.join("apps/web/package.json"), &app_manifest).unwrap();
    let lock = json!({
        "lockfileVersion":3,
        "packages":{
            "": root_manifest,
            "apps/web": app_manifest,
            "node_modules/web": {"resolved":"apps/web","link":true}
        }
    });
    write_json(&root.join("package-lock.json"), &lock).unwrap();

    let release_id = "0".repeat(64);
    let seed_state: ProjectState = serde_json::from_value(json!({
        "schemaVersion": 2,
        "version": ver,
        "releaseIdentity": release_id,
        "generation": "00000000-0000-4000-8000-000000000000",
        "packageMode": "local",
        "managed": {},
        "components": {
            "apps/web": {
                "role": "app",
                "toolchain": "node",
                "toolingProfile": "web",
                "origin": { "template": "react-web", "version": ver },
                "toolDependencies": {},
                "dependencies": [],
                "development": { "owner": "managed", "kind": "compose", "service": "web" }
            }
        }
    }))
    .unwrap();
    modernize_node_fixture(&root, &seed_state);
    let native = admit_native_components(&root, &seed_state).unwrap();
    write_atomic(
        &root.join(".devkit/project.mk"),
        render_project_make(&seed_state).as_bytes(),
        0o644,
    )
    .unwrap();
    write_json(
        &root.join(".devkit/compose.apps.yaml"),
        &render_app_compose_value(&seed_state, native.cargo_workspace.as_ref()).unwrap(),
    )
    .unwrap();

    let mut managed = BTreeMap::new();
    for path in [
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/tooling-profiles.json",
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/LICENSE",
        ".devkit/runtime/toolchains.env",
        ".devkit/runtime/pnpm.env",
    ] {
        managed.insert(
            path.to_string(),
            identity(&root.join(path)).unwrap().unwrap(),
        );
    }
    let checksum_body: String = managed
        .iter()
        .map(|(p, id)| format!("{}  {p}\n", id.sha256))
        .collect();
    write_atomic(&root.join(CHECKSUM_PATH), checksum_body.as_bytes(), 0o644).unwrap();
    managed.insert(
        CHECKSUM_PATH.to_string(),
        identity(&root.join(CHECKSUM_PATH)).unwrap().unwrap(),
    );

    let mut final_state = seed_state;
    final_state.managed = managed;
    write_json(
        &root.join(MANIFEST_PATH),
        &serde_json::to_value(&final_state).unwrap(),
    )
    .unwrap();
    let receipt = devkit::doctor::NodeWorkspaceReceipt::new(
        fixture_platform(),
        &release_id,
        &native.node_inputs.digest,
    );
    write_json(&root.join(".devkit/state/node-workspace.lock"), &receipt).unwrap();

    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let attempt_dir = temp.path().join("attempts");
    fs::create_dir_all(&attempt_dir).unwrap();
    let cache = temp.path().join("cache");
    let ports = devkit::stand::ports::Ports::open(&root, &cache).unwrap();
    ports.save().unwrap();
    let project = devkit::doctor::compose_project_name(&root);
    let container_id = "a".repeat(64);
    // A daemon cgroup error happens after container creation. Preserve that
    // outcome in the fixture instead of claiming the creator produced nothing.
    for (name, running) in [("stopped", false), ("running", true)] {
        let values = vec![
            json!(container_id),
            json!(ports.owner.key),
            json!(project),
            json!("web"),
            json!(running),
            json!(format!("sha256:{}", "b".repeat(64))),
            if running {
                json!({"5173/tcp":[{"HostIp":"127.0.0.1","HostPort":"45173"}]})
            } else {
                json!({})
            },
            json!([]),
            json!({"5173/tcp":[{"HostIp":"127.0.0.1","HostPort":"45173"}]}),
            json!(""),
            json!(""),
            json!(""),
            json!(""),
            json!(""),
            json!(""),
            json!(""),
            json!(""),
            json!(""),
            json!("1"),
        ];
        fs::write(
            temp.path().join(format!("{name}-observation")),
            values
                .iter()
                .map(|value| format!("{value}\n"))
                .collect::<String>(),
        )
        .unwrap();
    }
    let docker_script = fake_bin.join("docker");
    let script = format!(
        r#"#!/bin/sh
set -eu
if [ "$1" = "compose" ] && [ "$2" = "version" ]; then
  printf 'v5.0.2\n'
  exit 0
fi
if [ "$1" = "info" ]; then
  case "$*" in *'.ID'* ) printf 'fixture-daemon\n'; exit 0 ;; esac
  printf '{{"OSType":"linux","Architecture":"{arch}","NCPU":4,"MemTotal":4294967296,"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUShares":true,"PidsLimit":true}}\n'
  exit 0
fi
if [ "$1" = "context" ]; then
  case "$2" in
    inspect) printf 'default\ntcp://127.0.0.1:2375\n' ;;
    *) exit 90 ;;
  esac
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
  printf 'linux/{arch}\n'
  exit 0
fi
if [ "$1" = "ps" ]; then
  if [ -f "{fixture}/created" ]; then printf '%s\n' '{container_id}'; fi
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "inspect" ]; then
  if [ -f "{dir}/attempt-2.json" ]; then
    cat "{fixture}/running-observation"
  else
    cat "{fixture}/stopped-observation"
  fi
  exit 0
fi
if [ "$1" = "compose" ]; then
  case " $* " in
    *" config --no-env-resolution --format json "*)
      printf '{{"services":{{"web":{{}}}}}}\n'
      exit 0
      ;;
    *" up --no-start web "*)
      touch "{fixture}/created"
      exit 0
      ;;
    *" start --wait web "*)
      overlay=""
      prev=""
      for arg in "$@"; do
        if [ "$prev" = "--file" ]; then
          case "$arg" in
            *compose-resources-*) overlay="$arg" ;;
          esac
        fi
        prev="$arg"
      done
      idx=$(ls -1 "{dir}" | wc -l)
      idx=$((idx + 1))
      cp "$overlay" "{dir}/attempt-$idx.json"
      if [ "$idx" -eq 1 ]; then
        printf 'Error response from daemon: failed to create task for container: cannot enter cgroupv2 "/sys/fs/cgroup/docker" with domain controllers -- it is in threaded mode\n' >&2
        exit 1
      fi
      exit 0
      ;;
    *" ps --format json "*)
      printf '{{"Service":"web","Name":"devkit-web-1","State":"running","Publishers":[{{"URL":"127.0.0.1","PublishedPort":45173,"TargetPort":5173,"Protocol":"tcp"}}]}}\n'
      exit 0
      ;;
  esac
fi
printf 'unexpected docker args: %s\n' "$*" >&2
exit 90
"#,
        arch = fixture_platform().strip_prefix("linux/").unwrap(),
        dir = attempt_dir.display(),
        fixture = temp.path().display(),
    );
    fs::write(&docker_script, script).unwrap();
    fs::set_permissions(&docker_script, fs::Permissions::from_mode(0o755)).unwrap();

    let path_env = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    let out = Command::new(exe)
        .args([
            "task",
            "up",
            "--root",
            root.to_str().unwrap(),
            "--component",
            "apps/web",
        ])
        .env("PATH", path_env)
        .env("DEVKIT_PNPM_BIN_OVERRIDE", exe)
        .env("DEVKIT_CACHE_DIR", &cache)
        .env("DOCKER_DEFAULT_PLATFORM", "linux/s390x")
        .env("DOCKER_HOST", "tcp://127.0.0.1:2375")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "expected compose reactive retry to heal exit-1 cgroupv2 error, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("retrying Compose without container cgroup limits"),
        "missing warning on stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("Service web (apps/web): http://127.0.0.1:45173"),
        "missing discovered URL on stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let first_overlay: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(attempt_dir.join("attempt-1.json")).unwrap())
            .unwrap();
    let second_overlay: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(attempt_dir.join("attempt-2.json")).unwrap())
            .unwrap();

    let svc1 = &first_overlay["services"]["web"];
    assert!(svc1.get("cpus").is_some() && svc1.get("mem_limit").is_some());
    assert!(svc1["environment"].get("NODE_OPTIONS").is_none());

    let svc2 = &second_overlay["services"]["web"];
    for stripped in [
        "cpus",
        "cpu_shares",
        "mem_limit",
        "memswap_limit",
        "pids_limit",
    ] {
        assert!(
            svc2.get(stripped).is_none(),
            "attempt 2 retained {stripped}: {svc2}"
        );
    }
    assert!(
        svc2["environment"]["NODE_OPTIONS"]
            .as_str()
            .is_some_and(|v| v.starts_with("--max-old-space-size=")),
        "attempt 2 missing fallback NODE_OPTIONS: {svc2}"
    );
}

#[test]
fn derived_image_tags_existing_state_on_prepare_and_reuses_local_tag_without_build() {
    use devkit::{
        delivery::file_digest,
        files::{digest, identity, write_atomic, write_json},
        native::admit_native_components,
        projections::{render_app_compose_value, render_project_make},
        state::{CHECKSUM_PATH, MANIFEST_PATH},
    };
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, process::Command};

    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir_all(root.join(".devkit/runtime")).unwrap();
    fs::create_dir_all(root.join(".devkit/state")).unwrap();
    fs::create_dir_all(root.join("apps/api/src")).unwrap();

    let exe = Path::new(env!("CARGO_BIN_EXE_devkit"));
    let engine_sha = file_digest(exe).unwrap();
    let ver = env!("CARGO_PKG_VERSION");
    let rust_dockerfile = include_bytes!("../../../runtime/rust.Dockerfile");
    let rust_recipe = digest(rust_dockerfile);
    let node_image = "node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205";

    let engine_pins = fixture_engine_pins(&engine_sha);
    let toolchains = format!(
        "DEVKIT_VERSION={ver}\n{engine_pins}DEVKIT_NODE_IMAGE={node_image}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={rust_recipe}\n"
    );
    write_atomic(
        &root.join(".devkit/runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    write_atomic(
        &root.join(".devkit/runtime/rust.Dockerfile"),
        rust_dockerfile,
        0o644,
    )
    .unwrap();
    for stub in [
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/LICENSE",
    ] {
        write_runtime_stub(&root, stub);
    }
    write_json(
        &root.join(".devkit/tooling-profiles.json"),
        &json!({"schemaVersion":1,"version":ver,"packageMode":"local","profiles":{"rust":{}}}),
    )
    .unwrap();
    write_atomic(
        &root.join("Cargo.toml"),
        b"[workspace]\nresolver = \"3\"\nmembers = [\"apps/api\"]\n",
        0o644,
    )
    .unwrap();
    write_atomic(
        &root.join("apps/api/Cargo.toml"),
        b"[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        0o644,
    )
    .unwrap();
    write_atomic(
        &root.join("Cargo.lock"),
        b"version = 4\n\n[[package]]\nname = \"api\"\nversion = \"0.1.0\"\n",
        0o644,
    )
    .unwrap();
    write_atomic(
        &root.join("rust-toolchain.toml"),
        b"[toolchain]\nchannel = \"1.99.1\"\n",
        0o644,
    )
    .unwrap();
    write_atomic(&root.join("rustfmt.toml"), b"edition = \"2024\"\n", 0o644).unwrap();
    write_atomic(&root.join("apps/api/src/main.rs"), b"fn main() {}\n", 0o644).unwrap();

    let seed_state: ProjectState = serde_json::from_value(json!({
        "schemaVersion": 2,
        "version": ver,
        "releaseIdentity": "0".repeat(64),
        "generation": "00000000-0000-4000-8000-000000000000",
        "packageMode": "local",
        "managed": {},
        "components": {
            "apps/api": {
                "role": "app",
                "toolchain": "rust",
                "toolingProfile": "rust",
                "origin": { "template": "rust-api", "version": ver },
                "toolDependencies": {},
                "dependencies": [],
                "development": { "owner": "managed", "kind": "compose", "service": "api" }
            }
        }
    }))
    .unwrap();
    modernize_node_fixture(&root, &seed_state);
    let native = admit_native_components(&root, &seed_state).unwrap();
    write_atomic(
        &root.join(".devkit/project.mk"),
        render_project_make(&seed_state).as_bytes(),
        0o644,
    )
    .unwrap();
    write_json(
        &root.join(".devkit/compose.apps.yaml"),
        &render_app_compose_value(&seed_state, native.cargo_workspace.as_ref()).unwrap(),
    )
    .unwrap();

    let mut managed = BTreeMap::new();
    for path in [
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/tooling-profiles.json",
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/LICENSE",
        ".devkit/runtime/toolchains.env",
        ".devkit/runtime/rust.Dockerfile",
        "rust-toolchain.toml",
    ] {
        managed.insert(
            path.to_string(),
            identity(&root.join(path)).unwrap().unwrap(),
        );
    }
    let checksum_body: String = managed
        .iter()
        .map(|(p, id)| format!("{}  {p}\n", id.sha256))
        .collect();
    write_atomic(&root.join(CHECKSUM_PATH), checksum_body.as_bytes(), 0o644).unwrap();
    managed.insert(
        CHECKSUM_PATH.to_string(),
        identity(&root.join(CHECKSUM_PATH)).unwrap().unwrap(),
    );
    let mut final_state = seed_state;
    final_state.managed = managed;
    write_json(
        &root.join(MANIFEST_PATH),
        &serde_json::to_value(&final_state).unwrap(),
    )
    .unwrap();

    let img_id = format!("sha256:{}", "e".repeat(64));
    let platform = fixture_platform();
    let arch = platform.strip_prefix("linux/").unwrap();
    let derived_tag = format!("devkit-derived-rust:{arch}-{rust_recipe}");
    write_json(
        &root.join(".devkit/state/rust-image.json"),
        &devkit::doctor::DerivedImageReceipt::new(platform, &img_id, &rust_recipe),
    )
    .unwrap();

    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let docker_log = temp.path().join("docker.log");
    let mode_file = temp.path().join("tag-mode");
    let fetch_failure = temp.path().join("fail-fetch");
    fs::write(&mode_file, "present").unwrap();
    let docker_script = fake_bin.join("docker");
    let script = format!(
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "compose" ] && [ "$2" = "version" ]; then
  printf 'v5.0.2\n'
  exit 0
fi
if [ "$1" = "info" ]; then
  printf '{{"OSType":"linux","Architecture":"{arch}","NCPU":4,"MemTotal":4294967296,"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUShares":true,"PidsLimit":true,"ClientInfo":{{"Context":"default"}}}}\n'
  exit 0
fi
if [ "$1" = "context" ]; then
  case "$2" in
    show) printf 'default\n' ;;
    inspect) printf 'default\nunix:///var/run/docker.sock\n' ;;
    *) exit 90 ;;
  esac
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "tag" ]; then
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
  last=""
  for arg in "$@"; do last="$arg"; done
  if [ "$last" = "{img_id}" ]; then
    printf '{img_id}\n%s\n{pin}\nlinux/{arch}\n' "${{FIXTURE_IMAGE_PLATFORM:-linux/{arch}}}"
    exit 0
  fi
  if [ "$last" = "{tag}" ]; then
    if [ "$(cat "{mode}")" = "present" ]; then
      printf '{img_id}\nlinux/{arch}\n{pin}\nlinux/{arch}\n'
      exit 0
    fi
    exit 1
  fi
  printf 'linux/{arch}\n'
  exit 0
fi
if [ "$1" = "build" ]; then
  test "$(sha256sum | cut -d' ' -f1)" = "{pin}"
  prev=""
  for arg in "$@"; do
    if [ "$prev" = "--iidfile" ]; then
      printf '{img_id}\n' > "$arg"
    fi
    prev="$arg"
  done
  exit 0
fi
if [ "$1" = "run" ]; then
  case "$*" in
    *"internal rust setup"*) if [ -e "{fetch_failure}" ]; then exit 21; fi ;;
  esac
  exit 0
fi
exit 0
"#,
        log = docker_log.display(),
        img_id = img_id,
        pin = rust_recipe,
        tag = derived_tag,
        mode = mode_file.display(),
        fetch_failure = fetch_failure.display(),
    );
    fs::write(&docker_script, script).unwrap();
    fs::set_permissions(&docker_script, fs::Permissions::from_mode(0o755)).unwrap();
    let path_env = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );

    // 1. Non-prepare (`exec` without `--prepare`) hits `.devkit/state/rust-image` without mutating daemon tags.
    fs::write(&docker_log, "").unwrap();
    let check_out = Command::new(exe)
        .args([
            "exec",
            "--component",
            "apps/api",
            "--root",
            root.to_str().unwrap(),
            "--",
            "true",
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        check_out.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&check_out.stderr)
    );
    let log1 = fs::read_to_string(&docker_log).unwrap();
    assert!(
        !log1.contains("image tag"),
        "non-prepare tagged image: {log1}"
    );
    assert!(!log1.contains("build "), "non-prepare built image: {log1}");
    assert!(log1.contains("--workdir /workspace/apps/api"));
    assert!(log1.contains("CARGO_TARGET_DIR=/tmp/cargo-target"));
    assert!(log1.contains("CARGO_INCREMENTAL=0"));
    assert!(log1.contains("\"target=/cache/cargo\",readonly"));
    assert!(!root.join("target").exists());
    assert!(!root.join(".devkit/state/rust-workspace.lock").exists());

    // Read-only exec with --network bridge (without --write) still keeps /cache/cargo read-only.
    fs::write(&docker_log, "").unwrap();
    let bridge_ro = Command::new(exe)
        .args([
            "exec",
            "--toolchain",
            "rust",
            "--network",
            "bridge",
            "--root",
            root.to_str().unwrap(),
            "--",
            "true",
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        bridge_ro.status.success(),
        "bridge ro exec failed: {}",
        String::from_utf8_lossy(&bridge_ro.stderr)
    );
    let log_bridge = fs::read_to_string(&docker_log).unwrap();
    assert!(
        log_bridge.contains("\"target=/cache/cargo\",readonly"),
        "read-only exec --network bridge must keep /cache/cargo read-only:\n{log_bridge}"
    );

    let foreign = if platform == "linux/amd64" {
        "linux/arm64"
    } else {
        "linux/amd64"
    };
    fs::write(&docker_log, "").unwrap();
    let rejected = Command::new(exe)
        .args([
            "exec",
            "--component",
            "apps/api",
            "--root",
            root.to_str().unwrap(),
            "--",
            "true",
        ])
        .env("PATH", &path_env)
        .env("FIXTURE_IMAGE_PLATFORM", foreign)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("accepted rust image is absent/stale")
    );
    assert!(
        !fs::read_to_string(&docker_log)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("run "))
    );

    // 2. Prepare (`task setup`) hits `.devkit/state/rust-image` and idempotently applies `devkit-derived-rust:<pin>`.
    fs::write(&docker_log, "").unwrap();
    let setup_hit = Command::new(exe)
        .args(["task", "setup-rust", "--root", root.to_str().unwrap()])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        setup_hit.status.success(),
        "task setup (state hit) failed: {}",
        String::from_utf8_lossy(&setup_hit.stderr)
    );
    let log2 = fs::read_to_string(&docker_log).unwrap();
    assert!(
        log2.contains(&format!("image tag {img_id} {derived_tag}")),
        "prepare on existing state did not apply local tag: {log2}"
    );
    assert!(
        !log2.contains("build "),
        "prepare on existing state ran build: {log2}"
    );

    assert!(log2.contains("internal rust setup --workspace-root /workspace"));
    assert!(log2.contains("--workdir /workspace "));
    assert!(!log2.contains("target=/workspace/target"));
    let receipt = root.join(".devkit/state/rust-workspace.lock");
    assert!(receipt.is_file());
    let setup_in = |cache| {
        Command::new(exe)
            .args(["task", "setup-rust", "--root", root.to_str().unwrap()])
            .env("PATH", &path_env)
            .env("DEVKIT_CACHE_DIR", temp.path().join(cache))
            .output()
            .unwrap()
    };
    assert!(setup_in("alternate-cache").status.success());
    fs::write(&fetch_failure, "").unwrap();
    assert!(!setup_in("alternate-cache").status.success());
    fs::remove_file(fetch_failure).unwrap();
    assert!(setup_in("cache").status.success());
    let stale = Command::new(exe)
        .args([
            "task",
            "setup",
            "--component",
            "apps/api",
            "--root",
            root.to_str().unwrap(),
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("alternate-cache"))
        .output()
        .unwrap();
    assert!(!stale.status.success());
    assert!(
        String::from_utf8_lossy(&stale.stderr)
            .contains("rust workspace dependencies are absent/stale")
    );
    fs::write(&docker_log, "").unwrap();
    let setup_component = Command::new(exe)
        .args([
            "task",
            "setup",
            "--component",
            "apps/api",
            "--root",
            root.to_str().unwrap(),
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        setup_component.status.success(),
        "{}",
        String::from_utf8_lossy(&setup_component.stderr)
    );
    assert!(
        !fs::read_to_string(&docker_log)
            .unwrap()
            .contains("internal rust setup")
    );
    fs::write(&docker_log, "").unwrap();
    let component_fix = Command::new(exe)
        .args([
            "task",
            "fix",
            "--component",
            "apps/api",
            "--root",
            root.to_str().unwrap(),
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        component_fix.status.success(),
        "{}",
        String::from_utf8_lossy(&component_fix.stderr)
    );
    let log = fs::read_to_string(&docker_log).unwrap();
    assert!(log.contains("target=/workspace/target"));
    assert!(log.contains("\"target=/cache/cargo\",readonly"));
    assert!(log.contains("RUSTC=/usr/local/cargo/bin/rustc"));
    assert!(log.contains("CARGO_NET_GIT_FETCH_WITH_CLI=false"));
    assert!(log.contains("--network none"));
    assert!(log.contains("--workdir /workspace "));
    assert!(log.contains("internal rust fix --workspace-root /workspace --component apps/api --member apps/api --fmt-package api --package path+file:///workspace/apps/api#api@0.1.0"));
    // Setup receipt must live in cargo-receipts/, never inside CARGO_HOME (/cache/cargo).
    assert!(temp.path().join("cache/cargo-receipts").is_dir());
    assert!(!temp.path().join("cache/cargo/.devkit").exists());
    assert!(
        !fs::read_dir(temp.path().join("cache/cargo"))
            .unwrap()
            .any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("devkit-cargo-cache")),
        "receipt must not live inside CARGO_HOME"
    );
    // Poisoned CARGO_HOME config.toml is rejected before launching Docker.
    let poison = temp.path().join("cache/cargo/config.toml");
    fs::write(&poison, "[build]\nrustc-wrapper = '/bin/sh'\n").unwrap();
    fs::write(&docker_log, "").unwrap();
    let poisoned_fix = Command::new(exe)
        .args([
            "task",
            "fix",
            "--component",
            "apps/api",
            "--root",
            root.to_str().unwrap(),
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(!poisoned_fix.status.success());
    assert!(
        String::from_utf8_lossy(&poisoned_fix.stderr)
            .contains("Refusing unexpected Cargo configuration in shared cache"),
        "{}",
        String::from_utf8_lossy(&poisoned_fix.stderr)
    );
    assert!(!fs::read_to_string(&docker_log).unwrap().contains("run "));
    fs::remove_file(&poison).unwrap();
    let root_exec = Command::new(exe)
        .args([
            "exec",
            "--write",
            "--root",
            root.to_str().unwrap(),
            "--",
            "cargo",
            "update",
            "--workspace",
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        root_exec.status.success(),
        "{}",
        String::from_utf8_lossy(&root_exec.stderr)
    );
    assert!(!receipt.exists());
    assert_eq!(
        fs::read_dir(temp.path().join("cache/cargo-receipts"))
            .unwrap()
            .count(),
        0,
        "exec --write must also remove the cargo-receipts marker"
    );

    // 3. Fresh checkout without `.devkit/state/rust-image.json` reuses local tag without `docker build`.
    fs::remove_file(root.join(".devkit/state/rust-image.json")).unwrap();
    fs::write(&docker_log, "").unwrap();
    let setup_tag_hit = Command::new(exe)
        .args(["task", "setup-rust", "--root", root.to_str().unwrap()])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        setup_tag_hit.status.success(),
        "task setup (tag hit) failed: {}",
        String::from_utf8_lossy(&setup_tag_hit.stderr)
    );
    let log3 = fs::read_to_string(&docker_log).unwrap();
    assert!(
        !log3.contains("build "),
        "tag hit unexpectedly ran docker build: {log3}"
    );
    let receipt: devkit::doctor::DerivedImageReceipt =
        devkit::files::read_json(&root.join(".devkit/state/rust-image.json")).unwrap();
    assert_eq!(receipt.image_id, img_id);
    assert!(!root.join(".devkit/state/rust-image").exists());

    // 4. Cold daemon without state or local tag runs `docker build --tag devkit-derived-rust:<pin>`.
    fs::remove_file(root.join(".devkit/state/rust-image.json")).unwrap();
    fs::write(&mode_file, "absent").unwrap();
    fs::write(&docker_log, "").unwrap();
    let setup_cold = Command::new(exe)
        .args(["task", "setup-rust", "--root", root.to_str().unwrap()])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        setup_cold.status.success(),
        "task setup (cold build) failed: {}",
        String::from_utf8_lossy(&setup_cold.stderr)
    );
    let log4 = fs::read_to_string(&docker_log).unwrap();
    assert!(
        log4.contains(&format!("--tag {derived_tag}")),
        "cold docker build missing --tag {derived_tag}: {log4}"
    );
}
