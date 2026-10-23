use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    files::{read_json, write_atomic, write_json},
    graph::{
        LockSeed, assert_product_graphs_preserved, capture_product_graph, read_native_graph,
        seed_workspace_lock,
    },
    model::{ComponentState, Environment, PackageMode, ProjectState, Release},
    native::{
        admit_native_components, assert_native_components, assert_npmrc_compatibility,
        collect_native_input_closure, derive_component_dependencies, package_lock, valid_integrity,
    },
    project::{InitOptions, LockResolver, UpdateOptions, init_project, update_project},
    release::AdmittedRelease,
    state::{admit_runtime_project, source_signature},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

// Small sealed release fixture exercises the real public project/runtime APIs.
// The controlled lock boundary below is not an npm resolution proof.
fn runtime_release(root: &Path) -> std::path::PathBuf {
    use devkit::files::file_map;
    use sha2::{Digest, Sha512};
    let release = root.join("release");
    let version = "0.3.0";
    for name in ["devkit.mk", "devkit.sh", "compose.tools.yaml"] {
        put(
            &release,
            &format!("runtime/{name}"),
            &format!("fixture {name}\n"),
        );
    }
    put(
        &release,
        "runtime/toolchains.env",
        "DEVKIT_VERSION=0.3.0\nDEVKIT_RUST_VERSION=1.99.1\n",
    );
    put(
        &release,
        "npm/env.tgz",
        "sealed archive fixture, not an npm installation proof",
    );
    let environment_integrity = format!(
        "sha512-{}",
        STANDARD.encode(Sha512::digest(
            fs::read(release.join("npm/env.tgz")).unwrap()
        ))
    );
    let mut node = manifest("fixture-node", json!({}), false);
    node["devDependencies"] = json!({"@docouno/env":version});
    put_json(&release, "templates/node-api/package.json", &node);
    put_json(
        &release,
        "templates/node-api/package-lock.json",
        &json!({"lockfileVersion":3,"packages":{
            "":node,"node_modules/@docouno/env":{"version":version,"resolved":"https://registry.npmjs.org/@docouno/env/-/env-0.3.0.tgz","integrity":environment_integrity}
        }}),
    );
    put(
        &release,
        "templates/node-api/src/index.ts",
        "export const value = 1;\n",
    );
    rust(
        &release,
        "templates/rust-api",
        "[package]\nname=\"fixture-api\"\nversion=\"1.0.0\"\nedition=\"2024\"\n",
        true,
    );
    put(&release, "templates/rust-api/src/main.rs", "fn main() {}\n");
    write_json(
        &release.join("release.json"),
        &Release {
            schema_version: 1,
            version: version.into(),
            rust_version: "1.99.1".into(),
            environment: Environment {
                name: "@docouno/env".into(),
                version: version.into(),
                archive: "npm/env.tgz".into(),
                integrity: environment_integrity,
            },
            files: file_map(&release).unwrap(),
        },
    )
    .unwrap();
    release
}
struct SeedFixtureResolver;
impl LockResolver for SeedFixtureResolver {
    fn regenerate(&self, root: &Path, _: &AdmittedRelease, _: Option<&str>) -> devkit::Result<()> {
        // The fixture release supplies the full selected graph. Admit it rather
        // than implementing an alternate ecosystem resolver in the test.
        package_lock(root)?;
        Ok(())
    }
}
struct RefuseResolver;
impl LockResolver for RefuseResolver {
    fn regenerate(&self, _: &Path, _: &AdmittedRelease, _: Option<&str>) -> devkit::Result<()> {
        anyhow::bail!("Admission must reject this input before requesting npm resolution")
    }
}
fn runtime_project(
    root: &Path,
    template: &str,
) -> (std::path::PathBuf, std::path::PathBuf, ProjectState) {
    let release = runtime_release(root);
    let project = root.join("consumer");
    let state = init_project(
        &release,
        &project,
        &InitOptions {
            template: Some(template.into()),
            package_mode: Some(PackageMode::Registry),
            name: Some("native-fixture".into()),
            ..Default::default()
        },
        &SeedFixtureResolver,
    )
    .unwrap();
    (release, project, state)
}

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
fn checkout_root_package_source_has_an_explicit_boundary_but_component_dot_is_valid() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/api", "rust")]);
    rust(
        root,
        "apps/api",
        "[package]\nname='api'\nversion='1.0.0'\n[patch.crates-io]\napi={path='.'}\n",
        true,
    );
    collect_native_input_closure(root, &state).unwrap();
    put(
        root,
        "Cargo.toml",
        "[package]\nname='root-source'\nversion='1.0.0'\n",
    );
    put(
        root,
        "apps/api/Cargo.toml",
        "[package]\nname='api'\nversion='1.0.0'\n[patch.crates-io]\nroot-source={path='../..'}\n",
    );
    let error = collect_native_input_closure(root, &state)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Unsupported checkout-root local package location"),
        "{error}"
    );
    assert!(
        !error.contains("escapes"),
        "A confined but unsupported package root must not be reported as an escape"
    );
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

