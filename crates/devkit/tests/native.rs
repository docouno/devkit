use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    files::{read_json, write_atomic, write_json},
    graph::{
        LockSeed, assert_product_graphs_preserved, capture_product_graph, read_native_graph,
        seed_workspace_lock,
    },
    model::{ComponentState, ProjectState},
    native::{
        admit_native_components, assert_native_components, assert_npmrc_compatibility,
        collect_native_input_closure, derive_component_dependencies, package_lock, valid_integrity,
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, name: &str, bytes: &str) {
    write_atomic(&root.join(name), bytes.as_bytes(), 0o644).unwrap();
}
fn put_json(root: &Path, name: &str, value: &Value) {
    write_json(&root.join(name), value).unwrap();
}
fn sri(byte: u8) -> String {
    format!("sha512-{}", STANDARD.encode([byte; 64]))
}
fn artifact(name: &str, extra: Value) -> Value {
    let mut value = json!({"version":"1.0.0","resolved":format!("https://registry.npmjs.org/{name}/-/{name}-1.0.0.tgz"),"integrity":sri(1)});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    value
}
fn component(path: &str, toolchain: &str) -> ComponentState {
    serde_json::from_value(json!({
        "role":if path.starts_with("apps/") {"app"} else {"library"},"toolchain":toolchain,
        "toolingProfile":if toolchain=="rust" {"rust"} else if path.starts_with("apps/") {"node"} else {"react-library"},
        "origin":{"template":"adopted","version":"0.2.1"},"toolDependencies":{},"dependencies":[]
    })).unwrap()
}
fn state(components: &[(&str, &str)]) -> ProjectState {
    serde_json::from_value(json!({"schemaVersion":2,"version":"0.2.1","releaseIdentity":"0".repeat(64),
        "generation":"00000000-0000-4000-8000-000000000000","packageMode":"registry","managed":{},
        "components":components.iter().map(|(p,t)| (p.to_string(),serde_json::to_value(component(p,t)).unwrap())).collect::<BTreeMap<_,_>>()
    })).unwrap()
}
fn manifest(name: &str, deps: Value, library: bool) -> Value {
    let mut value = json!({"name":name,"version":"1.0.0","dependencies":deps,
        "scripts":{"check":"check","fix":"fix","test":"test","build":"build","dev":"dev"}});
    if library {
        value["exports"] = json!({".":"./src/index.ts"});
    }
    value
}
fn profiles(root: &Path, state: &ProjectState) {
    let profiles = state
        .components
        .values()
        .map(|c| (c.tooling_profile.as_str(), json!(c.tool_dependencies)))
        .collect::<BTreeMap<_, _>>();
    put_json(
        root,
        ".devkit/tooling-profiles.json",
        &json!({"schemaVersion":1,"version":state.version,"packageMode":state.package_mode,"profiles":profiles}),
    );
}
fn node_project(root: &Path, state: &ProjectState, members: &[(&str, Value)], artifacts: Value) {
    let paths: Vec<_> = state
        .components
        .iter()
        .filter(|(_, c)| c.toolchain.as_str() == "node")
        .map(|(p, _)| p)
        .collect();
    let root_manifest =
        json!({"name":"consumer","version":"1.0.0","private":true,"workspaces":paths});
    put_json(root, "package.json", &root_manifest);
    let mut packages = json!({"":root_manifest});
    for (path, member) in members {
        put_json(root, &format!("{path}/package.json"), member);
        packages[*path] = member.clone();
        let link = format!("node_modules/{}", member["name"].as_str().unwrap());
        packages[link] = json!({"resolved":path,"link":true});
    }
    packages
        .as_object_mut()
        .unwrap()
        .extend(artifacts.as_object().unwrap().clone());
    put_json(
        root,
        "package-lock.json",
        &json!({"lockfileVersion":3,"packages":packages}),
    );
    profiles(root, state);
}
fn rust(root: &Path, path: &str, contents: &str, registered: bool) {
    put(root, &format!("{path}/Cargo.toml"), contents);
    if registered {
        put(root, &format!("{path}/Cargo.lock"), "version = 4\n");
        put(
            root,
            &format!("{path}/rustfmt.toml"),
            "edition = \"2024\"\n",
        );
        put(
            root,
            &format!("{path}/rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.99.1\"\n",
        );
    }
}

#[test]
fn lock_target_distinguishes_registry_artifact_from_same_named_library() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node"), ("packages/shared", "node")]);
    let app = manifest("@test/main", json!({"shared":"1.0.0"}), false);
    let library = manifest("shared", json!({}), true);
    node_project(
        root,
        &state,
        &[("apps/main", app), ("packages/shared", library)],
        json!({"apps/main/node_modules/shared":artifact("shared",json!({}))}),
    );
    assert_eq!(
        derive_component_dependencies(root, &state).unwrap()["apps/main"],
        Vec::<String>::new()
    );
    assert_native_components(root, &state).unwrap();
    let mut lock: Value = read_json(&root.join("package-lock.json")).unwrap();
    lock["packages"]
        .as_object_mut()
        .unwrap()
        .remove("apps/main/node_modules/shared");
    put_json(root, "package-lock.json", &lock);
    assert_eq!(
        derive_component_dependencies(root, &state).unwrap()["apps/main"],
        vec!["packages/shared"]
    );
    assert!(
        assert_native_components(root, &state)
            .unwrap_err()
            .to_string()
            .contains("dependency projection is stale")
    );
}

