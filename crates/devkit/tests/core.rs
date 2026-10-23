use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    compose::{ComposeNormalizer, admit_compose_inventory, capture_compose_inputs, parse_compose},
    files::*,
    model::*,
    project::*,
    release::*,
    state::*,
    transaction::{MutationHooks, read_journal},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn put(root: &Path, name: &str, bytes: &[u8]) {
    write_atomic(&root.join(name), bytes, 0o644).unwrap();
}
fn put_json(root: &Path, name: &str, value: &Value) {
    write_json(&root.join(name), value).unwrap();
}
fn sri(bytes: &[u8]) -> String {
    format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes)))
}
fn seal(root: &Path, version: &str) {
    remove_file(&root.join("release.json")).unwrap();
    let archive = format!("npm/env-{version}.tgz");
    write_json(
        &root.join("release.json"),
        &Release {
            schema_version: 1,
            version: version.to_string(),
            rust_version: "1.99.1".to_string(),
            environment: Environment {
                name: "@docouno/env".to_string(),
                version: version.to_string(),
                archive: archive.clone(),
                integrity: sri(&fs::read(root.join(archive)).unwrap()),
            },
            files: file_map(root).unwrap(),
        },
    )
    .unwrap();
}
fn fixture_release(parent: &Path, version: &str) -> PathBuf {
    let root = parent.join(format!("release-{version}"));
    fs::create_dir(&root).unwrap();
    for name in ["devkit.mk", "devkit.sh", "compose.tools.yaml"] {
        put(
            &root,
            &format!("runtime/{name}"),
            format!("fixture {name} {version}\n").as_bytes(),
        );
    }
    if version.starts_with("0.2.") {
        put(&root, "runtime/project.mjs", b"legacy managed controller\n");
    }
    put(&root, "runtime/toolchains.env", format!("DEVKIT_VERSION={version}\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\n").as_bytes());
    // These tests prove engine identity and transactions, not npm installation.
    put(
        &root,
        &format!("npm/env-{version}.tgz"),
        format!("env archive fixture {version}").as_bytes(),
    );
    for (name, library) in [
        ("node-api", false),
        ("react-web", false),
        ("electron-react", false),
        ("react-client", true),
    ] {
        let mut manifest = json!({"name":format!("fixture-{name}"),"version":"1.0.0","private":true,
            "devDependencies":{"@docouno/env":version},"scripts":{"check":"check","fix":"fix","test":"test","build":"build","dev":"dev"}});
        if library {
            manifest["exports"] = json!({".":"./src/index.ts"});
            manifest["scripts"].as_object_mut().unwrap().remove("dev");
        }
        put_json(&root, &format!("templates/{name}/package.json"), &manifest);
        put_json(
            &root,
            &format!("templates/{name}/package-lock.json"),
            &json!({"lockfileVersion":3,"packages":{"":manifest,"node_modules/@docouno/env":{"version":version,"resolved":format!("https://registry.npmjs.org/@docouno/env/-/env-{version}.tgz"),"integrity":sri(format!("env archive fixture {version}").as_bytes()),"dev":true}}}),
        );
        put(
            &root,
            &format!("templates/{name}/src/index.ts"),
            b"export const answer = 42;\n",
        );
    }
    for (name, bytes) in [
        (
            "Cargo.toml",
            "[package]\nname = \"fixture-api\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        ),
        ("Cargo.lock", "version = 4\n"),
        ("rustfmt.toml", "edition = \"2024\"\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.99.1\"\n"),
        ("src/main.rs", "fn main() {}\n"),
    ] {
        put(
            &root,
            &format!("templates/rust-api/{name}"),
            bytes.as_bytes(),
        );
    }
    seal(&root, version);
    root
}
struct NoResolver;
impl LockResolver for NoResolver {
    fn regenerate(&self, _: &Path, _: &AdmittedRelease, _: Option<&str>) -> Result<()> {
        bail!("Rust-only operations must not start npm")
    }
}
// A controlled external native-tool boundary for failure/CAS tests. Graph
// selection behavior itself is independently exercised in tests/native.rs.
struct FixtureResolver;
impl LockResolver for FixtureResolver {
    fn regenerate(&self, root: &Path, release: &AdmittedRelease, _: Option<&str>) -> Result<()> {
        let mut lock: Value = read_json(&root.join("package-lock.json"))?;
        let manifest: Value = read_json(&root.join("package.json"))?;
        lock["packages"][""] = manifest.clone();
        for path in manifest["workspaces"].as_array().unwrap() {
            let path = path.as_str().unwrap();
            let member: Value = read_json(&root.join(path).join("package.json"))?;
            lock["packages"][path] = member;
        }
        let local = manifest["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| {
                lock["packages"][p.as_str().unwrap()]["devDependencies"]["@docouno/env"].as_str()
            })
            .any(|v| v.starts_with("file:"));
        lock["packages"]["node_modules/@docouno/env"] = json!({"version":release.manifest.version,"resolved":if local {format!("file:.devkit/npm/env-{}.tgz",release.manifest.version)} else {format!("https://registry.npmjs.org/@docouno/env/-/env-{}.tgz",release.manifest.version)},"integrity":release.manifest.environment.integrity,"dev":true});
        write_json(&root.join("package-lock.json"), &lock)
    }
}
fn init(root: &Path, target: &Path, template: &str, mode: PackageMode) -> ProjectState {
    init_project(
        root,
        target,
        &InitOptions {
            template: Some(template.to_string()),
            package_mode: Some(mode),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap()
}
fn preserved(root: &Path, names: &[&str]) -> BTreeMap<String, Vec<u8>> {
    names
        .iter()
        .map(|n| (n.to_string(), fs::read(root.join(n)).unwrap()))
        .collect()
}

#[test]
fn release_admission_is_exact_and_npm_import_normalizes_only_modes() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let admitted = admit_release(&release).unwrap();
    let raw = fs::read(release.join("release.json")).unwrap();
    assert_eq!(admitted.identity, digest(&raw));
    put(&release, "extra", b"unknown");
    assert!(admit_release(&release).is_err());
    fs::remove_file(release.join("extra")).unwrap();
    let source = f.path().join("npm");
    copy_tree(&release, &source, &|_| false, false).unwrap();
    for name in file_map(&source).unwrap().keys() {
        fs::set_permissions(source.join(name), fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(admit_release(&source).is_err());
    let canonical = f.path().join("canonical");
    let imported = import_npm_payload(&source, &canonical).unwrap();
    assert_eq!(imported.identity, admitted.identity);
    put(&source, "npm/env-0.3.0.tgz", b"changed env bytes");
    assert!(import_npm_payload(&source, &f.path().join("rejected")).is_err());
    assert!(!f.path().join("rejected").exists());
    let mut forged: Value = read_json(&release.join("release.json")).unwrap();
    put(&release, "npm/env-0.3.0.tgz", b"new env archive");
    forged["files"]["npm/env-0.3.0.tgz"]["sha256"] = json!(digest(b"new env archive"));
    put_json(&release, "release.json", &forged);
    assert!(
        admit_release(&release)
            .unwrap_err()
            .to_string()
            .contains("Environment archive integrity")
    );
}

#[test]
fn public_schema2_update_preserves_unknown_fields_and_product_bytes() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("rust product with spaces");
    init(&old, &target, "rust", PackageMode::Local);
    let mut state: Value = read_json(&target.join(MANIFEST_PATH)).unwrap();
    state["future"] = json!({"flag":false,"n":null});
    state["components"]["apps/api"]["future"] = json!([1, 2]);
    state["components"]["apps/api"]["origin"]["ticket"] = json!("kept");
    state["components"]["apps/api"]["development"]["custom"] = json!({"n":3});
    put_json(&target, MANIFEST_PATH, &state);
    put(
        &target,
        "apps/api/src/main.rs",
        b"fn main() { println!(\"product source\"); }\n",
    );
    put(&target,"apps/api/Cargo.toml",b"[package]\nname=\"custom\"\nversion=\"2.3.4\"\nedition=\"2024\"\n[package.metadata.product]\nfeature=true\n");
    put(
        &target,
        "Makefile",
        b"include .devkit/runtime/devkit.mk\nbuild-api: custom\ncustom:\n\t@true\n",
    );
    put(
        &target,
        "compose.yaml",
        b"services:\n  cache:\n    image: redis:alpine\n",
    );
    put(&target, ".env", b"PRODUCT=true\n");
    let names = [
        "apps/api/src/main.rs",
        "apps/api/Cargo.toml",
        "Makefile",
        "compose.yaml",
        ".env",
    ];
    let bytes = preserved(&target, &names);
    let outcome = update_project(&new, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert!(outcome.applied);
    assert_eq!(outcome.state.version, "0.3.0");
    assert!(!target.join(".devkit/runtime/project.mjs").exists());
    assert_eq!(preserved(&target, &names), bytes);
    let accepted: Value = read_json(&target.join(MANIFEST_PATH)).unwrap();
    assert_eq!(accepted["future"], state["future"]);
    for field in ["future", "origin", "development"] {
        assert_eq!(
            accepted["components"]["apps/api"][field],
            state["components"]["apps/api"][field]
        );
    }
    read_runtime_project(&target, "0.3.0").unwrap();
}

#[test]
fn preview_is_read_only_noop_is_byte_stable_and_local_registry_is_transactional() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.0");
    let new = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("project");
    let initial = init(&old, &target, "node", PackageMode::Local);
    let before = source_signature(&target, Some(&initial)).unwrap();
    let manifest = fs::read(target.join(MANIFEST_PATH)).unwrap();
    let preview = update_project(
        &new,
        &target,
        &UpdateOptions {
            preview: true,
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert!(!preview.applied && !preview.files.is_empty());
    assert_eq!(source_signature(&target, Some(&initial)).unwrap(), before);
    assert_eq!(fs::read(target.join(MANIFEST_PATH)).unwrap(), manifest);
    assert!(!target.join(PENDING_PATH).exists());
    let updated =
        update_project(&new, &target, &UpdateOptions::default(), &FixtureResolver).unwrap();
    let bytes = fs::read(target.join(MANIFEST_PATH)).unwrap();
    let no_op = update_project(&new, &target, &UpdateOptions::default(), &FixtureResolver).unwrap();
    assert!(no_op.no_op && no_op.files.is_empty());
    assert_eq!(updated.state.generation, no_op.state.generation);
    assert_eq!(fs::read(target.join(MANIFEST_PATH)).unwrap(), bytes);
    let registry = update_project(
        &new,
        &target,
        &UpdateOptions {
            package_mode: Some(PackageMode::Registry),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert!(registry.applied && !registry.no_op);
    assert_ne!(registry.state.generation, no_op.state.generation);
    assert!(!target.join(".devkit/npm/env-0.3.0.tgz").exists());
    assert_eq!(
        read_json::<Value>(&target.join("apps/main/package.json")).unwrap()["devDependencies"]["@docouno/env"],
        "0.3.0"
    );
    update_project(
        &new,
        &target,
        &UpdateOptions {
            package_mode: Some(PackageMode::Local),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    read_runtime_project(&target, "0.3.0").unwrap();
}

#[test]
fn same_release_updates_native_name_links_but_preserves_root_product_graph() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("consumer");
    let initial = init(&release, &target, "node", PackageMode::Registry);
    let mut root: Value = read_json(&target.join("package.json")).unwrap();
    root["scripts"] = json!({"custom":"unchanged"});
    root["dependencies"] = json!({"product":"1.0.0"});
    root["unknown"] = json!({"keep":true});
    put_json(&target, "package.json", &root);
    let mut member: Value = read_json(&target.join("apps/main/package.json")).unwrap();
    let previous = member["name"].as_str().unwrap().to_string();
    member["name"] = json!("@custom/renamed");
    member["version"] = json!("2.3.4");
    member["exports"] = json!({".":"./product.js"});
    put_json(&target, "apps/main/package.json", &member);
    let mut lock: Value = read_json(&target.join("package-lock.json")).unwrap();
    lock["packages"][""] = root.clone();
    lock["packages"]["node_modules/product"] = json!({"version":"1.0.0","resolved":"https://registry.npmjs.org/product/-/product-1.0.0.tgz","integrity":sri(b"product artifact"),"optional":false});
    put_json(&target, "package-lock.json", &lock);
    let changed = update_project(
        &release,
        &target,
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap();
    assert_ne!(initial.generation, changed.state.generation);
    let after: Value = read_json(&target.join("package-lock.json")).unwrap();
    assert!(
        after["packages"]
            .get(format!("node_modules/{previous}"))
            .is_none()
    );
    assert_eq!(
        after["packages"]["node_modules/@custom/renamed"]["resolved"],
        "apps/main"
    );
    assert_eq!(
        after["packages"]["node_modules/product"],
        lock["packages"]["node_modules/product"]
    );
    assert_eq!(
        read_json::<Value>(&target.join("package.json")).unwrap(),
        root
    );
    assert_eq!(
        read_json::<Value>(&target.join("apps/main/package.json")).unwrap(),
        member
    );
}

struct ConcurrentEdit {
    root: PathBuf,
}
impl MutationHooks for ConcurrentEdit {
    fn after_stage(&self) -> Result<()> {
        put(&self.root, "apps/api/src/concurrent.rs", b"user edit");
        Ok(())
    }
}
#[test]
fn conflicts_and_concurrent_inputs_fail_before_any_payload_write() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("consumer");
    let initial = init(&old, &target, "rust", PackageMode::Registry);
    let manifest = fs::read(target.join(MANIFEST_PATH)).unwrap();
    let runtime = fs::read(target.join(".devkit/runtime/devkit.mk")).unwrap();
    let hooks = ConcurrentEdit {
        root: target.clone(),
    };
    let e = update_project(
        &new,
        &target,
        &UpdateOptions {
            hooks: Some(&hooks),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap_err();
    assert!(e.to_string().contains("changed while staging"));
    assert_eq!(fs::read(target.join(MANIFEST_PATH)).unwrap(), manifest);
    assert_eq!(
        fs::read(target.join(".devkit/runtime/devkit.mk")).unwrap(),
        runtime
    );
    assert!(!target.join(PENDING_PATH).exists());
    let before = source_signature(&target, Some(&initial)).unwrap();
    put(
        &target,
        ".devkit/runtime/devkit.mk",
        b"user modified managed bytes",
    );
    let edited = source_signature(&target, Some(&initial)).unwrap();
    assert_ne!(before, edited);
    assert!(
        update_project(&new, &target, &UpdateOptions::default(), &NoResolver)
            .unwrap_err()
            .to_string()
            .contains("Managed file conflict")
    );
    assert_eq!(source_signature(&target, Some(&initial)).unwrap(), edited);
}

struct FailAt {
    path: String,
}
impl MutationHooks for FailAt {
    fn after_write(&self, _: usize, path: &str) -> Result<()> {
        if path == self.path {
            bail!("injected apply failure");
        }
        Ok(())
    }
}
#[test]
fn rollback_removes_only_created_namespaces_even_after_manifest_write() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    for existing in [false, true] {
        for at_manifest in [false, true] {
            let target = f.path().join(format!("{existing}-{at_manifest}"));
            let state = init(&release, &target, "rust", PackageMode::Registry);
            if existing {
                fs::create_dir(target.join("packages")).unwrap();
            }
            let before = source_signature(&target, Some(&state)).unwrap();
            let hook = FailAt {
                path: if at_manifest {
                    MANIFEST_PATH.to_string()
                } else {
                    "packages/client/package.json".to_string()
                },
            };
            let e = add_project_component(
                &release,
                &target,
                Role::Library,
                ComponentSpec {
                    name: "client".to_string(),
                    template: "react-client".to_string(),
                    uses: vec![],
                },
                &UpdateOptions {
                    hooks: Some(&hook),
                    ..Default::default()
                },
                &FixtureResolver,
            )
            .unwrap_err();
            assert!(e.to_string().contains("injected apply failure"));
            assert_eq!(source_signature(&target, Some(&state)).unwrap(), before);
            assert_eq!(target.join("packages").exists(), existing);
            assert!(!target.join("packages/client").exists());
            assert!(!target.join(PENDING_PATH).exists());
        }
    }
}

struct ExitAfterWrite;
impl MutationHooks for ExitAfterWrite {
    fn after_write(&self, _: usize, path: &str) -> Result<()> {
        let at_manifest = std::env::var_os("DEVKIT_TEST_MANIFEST").is_some();
        if !at_manifest || path == MANIFEST_PATH {
            std::process::exit(86);
        }
        Ok(())
    }
}
#[test]
#[ignore = "invoked by interruption recovery integration test in a separate process"]
fn interruption_worker() {
    let release = PathBuf::from(std::env::var_os("DEVKIT_TEST_RELEASE").unwrap());
    let target = PathBuf::from(std::env::var_os("DEVKIT_TEST_TARGET").unwrap());
    update_project(
        &release,
        &target,
        &UpdateOptions {
            hooks: Some(&ExitAfterWrite),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
}
#[test]
fn durable_journal1_and2_recovery_preflights_every_saved_and_current_identity() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    for at_manifest in [false, true] {
        let target = f.path().join(format!("interrupted-{at_manifest}"));
        let state = init(&old, &target, "rust", PackageMode::Registry);
        let before = source_signature(&target, Some(&state)).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "interruption_worker", "--ignored"])
            .env("DEVKIT_TEST_RELEASE", &new)
            .env("DEVKIT_TEST_TARGET", &target);
        if at_manifest {
            child.env("DEVKIT_TEST_MANIFEST", "1");
        }
        let status = child.status().unwrap();
        assert_eq!(status.code(), Some(86));
        let tx = read_journal(&target).unwrap();
        let file = tx.files.iter().find(|f| f.backup.is_some()).unwrap();
        let backup = target.join(file.backup.as_ref().unwrap());
        let saved = fs::read(&backup).unwrap();
        put(&target, file.backup.as_ref().unwrap(), b"corrupt backup");
        let partial = source_signature(&target, Some(&state)).unwrap();
        assert!(
            recover_project(&target)
                .unwrap_err()
                .to_string()
                .contains("backup integrity mismatch")
        );
        assert_eq!(source_signature(&target, Some(&state)).unwrap(), partial);
        write_atomic(&backup, &saved, file.before.as_ref().unwrap().mode).unwrap();
        let changed = &tx.files[0];
        let current = fs::read(target.join(&changed.path)).ok();
        put(&target, &changed.path, b"unknown user bytes");
        let partial = source_signature(&target, Some(&state)).unwrap();
        assert!(
            recover_project(&target)
                .unwrap_err()
                .to_string()
                .contains("Recovery conflict")
        );
        assert_eq!(source_signature(&target, Some(&state)).unwrap(), partial);
        if let Some(bytes) = current {
            write_atomic(
                &target.join(&changed.path),
                &bytes,
                changed.after.as_ref().unwrap().mode,
            )
            .unwrap();
        } else {
            remove_file(&target.join(&changed.path)).unwrap();
        }
        // Public journal1 and journal2 share the same exact saved-path contract.
        let mut raw: Value = read_json(&target.join(PENDING_PATH)).unwrap();
        raw["schemaVersion"] = json!(if at_manifest { 1 } else { 2 });
        put_json(&target, PENDING_PATH, &raw);
        assert!(recover_project(&target).unwrap());
        assert!(!recover_project(&target).unwrap());
        assert_eq!(source_signature(&target, Some(&state)).unwrap(), before);
        assert!(!target.join(tx.stage).exists());
    }
}

struct UnknownSource {
    root: PathBuf,
}
impl MutationHooks for UnknownSource {
    fn after_write(&self, _: usize, path: &str) -> Result<()> {
        if path == "packages/client/package.json" {
            put(&self.root, "packages/client/unknown.txt", b"preserve me");
            bail!("interrupted with new user file");
        }
        Ok(())
    }
}
#[test]
fn unknown_new_component_source_blocks_all_rollback_writes() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("new source");
    let state = init(&release, &target, "rust", PackageMode::Registry);
    let before = source_signature(&target, Some(&state)).unwrap();
    let hook = UnknownSource {
        root: target.clone(),
    };
    let error = add_project_component(
        &release,
        &target,
        Role::Library,
        ComponentSpec {
            name: "client".to_string(),
            template: "react-client".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            hooks: Some(&hook),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap_err();
    assert!(error.to_string().contains("automatic rollback was blocked"));
    let partial = source_signature(&target, Some(&state)).unwrap();
    assert!(
        recover_project(&target)
            .unwrap_err()
            .to_string()
            .contains("unknown file")
    );
    assert_eq!(source_signature(&target, Some(&state)).unwrap(), partial);
    fs::remove_file(target.join("packages/client/unknown.txt")).unwrap();
    recover_project(&target).unwrap();
    assert_eq!(source_signature(&target, Some(&state)).unwrap(), before);
}

#[test]
fn accepted_release_policy_and_registration_preserve_native_manifests() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("consumer");
    init(&old, &target, "rust", PackageMode::Registry);
    copy_tree(
        &old.join("templates/rust-api"),
        &target.join("packages/shared"),
        &|_| false,
        false,
    )
    .unwrap();
    let cargo = fs::read(target.join("packages/shared/Cargo.toml")).unwrap();
    assert!(
        register_project_component(
            &new,
            &target,
            Role::Library,
            "shared".to_string(),
            ToolingProfile::Rust,
            &UpdateOptions::default(),
            &NoResolver
        )
        .unwrap_err()
        .to_string()
        .contains("exact accepted release")
    );
    let adopted = register_project_component(
        &old,
        &target,
        Role::Library,
        "shared".to_string(),
        ToolingProfile::Rust,
        &UpdateOptions::default(),
        &NoResolver,
    )
    .unwrap();
    assert_eq!(
        adopted.state.components["packages/shared"].origin.template,
        "adopted"
    );
    assert_eq!(
        fs::read(target.join("packages/shared/Cargo.toml")).unwrap(),
        cargo
    );
    update_project(&new, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    let duplicate = f.path().join("different-same-version");
    copy_tree(&new, &duplicate, &|_| false, false).unwrap();
    put(&duplicate, "runtime/devkit.mk", b"different release");
    seal(&duplicate, "0.3.0");
    assert!(
        update_project(&duplicate, &target, &UpdateOptions::default(), &NoResolver)
            .unwrap_err()
            .to_string()
            .contains("same version")
    );
}

struct CollisionNormalizer;
impl ComposeNormalizer for CollisionNormalizer {
    fn normalize(&self, snapshot: &Path, entrypoints: &[String]) -> Result<Value> {
        assert!(snapshot.join("included.yaml").exists());
        assert!(entrypoints.iter().any(|p| p == "compose.yaml"));
        Ok(json!({"services":{"api":{},"collision":{}}}))
    }
}
#[test]
fn compose_closure_is_confined_budgeted_and_normalized_inventory_blocks_collisions() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("consumer");
    let state = init(&release, &target, "rust", PackageMode::Registry);
    put(
        &target,
        "compose.yaml",
        b"include: [included.yaml]\nservices: {}\n",
    );
    put(
        &target,
        "included.yaml",
        b"services:\n  collision:\n    image: busybox\n",
    );
    let inventory = admit_compose_inventory(&target, &CollisionNormalizer).unwrap();
    let before = source_signature(&target, Some(&state)).unwrap();
    let error = add_project_component(
        &release,
        &target,
        Role::App,
        ComponentSpec {
            name: "collision".to_string(),
            template: "rust-api".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap_err();
    assert!(error.to_string().contains("Compose service conflict"));
    assert_eq!(source_signature(&target, Some(&state)).unwrap(), before);
    assert!(parse_compose(b"services: {}\nservices: {}\n").is_err());
    let merged =
        parse_compose(b"x-base: &base {image: busybox}\nservices:\n  child:\n    <<: *base\n")
            .unwrap();
    assert_eq!(merged["services"]["child"]["image"], "busybox");
    let aliases = format!(
        "base: &base [x]\nservices: {{}}\nbomb: [{}]\n",
        std::iter::repeat_n("*base", 101)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(parse_compose(aliases.as_bytes()).is_err());
    put(
        &target,
        "compose.yaml",
        b"include: [../outside.yaml]\nservices: {}\n",
    );
    assert!(capture_compose_inputs(&target).is_err());
}

struct PopulateTarget {
    target: PathBuf,
}
impl MutationHooks for PopulateTarget {
    fn after_stage(&self) -> Result<()> {
        put(&self.target, "concurrent.txt", b"keep");
        Ok(())
    }
}
#[test]
fn init_rechecks_empty_target_preserves_git_and_rejects_symlink_ancestors() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("empty");
    fs::create_dir_all(target.join(".git")).unwrap();
    put(&target, ".git/config", b"existing git metadata");
    init(&release, &target, "rust", PackageMode::Registry);
    assert_eq!(
        fs::read(target.join(".git/config")).unwrap(),
        b"existing git metadata"
    );
    let race = f.path().join("race");
    fs::create_dir(&race).unwrap();
    let hook = PopulateTarget {
        target: race.clone(),
    };
    assert!(
        init_project(
            &release,
            &race,
            &InitOptions {
                template: Some("rust".to_string()),
                hooks: Some(&hook),
                ..Default::default()
            },
            &NoResolver
        )
        .is_err()
    );
    assert_eq!(sorted_entries(&race).unwrap().len(), 1);
    let outside = f.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let link = f.path().join("link");
    symlink(&outside, &link).unwrap();
    assert!(
        init_project(
            &release,
            &link.join("child"),
            &InitOptions {
                template: Some("rust".to_string()),
                ..Default::default()
            },
            &NoResolver
        )
        .is_err()
    );
    assert!(!outside.join("child").exists());
}

#[test]
#[ignore = "requires explicit genuine public0.2 release/consumer fixtures and Docker CLI; run the compiled test binary without a host SDK"]
fn genuine_accepted_legacy_add_passes_old_runtime_doctor() {
    let release = PathBuf::from(
        std::env::var_os("DEVKIT_TEST_LEGACY_RELEASE").expect("legacy release fixture"),
    );
    let consumer = PathBuf::from(
        std::env::var_os("DEVKIT_TEST_LEGACY_CONSUMER").expect("legacy consumer fixture"),
    );
    let f = TempDir::new().unwrap();
    let target = f.path().join("native old-version operation");
    let old = read_project_state(&consumer).unwrap();
    copy_tree(&consumer, &target, &|p| snapshot_excluded(&old, p), true).unwrap();
    let mut inventory = capture_compose_inputs(&target).unwrap();
    inventory.services = old
        .components
        .values()
        .filter_map(|c| c.development.as_ref()?.service.clone())
        .collect();
    let outcome = add_project_component(
        &release,
        &target,
        Role::App,
        ComponentSpec {
            name: "native-added".to_string(),
            template: "rust-api".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &DockerNpmResolver,
    )
    .unwrap();
    assert_eq!(outcome.state.version, old.version);
    assert_eq!(outcome.state.release_identity, old.release_identity);
    for (key, value) in old.extra {
        assert_eq!(outcome.state.extra.get(&key), Some(&value));
    }
    let setup = Command::new("make")
        .arg("setup")
        .env("CARGO_BUILD_JOBS", "2")
        .current_dir(&target)
        .status()
        .unwrap();
    assert!(
        setup.success(),
        "Old public runtime setup failed after native accepted-version add"
    );
    let status = Command::new("make")
        .arg("doctor")
        .current_dir(&target)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "Old public runtime rejected native-generated accepted-version projections"
    );
}

#[test]
fn native_lifecycle_lock_is_shared_and_recovery_refuses_a_linked_lock() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("locked");
    init(&release, &target, "rust", PackageMode::Registry);
    let lock = CheckoutLock::acquire(&target).unwrap();
    assert!(CheckoutLock::acquire(&target).is_err());
    drop(lock);
    drop(CheckoutLock::acquire(&target).unwrap());
    let outside = f.path().join("outside-lock");
    fs::remove_file(target.join(".devkit/state/task.lock")).unwrap();
    symlink(&outside, target.join(".devkit/state/task.lock")).unwrap();
    assert!(recover_project(&target).is_err());
    assert!(!outside.exists());
}

#[test]
fn rollback_removes_a_new_runtime_directory_without_pruning_existing_parents() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    put(&new, "runtime/nested/helper", b"new managed helper\n");
    seal(&new, "0.3.0");
    let target = f.path().join("runtime parent");
    let state = init(&old, &target, "rust", PackageMode::Registry);
    let before = source_signature(&target, Some(&state)).unwrap();
    let hook = FailAt {
        path: ".devkit/runtime/nested/helper".to_string(),
    };
    let error = update_project(
        &new,
        &target,
        &UpdateOptions {
            hooks: Some(&hook),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap_err();
    assert!(error.to_string().contains("injected apply failure"));
    assert_eq!(source_signature(&target, Some(&state)).unwrap(), before);
    assert!(!target.join(".devkit/runtime/nested").exists());
    assert!(target.join(".devkit/runtime").is_dir());
}

#[test]
fn component_named_target_is_source_while_its_rust_target_output_is_excluded() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("named target");
    let initial = init_project(
        &old,
        &target,
        &InitOptions {
            apps: Some(vec![ComponentSpec {
                name: "target".to_string(),
                template: "rust-api".to_string(),
                uses: vec![],
            }]),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    put(
        &target,
        "apps/target/src/main.rs",
        b"fn main() { println!(\"preserved\"); }\n",
    );
    put(
        &target,
        "apps/target/target/build-output",
        b"cache must not be staged",
    );
    assert!(!snapshot_excluded(&initial, "apps/target/src/main.rs"));
    assert!(snapshot_excluded(
        &initial,
        "apps/target/target/build-output"
    ));
    update_project(&new, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert_eq!(
        fs::read(target.join("apps/target/src/main.rs")).unwrap(),
        b"fn main() { println!(\"preserved\"); }\n"
    );
    assert!(target.join("apps/target/target/build-output").is_file());
}