#[test]
fn required_npm_targets_reject_root_member_transitive_and_same_release_noop() {
    for location in ["root", "member", "transitive", "development", "peer"] {
        let f = TempDir::new().unwrap();
        let (release, root, state) = runtime_project(f.path(), "node");
        let mut lock: Value = read_json(&root.join("package-lock.json")).unwrap();
        let mut member: Value = read_json(&root.join("apps/main/package.json")).unwrap();
        if location == "root" {
            let mut manifest: Value = read_json(&root.join("package.json")).unwrap();
            manifest["dependencies"] = json!({"missing-required":"1.0.0"});
            put_json(&root, "package.json", &manifest);
            lock["packages"][""] = manifest;
        } else if location == "transitive" {
            member["dependencies"] = json!({"parent":"1.0.0"});
            lock["packages"]["node_modules/parent"] = artifact(
                "parent",
                json!({"dependencies":{"missing-required":"1.0.0"}}),
            );
        } else {
            let field = match location {
                "development" => "devDependencies",
                "peer" => "peerDependencies",
                _ => "dependencies",
            };
            if member.get(field).is_none() {
                member[field] = json!({});
            }
            member[field]["missing-required"] = json!("1.0.0");
        }
        put_json(&root, "apps/main/package.json", &member);
        lock["packages"]["apps/main"] = member;
        put_json(&root, "package-lock.json", &lock);
        let before = source_signature(&root, Some(&state)).unwrap();
        assert!(
            admit_native_components(&root, &state)
                .err()
                .unwrap()
                .to_string()
                .contains("Native lock omits dependency"),
            "{location}"
        );
        assert!(
            admit_runtime_project(&root, "0.3.0")
                .err()
                .unwrap()
                .to_string()
                .contains("Native lock omits dependency"),
            "{location}"
        );
        let error = update_project(&release, &root, &UpdateOptions::default(), &RefuseResolver)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("Native lock omits dependency"),
            "{location}: {error}"
        );
        assert_eq!(source_signature(&root, Some(&state)).unwrap(), before);
        let after = devkit::state::read_project_state(&root).unwrap();
        assert_eq!(after.generation, state.generation);
    }
}

#[test]
fn npm_missing_optional_and_optional_peer_rules_match_graph_admission() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    let mut member = manifest("main", json!({"optional":"1.0.0"}), false);
    member["optionalDependencies"] = json!({"optional":"1.0.0","missing-optional":"1.0.0"});
    member["peerDependencies"] = json!({"optional-peer":"1.0.0"});
    member["peerDependenciesMeta"] = json!({"optional-peer":{"optional":true}});
    node_project(root, &state, &[("apps/main", member.clone())], json!({}));
    admit_native_components(root, &state).unwrap();
    read_native_graph(root).unwrap();
    for metadata in [
        json!({}),
        json!({"optional-peer":null}),
        json!({"optional-peer":{"optional":false}}),
    ] {
        member["peerDependenciesMeta"] = metadata;
        put_json(root, "apps/main/package.json", &member);
        let mut lock: Value = read_json(&root.join("package-lock.json")).unwrap();
        lock["packages"]["apps/main"] = member.clone();
        put_json(root, "package-lock.json", &lock);
        assert!(
            admit_native_components(root, &state)
                .err()
                .unwrap()
                .to_string()
                .contains("optional-peer")
        );
        assert!(
            read_native_graph(root)
                .unwrap_err()
                .to_string()
                .contains("optional-peer")
        );
    }
}