#[test]
fn strict_artifact_integrity_and_root_shrinkwrap_are_admission_boundaries() {
    for (algorithm, length) in [("sha1", 20), ("sha256", 32), ("sha384", 48), ("sha512", 64)] {
        assert!(valid_integrity(&format!(
            "{algorithm}-{}",
            STANDARD.encode(vec![0; length])
        )));
        assert!(!valid_integrity(&format!(
            "{algorithm}-{}",
            STANDARD.encode(vec![0; length - 1])
        )));
    }
    for invalid in [
        "sha512-AA==",
        "sha512-",
        "sha512-???",
        "sha999-YQ==",
        "sha256-YQ== sha512-YQ==",
    ] {
        assert!(!valid_integrity(invalid));
    }
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[("apps/main", manifest("main", json!({"dep":"1.0.0"}), false))],
        json!({"node_modules/dep":artifact("dep",json!({}))}),
    );
    package_lock(root).unwrap();
    let original: Value = read_json(&root.join("package-lock.json")).unwrap();
    let mut incomplete = original.clone();
    incomplete["packages"]["node_modules/dep"]
        .as_object_mut()
        .unwrap()
        .remove("integrity");
    put_json(root, "package-lock.json", &incomplete);
    assert!(
        package_lock(root)
            .unwrap_err()
            .to_string()
            .contains("Unprovable native artifact")
    );
    put_json(root, "package-lock.json", &original);
    put_json(root, "npm-shrinkwrap.json", &original);
    assert!(
        package_lock(root)
            .unwrap_err()
            .to_string()
            .contains("npm-shrinkwrap.json conflicts")
    );
}

