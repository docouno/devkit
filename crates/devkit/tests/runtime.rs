use devkit::{
    model::ProjectState,
    projections::render_app_compose_value,
    runtime::{ToolPins, bind_mount, development_components},
};
use serde_json::json;
use std::path::Path;
fn state(version: &str) -> ProjectState {
    serde_json::from_value(json!({"schemaVersion":2,"version":version,"releaseIdentity":"0".repeat(64),"generation":"00000000-0000-4000-8000-000000000000","packageMode":"registry","managed":{},"components":{
"apps/web":{"role":"app","toolchain":"node","toolingProfile":"web","origin":{"template":"react-web","version":version},"toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"compose","service":"web"}},
"apps/api":{"role":"app","toolchain":"rust","toolingProfile":"rust","origin":{"template":"rust-api","version":version},"toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"compose","service":"api"}},
"apps/desktop":{"role":"app","toolchain":"node","toolingProfile":"electron","origin":{"template":"electron-react","version":version},"toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"desktop","service":"desktop"}}}})).unwrap()
}
#[test]
fn compose_selected_profiles_and_optional_dependencies_match_native_reachability() {
    let state = state("0.3.0");
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
fn old_desktop_projection_stays_shell_compatible_while_native_uses_same_elf() {
    let old = render_app_compose_value(&state("0.2.1")).unwrap();
    assert_eq!(
        old["services"]["desktop"]["command"],
        json!(["bash", "/workspace/.devkit/runtime/desktop-dev.sh"])
    );
    let new = render_app_compose_value(&state("0.3.0")).unwrap();
    assert_eq!(
        new["services"]["desktop"]["command"],
        json!(["/devkit/devkit", "internal", "desktop"])
    );
    assert!(
        new["services"]["desktop"]["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["target"] == "/devkit/devkit" && v["read_only"] == true)
    );
    assert!(
        !old["services"]["desktop"]["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["target"] == "/devkit/devkit")
    );
}
#[test]
fn tool_pin_parser_never_evaluates_shell_and_rejects_duplicate_or_unpinned_images() {
    let source = format!(
        "DEVKIT_VERSION=0.3.0\nDEVKIT_NODE_IMAGE=node:24@sha256:{}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={}\nDEVKIT_DESKTOP_RECIPE_HASH={}\nDEVKIT_ENGINE_SHA256={}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\n",
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64),
        "d".repeat(64)
    );
    assert!(ToolPins::parse(source.as_bytes()).is_ok());
    let legacy = source
        .lines()
        .filter(|line| !line.starts_with("DEVKIT_ENGINE_"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("DEVKIT_VERSION=0.3.0", "DEVKIT_VERSION=0.2.1");
    assert!(ToolPins::parse(legacy.as_bytes()).unwrap().engine.is_none());
    assert!(
        ToolPins::parse(
            legacy
                .replace("DEVKIT_VERSION=0.2.1", "DEVKIT_VERSION=0.3.0")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        ToolPins::parse(
            source
                .replace("DEVKIT_VERSION=0.3.0", "DEVKIT_VERSION=0.2.1")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        ToolPins::parse(
            source
                .replace("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl")
                .as_bytes()
        )
        .is_err()
    );
    assert!(ToolPins::parse(format!("{source}DEVKIT_VERSION=0.3.0\n").as_bytes()).is_err());
    assert!(
        ToolPins::parse(
            source
                .replace("node:24@sha256:", "$(touch/tmp/NO)@sha256:")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        ToolPins::parse(
            source
                .replace(&format!("@sha256:{}", "a".repeat(64)), "")
                .as_bytes()
        )
        .is_err()
    );
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
    let state = state("0.3.0");
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
        ("Cargo.lock", "version = 4\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel=\"1.99.1\"\n"),
        ("rustfmt.toml", "edition=\"2024\"\n"),
    ] {
        std::fs::write(root.join("apps/api").join(name), contents).unwrap();
    }
    write_json(&root.join(".devkit/tooling-profiles.json"),&json!({"schemaVersion":1,"version":"0.3.0","packageMode":"registry","profiles":{"web":{},"electron":{},"rust":{}}})).unwrap();
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