#[test]
fn runtime_snapshot_refuses_late_alternate_manifests_and_namespace_changes() {
    for template in ["node", "rust"] {
        let f = TempDir::new().unwrap();
        let (_, root, state) = runtime_project(f.path(), template);
        let (path, alternate) = if template == "node" {
            ("apps/main", "Cargo.toml")
        } else {
            ("apps/api", "package.json")
        };
        let admitted = admit_runtime_project(&root, "0.3.0").unwrap();
        assert_eq!(
            admitted.native.inputs.files[&format!("{path}/{alternate}")],
            None
        );
        let before = admitted.native.inputs.digest.clone();
        put(
            &root,
            &format!("{path}/{alternate}"),
            if alternate == "package.json" {
                "{}\n"
            } else {
                "[package]\nname=\"competing\"\nversion=\"1.0.0\"\n"
            },
        );
        assert!(admitted.recheck().is_err());
        assert!(
            admit_runtime_project(&root, "0.3.0")
                .err()
                .unwrap()
                .to_string()
                .contains("Ambiguous native manifests")
        );
        assert_ne!(
            before,
            collect_native_input_closure(&root, &state).unwrap().digest
        );
        fs::remove_file(root.join(path).join(alternate)).unwrap();
        admitted.recheck().unwrap();
        assert!(!root.join("packages").exists());
        put_json(
            &root,
            "packages/extra/package.json",
            &manifest("extra", json!({}), true),
        );
        assert!(
            admitted
                .recheck()
                .unwrap_err()
                .to_string()
                .contains("namespace changed")
        );
        assert!(
            admit_runtime_project(&root, "0.3.0")
                .err()
                .unwrap()
                .to_string()
                .contains("Unregistered native component")
        );
    }
    let f = TempDir::new().unwrap();
    let (_, root, state) = runtime_project(f.path(), "node");
    put(
        &root,
        "packages/notes/readme.txt",
        "ordinary project data\n",
    );
    let admitted = admit_runtime_project(&root, "0.3.0").unwrap();
    assert_eq!(
        admitted.native.inputs.files["packages/notes/Cargo.toml"],
        None
    );
    put(
        &root,
        "packages/notes/Cargo.toml",
        "[package]\nname=\"unregistered\"\nversion=\"1.0.0\"\n",
    );
    assert!(admitted.recheck().is_err());
    assert_ne!(
        admitted.native.inputs.digest,
        collect_native_input_closure(&root, &state).unwrap().digest
    );
    assert!(
        admit_runtime_project(&root, "0.3.0")
            .err()
            .unwrap()
            .to_string()
            .contains("Unregistered native component")
    );
}