#[test]
fn native_closure_binds_transitive_local_manifests_optional_config_and_archive_bytes() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[(
            "apps/main",
            manifest("main", json!({"a":"file:../../vendor packages/a"}), false),
        )],
        json!({
            "node_modules/a":{"resolved":"vendor packages/a","link":true},
            "vendor packages/a":{"name":"a","version":"1.0.0","dependencies":{"b":"file:../b"}},
            "node_modules/b":{"resolved":"vendor packages/b","link":true},
            "vendor packages/b":{"name":"b","version":"1.0.0","dependencies":{"bytes":"file:../bytes.tgz"}},
            "node_modules/bytes":{"version":"1.0.0","resolved":"file:vendor packages/bytes.tgz","integrity":sri(2)}
        }),
    );
    put_json(
        root,
        "vendor packages/a/package.json",
        &json!({"name":"a","version":"1.0.0","dependencies":{"b":"file:../b"}}),
    );
    put_json(
        root,
        "vendor packages/b/package.json",
        &json!({"name":"b","version":"1.0.0","dependencies":{"bytes":"file:../bytes.tgz"}}),
    );
    put(root, "vendor packages/bytes.tgz", "archive one");
    let first = collect_native_input_closure(root, &state).unwrap();
    assert!(first.files.contains_key("vendor packages/b/package.json"));
    assert_eq!(first.files["vendor packages/b/.npmrc"], None);
    assert!(first.files["vendor packages/bytes.tgz"].is_some());
    put(root, "vendor packages/b/.npmrc", "fund=false\n");
    let second = collect_native_input_closure(root, &state).unwrap();
    assert_ne!(first.digest, second.digest);
    put(root, "vendor packages/bytes.tgz", "archive two");
    let third = collect_native_input_closure(root, &state).unwrap();
    assert_ne!(second.digest, third.digest);
    put(
        root,
        "vendor packages/b/package.json",
        "{\"name\":\"b\",\"version\":\"2.0.0\"}\n",
    );
    assert_ne!(
        third.digest,
        collect_native_input_closure(root, &state).unwrap().digest
    );
    put(root, "vendor packages/b/.npmrc", "legacy-peer-deps=true\n");
    assert!(
        assert_npmrc_compatibility(root, &second.directories)
            .unwrap_err()
            .to_string()
            .contains("workspace context")
    );
}

#[test]
fn native_closure_refuses_escaping_and_symlink_local_inputs() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[(
            "apps/main",
            manifest("main", json!({"outside":"file:../../../outside"}), false),
        )],
        json!({}),
    );
    assert!(collect_native_input_closure(root, &state).is_err());
    put_json(
        root,
        "apps/main/package.json",
        &manifest("main", json!({"outside":"file:../../linked"}), false),
    );
    std::os::unix::fs::symlink(f.path().parent().unwrap(), root.join("linked")).unwrap();
    assert!(
        collect_native_input_closure(root, &state)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
}

#[test]
fn native_admission_checks_competing_unregistered_and_stale_native_inputs() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[("apps/main", manifest("main", json!({}), false))],
        json!({}),
    );
    assert_native_components(root, &state).unwrap();
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"embedded\"\nversion=\"1.0.0\"\n",
    );
    assert!(
        assert_native_components(root, &state)
            .unwrap_err()
            .to_string()
            .contains("Ambiguous native manifests")
    );
    fs::remove_file(root.join("apps/main/Cargo.toml")).unwrap();
    put_json(
        root,
        "packages/extra/package.json",
        &manifest("extra", json!({}), true),
    );
    assert!(
        assert_native_components(root, &state)
            .unwrap_err()
            .to_string()
            .contains("Unregistered native component")
    );
    fs::remove_dir_all(root.join("packages/extra")).unwrap();
    let mut package: Value = read_json(&root.join("apps/main/package.json")).unwrap();
    package["name"] = json!("renamed");
    put_json(root, "apps/main/package.json", &package);
    assert!(
        assert_native_components(root, &state)
            .unwrap_err()
            .to_string()
            .contains("identity projection is stale")
    );
}

#[test]
fn admitted_snapshot_rechecks_raw_manifests_and_optional_config_presence() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[("apps/main", manifest("main", json!({}), false))],
        json!({}),
    );
    let admitted = admit_native_components(root, &state).unwrap();
    admitted.recheck(root).unwrap();
    put(root, "apps/main/.npmrc", "fund=false\n");
    assert!(
        admitted
            .recheck(root)
            .unwrap_err()
            .to_string()
            .contains("Native input changed")
    );
    fs::remove_file(root.join("apps/main/.npmrc")).unwrap();
    admitted.recheck(root).unwrap();
    put(root, "apps/main/package.json", "{\"name\":\"different\"}\n");
    assert!(
        admitted
            .recheck(root)
            .unwrap_err()
            .to_string()
            .contains("Native input changed")
    );
}

#[test]
fn cargo_inherited_dependency_can_reference_an_explicit_checkout_root_workspace() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/api", "rust"), ("packages/common", "rust")]);
    put(
        root,
        "Cargo.toml",
        "[workspace]\nmembers=[\"apps/api\",\"packages/common\"]\n[workspace.dependencies]\ncommon={path=\"packages/common\"}\n",
    );
    rust(
        root,
        "apps/api",
        "[package]\nname=\"api\"\nversion=\"1.0.0\"\nworkspace=\"../..\"\n[dependencies]\ncommon={workspace=true}\n",
        true,
    );
    rust(
        root,
        "packages/common",
        "[package]\nname=\"common\"\nversion=\"1.0.0\"\n",
        true,
    );
    let inputs = collect_native_input_closure(root, &state).unwrap();
    assert!(inputs.files["Cargo.toml"].is_some());
    assert_eq!(
        derive_component_dependencies(root, &state).unwrap()["apps/api"],
        vec!["packages/common"]
    );
}

#[test]
fn cargo_closure_follows_paths_targets_workspace_members_and_ancestor_configs() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/api", "rust")]);
    rust(
        root,
        "apps/api",
        "[package]\nname=\"api\"\nversion=\"1.0.0\"\n[workspace]\nmembers=[\"nested\"]\n[workspace.dependencies]\ncommon={path=\"../../rust vendor/common\"}\n[dependencies]\ndirect={path=\"../../rust vendor/direct\"}\n[target.'cfg(unix)'.dependencies]\nplatform={path=\"../../rust vendor/platform\"}\n",
        true,
    );
    rust(
        root,
        "apps/api/nested",
        "[package]\nname=\"nested\"\nversion=\"1.0.0\"\n[dependencies]\ncommon={workspace=true}\n",
        false,
    );
    for name in ["common", "direct", "platform"] {
        rust(
            root,
            &format!("rust vendor/{name}"),
            &format!("[package]\nname=\"{name}\"\nversion=\"1.0.0\"\n"),
            false,
        );
    }
    let first = collect_native_input_closure(root, &state).unwrap();
    for name in [
        "rust vendor/common/Cargo.toml",
        "rust vendor/direct/Cargo.toml",
        "rust vendor/platform/Cargo.toml",
        "apps/api/nested/Cargo.toml",
    ] {
        assert!(first.files[name].is_some());
    }
    assert_eq!(first.files[".cargo/config.toml"], None);
    put(root, ".cargo/config.toml", "[net]\noffline=true\n");
    assert_ne!(
        first.digest,
        collect_native_input_closure(root, &state).unwrap().digest
    );
    put(
        root,
        "rust vendor/common/.cargo/config.toml",
        "[build]\njobs=1\n",
    );
    assert_ne!(
        first.digest,
        collect_native_input_closure(root, &state).unwrap().digest
    );
}

fn graph_fixture(root: &Path) -> ProjectState {
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[(
            "apps/main",
            manifest("main", json!({"a":"1.0.0","b":"1.0.0"}), false),
        )],
        json!({
            "node_modules/a":artifact("a",json!({"dependencies":{"shared":"1.0.0"}})),
            "node_modules/b":artifact("b",json!({"dependencies":{"shared":"1.0.0"}})),
            "node_modules/shared":artifact("shared",json!({}))
        }),
    );
    state
}