fn override_crate(root: &Path, path: &str, name: &str) {
    rust(
        root,
        path,
        &format!("[package]\nname=\"{name}\"\nversion=\"1.0.0\"\nedition=\"2024\"\n"),
        false,
    );
    put(
        root,
        &format!("{path}/src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    );
}

#[test]
fn cargo_patch_replace_and_config_override_closures_bind_paths_and_transitive_inputs() {
    for form in [
        "patch",
        "replace",
        "config-patch",
        "config-paths",
        "config-include",
        "workspace-patch",
    ] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let state = state(&[("apps/api", "rust")]);
        let mut manifest = "[package]\nname=\"api\"\nversion=\"1.0.0\"\n".to_owned();
        match form {
            "patch" => {
                manifest.push_str("[patch.crates-io]\npatched={path=\"../../vendor/patched\"}\n")
            }
            "replace" => {
                manifest.push_str("[replace]\n\"patched:1.0.0\"={path=\"../../vendor/patched\"}\n")
            }
            "config-patch" => put(
                root,
                ".cargo/config.toml",
                "[patch.crates-io]\npatched={path=\"vendor/patched\"}\n",
            ),
            "config-paths" => put(root, ".cargo/config.toml", "paths=[\"vendor/patched\"]\n"),
            "config-include" => {
                put(
                    root,
                    ".cargo/config.toml",
                    "include=[\"patches.toml\",{path=\"optional.toml\",optional=true}]\n",
                );
                put(
                    root,
                    ".cargo/patches.toml",
                    "[patch.crates-io]\npatched={path=\"vendor/patched\"}\n",
                );
            }
            "workspace-patch" => put(
                root,
                "Cargo.toml",
                "[workspace]\nmembers=[\"apps/api\"]\n[patch.crates-io]\npatched={path=\"vendor/patched\"}\n",
            ),
            _ => unreachable!(),
        }
        rust(root, "apps/api", &manifest, true);
        override_crate(root, "vendor/patched", "patched");
        put(
            root,
            "vendor/patched/Cargo.toml",
            "[package]\nname=\"patched\"\nversion=\"1.0.0\"\n[dependencies]\nleaf={path=\"../leaf\"}\n",
        );
        override_crate(root, "vendor/leaf", "leaf");
        profiles(root, &state);
        let admitted = admit_native_components(root, &state).unwrap();
        assert!(
            admitted.inputs.files["vendor/patched/Cargo.toml"].is_some(),
            "{form}"
        );
        assert!(
            admitted.inputs.files["vendor/leaf/Cargo.toml"].is_some(),
            "{form}"
        );
        assert_eq!(
            admitted.inputs.files["vendor/patched/.cargo/config.toml"], None,
            "{form}"
        );
        put(root, "vendor/leaf/.cargo/config.toml", "[build]\njobs=1\n");
        assert!(admitted.recheck(root).is_err(), "{form}");
        fs::remove_file(root.join("vendor/leaf/.cargo/config.toml")).unwrap();
        admitted.recheck(root).unwrap();
        put(
            root,
            "vendor/patched/Cargo.toml",
            "[package]\nname=\"patched\"\nversion=\"1.0.0\"\nedition=\"2021\"\n",
        );
        assert!(admitted.recheck(root).is_err(), "{form}");
        assert_ne!(
            admitted.inputs.digest,
            collect_native_input_closure(root, &state).unwrap().digest,
            "{form}"
        );
    }
}

#[test]
fn cargo_override_paths_reject_escape_symlink_and_unmodelled_local_stores() {
    for form in ["patch", "replace", "paths", "config-patch", "include"] {
        let f = TempDir::new().unwrap();
        let root = f.path().join("checkout");
        let state = state(&[("apps/api", "rust")]);
        rust(
            &root,
            "apps/api",
            "[package]\nname=\"api\"\nversion=\"1.0.0\"\n",
            true,
        );
        override_crate(f.path(), "outside", "outside");
        profiles(&root, &state);
        match form {
            "patch" => put(
                &root,
                "apps/api/Cargo.toml",
                "[package]\nname=\"api\"\nversion=\"1.0.0\"\n[patch.crates-io]\noutside={path=\"../../../outside\"}\n",
            ),
            "replace" => put(
                &root,
                "apps/api/Cargo.toml",
                "[package]\nname=\"api\"\nversion=\"1.0.0\"\n[replace]\n\"outside:1.0.0\"={path=\"../../../outside\"}\n",
            ),
            "paths" => put(&root, ".cargo/config.toml", "paths=[\"../outside\"]\n"),
            "config-patch" => put(
                &root,
                ".cargo/config.toml",
                "[patch.crates-io]\noutside={path=\"../outside\"}\n",
            ),
            "include" => put(
                &root,
                ".cargo/config.toml",
                "include=[\"../../outside.toml\"]\n",
            ),
            _ => unreachable!(),
        }
        assert!(admit_native_components(&root, &state).is_err(), "{form}");
    }
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/api", "rust")]);
    rust(
        root,
        "apps/api",
        "[package]\nname=\"api\"\nversion=\"1.0.0\"\n[patch.crates-io]\nlinked={path=\"../../linked\"}\n",
        true,
    );
    profiles(root, &state);
    override_crate(root, "actual", "linked");
    std::os::unix::fs::symlink(root.join("actual"), root.join("linked")).unwrap();
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("symlink")
    );
    put(
        root,
        "apps/api/Cargo.toml",
        "[package]\nname=\"api\"\nversion=\"1.0.0\"\n",
    );
    for config in [
        "[source.vendored]\ndirectory=\"vendor\"\n",
        "[source.local]\nlocal-registry=\"registry\"\n",
        "[patch.crates-io]\nlocal={git=\"file:///outside\"}\n",
        "[resolver]\nlockfile-path=\"custom.lock\"\n",
    ] {
        put(root, ".cargo/config.toml", config);
        assert!(
            admit_native_components(root, &state)
                .err()
                .unwrap()
                .to_string()
                .contains("Unsupported"),
            "{config}"
        );
    }
    put(
        root,
        ".cargo/config.toml",
        "[build]\njobs=1\n[patch.crates-io]\nremote={git=\"https://example.invalid/repository\",rev=\"abc\"}\n",
    );
    admit_native_components(root, &state).unwrap();
}

#[test]
#[ignore = "requires Cargo inside the pinned Rust 1.99.1 validation container"]
fn pinned_cargo_metadata_proves_patch_inputs_and_outside_refusal() {
    fn metadata(root: &Path) -> Value {
        let version = std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&version.stdout).starts_with("cargo 1.99.1 "));
        let output = std::process::Command::new("cargo")
            .args([
                "metadata",
                "--offline",
                "--format-version",
                "1",
                "--manifest-path",
                "Cargo.toml",
            ])
            .current_dir(root.join("apps/api"))
            // The devkit build has already cached its exact dependencies. The
            // registry replacement case uses that immutable offline input.
            .env("CARGO_HOME", std::env::var_os("CARGO_HOME").unwrap())
            .env(
                "CARGO_TARGET_DIR",
                root.parent().unwrap().join("cargo-target"),
            )
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    let mut proof = Vec::new();
    let build_lock: toml::Value = toml::from_str(
        &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"))
            .unwrap(),
    )
    .unwrap();
    let anyhow_version = build_lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some("anyhow"))
        .unwrap()["version"]
        .as_str()
        .unwrap();
    for form in [
        "manifest-patch",
        "config-include",
        "replace",
        "outside-patch",
    ] {
        let f = TempDir::new().unwrap();
        let root = f.path().join("checkout");
        let state = state(&[("apps/api", "rust")]);
        let mut manifest =
            "[package]\nname=\"api\"\nversion=\"1.0.0\"\nedition=\"2024\"\n".to_owned();
        let selected = if form == "outside-patch" {
            f.path().join("outside/Cargo.toml")
        } else {
            root.join("vendor/patched-local/Cargo.toml")
        };
        let selected_name = if form == "replace" {
            "anyhow"
        } else {
            "patched-local"
        };
        match form {
            "manifest-patch"=>manifest.push_str("[dependencies]\npatched-local=\"1.0.0\"\n[patch.crates-io]\npatched-local={path=\"../../vendor/patched-local\"}\n"),
            "config-include"=>{
                manifest.push_str("[dependencies]\npatched-local=\"1.0.0\"\n");
                put(&root,".cargo/config.toml","include=[\"patches.toml\"]\n");
                put(&root,".cargo/patches.toml","[patch.crates-io]\npatched-local={path=\"vendor/patched-local\"}\n");
            }
            "replace"=>{
                manifest.push_str(&format!("[dependencies]\nanyhow=\"={anyhow_version}\"\n[replace]\n\"anyhow:{anyhow_version}\"={{path=\"../../vendor/patched-local\"}}\n"));
            }
            "outside-patch"=>manifest.push_str("[dependencies]\npatched-local=\"1.0.0\"\n[patch.crates-io]\npatched-local={path=\"../../../outside\"}\n"),
            _=>unreachable!(),
        }
        rust(&root, "apps/api", &manifest, true);
        put(&root, "apps/api/src/main.rs", "fn main() {}\n");
        profiles(&root, &state);
        if form == "outside-patch" {
            override_crate(f.path(), "outside", "patched-local");
        } else {
            override_crate(&root, "vendor/patched-local", selected_name);
            if form == "replace" {
                put(
                    &root,
                    "vendor/patched-local/Cargo.toml",
                    &format!(
                        "[package]\nname=\"anyhow\"\nversion=\"{anyhow_version}\"\nedition=\"2024\"\n"
                    ),
                );
            }
        }
        let native = metadata(&root);
        let package = native["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                p["name"] == selected_name && p["manifest_path"] == selected.to_str().unwrap()
            })
            .unwrap_or_else(|| {
                panic!(
                    "{form}: Cargo selected manifests: {:?}",
                    native["packages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| &p["manifest_path"])
                        .collect::<Vec<_>>()
                )
            });
        if form == "outside-patch" {
            assert!(
                admit_native_components(&root, &state)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("escapes the project")
            );
            proof.push(json!({"form":form,"cargoSelectedManifest":package["manifest_path"],"nativeAdmission":"rejected-checkout-escape"}));
        } else {
            let admitted = admit_native_components(&root, &state).unwrap();
            assert!(admitted.inputs.files["vendor/patched-local/Cargo.toml"].is_some());
            let lock = fs::read(root.join("apps/api/Cargo.lock")).unwrap();
            let old = fs::read_to_string(&selected).unwrap();
            put(
                &root,
                "vendor/patched-local/Cargo.toml",
                &old.replace("2024", "2021"),
            );
            assert!(admitted.recheck(&root).is_err());
            assert_ne!(
                admitted.inputs.digest,
                collect_native_input_closure(&root, &state).unwrap().digest
            );
            assert_eq!(fs::read(root.join("apps/api/Cargo.lock")).unwrap(), lock);
            proof.push(json!({"form":form,"cargoSelectedManifest":package["manifest_path"],"nativeAdmission":"tracked-and-edit-invalidates","lockUnchangedAfterEditionEdit":true}));
        }
    }
    if let Ok(directory) = std::env::var("DEVKIT_PROOF_DIR") {
        write_json(&Path::new(&directory).join("cargo-metadata-proof.json"),&json!({
            "toolchain":"1.99.1","offline":true,"cases":proof,
            "sources":["https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html","https://doc.rust-lang.org/cargo/reference/config.html#config-relative-paths"],
            "unsupportedLocalForms":["source.directory","source.local-registry","git=file:","resolver.lockfile-path"]
        })).unwrap();
    }
}