#[test]
fn preserved_graph_allows_hoisting_relocation_and_rejects_artifact_edge_or_sharing_drift() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = graph_fixture(root);
    let before = capture_product_graph(root, &state).unwrap();
    let original: Value = read_json(&root.join("package-lock.json")).unwrap();
    let mut relocated = original.clone();
    let a = relocated["packages"]
        .as_object_mut()
        .unwrap()
        .remove("node_modules/a")
        .unwrap();
    relocated["packages"]["apps/main/node_modules/a"] = a;
    put_json(root, "package-lock.json", &relocated);
    assert_product_graphs_preserved(&before, &read_native_graph(root).unwrap()).unwrap();
    let mut changed = relocated.clone();
    changed["packages"]["node_modules/shared"]["integrity"] = json!(sri(9));
    put_json(root, "package-lock.json", &changed);
    assert!(
        assert_product_graphs_preserved(&before, &read_native_graph(root).unwrap())
            .unwrap_err()
            .to_string()
            .contains("artifact changed")
    );
    let mut split = relocated.clone();
    split["packages"]["node_modules/b/node_modules/shared"] =
        split["packages"]["node_modules/shared"].clone();
    put_json(root, "package-lock.json", &split);
    assert!(
        assert_product_graphs_preserved(&before, &read_native_graph(root).unwrap())
            .unwrap_err()
            .to_string()
            .contains("shared dependency split")
    );
    let mut topology = relocated;
    topology["packages"]["node_modules/b"]["dependencies"]["extra"] = json!("1.0.0");
    topology["packages"]["node_modules/extra"] = artifact("extra", json!({}));
    put_json(root, "package-lock.json", &topology);
    assert!(
        assert_product_graphs_preserved(&before, &read_native_graph(root).unwrap())
            .unwrap_err()
            .to_string()
            .contains("topology changed")
    );
}

#[test]
fn preserved_graph_accepts_identical_artifact_deduplication() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = graph_fixture(root);
    let mut lock: Value = read_json(&root.join("package-lock.json")).unwrap();
    let shared = lock["packages"]
        .as_object_mut()
        .unwrap()
        .remove("node_modules/shared")
        .unwrap();
    lock["packages"]["node_modules/a/node_modules/shared"] = shared.clone();
    lock["packages"]["node_modules/b/node_modules/shared"] = shared.clone();
    put_json(root, "package-lock.json", &lock);
    let before = capture_product_graph(root, &state).unwrap();
    lock["packages"]
        .as_object_mut()
        .unwrap()
        .remove("node_modules/a/node_modules/shared");
    lock["packages"]
        .as_object_mut()
        .unwrap()
        .remove("node_modules/b/node_modules/shared");
    lock["packages"]["node_modules/shared"] = shared;
    put_json(root, "package-lock.json", &lock);
    assert_product_graphs_preserved(&before, &read_native_graph(root).unwrap()).unwrap();
}

#[test]
fn preserved_graph_distinguishes_absent_null_false_artifact_flags() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = graph_fixture(root);
    let original: Value = read_json(&root.join("package-lock.json")).unwrap();
    let before = capture_product_graph(root, &state).unwrap();
    for flag in [Value::Null, Value::Bool(false), Value::Bool(true)] {
        let mut lock = original.clone();
        lock["packages"]["node_modules/shared"]["optional"] = flag;
        put_json(root, "package-lock.json", &lock);
        assert!(
            assert_product_graphs_preserved(&before, &read_native_graph(root).unwrap())
                .unwrap_err()
                .to_string()
                .contains("artifact changed")
        );
    }
}

#[test]
fn seed_refreshes_native_identity_links_and_preserves_unknown_fields() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    let mut member = manifest("old-name", json!({"product":"1.0.0"}), false);
    member["custom"] = json!({"retained":true});
    node_project(
        root,
        &state,
        &[("apps/main", member.clone())],
        json!({"node_modules/product":artifact("product",json!({}))}),
    );
    let original: Value = read_json(&root.join("package-lock.json")).unwrap();
    member["name"] = json!("new-name");
    member["version"] = json!("2.0.0");
    put_json(root, "apps/main/package.json", &member);
    seed_workspace_lock(
        root,
        &state,
        &[LockSeed {
            prefix: String::new(),
            lock: original,
        }],
    )
    .unwrap();
    let lock: Value = read_json(&root.join("package-lock.json")).unwrap();
    assert_eq!(
        lock["packages"]["apps/main"]["custom"],
        json!({"retained":true})
    );
    assert_eq!(lock["packages"]["apps/main"]["version"], "2.0.0");
    assert!(lock["packages"].get("node_modules/old-name").is_none());
    assert_eq!(
        lock["packages"]["node_modules/new-name"]["resolved"],
        "apps/main"
    );
    assert!(lock["packages"].get("node_modules/product").is_some());
}
