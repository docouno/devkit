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

#[test]
fn rust_add_ignores_numeric_unicode_and_renamed_checkout_basenames() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    for basename in ["123", "Проект", "renamed Ω"] {
        let original = f.path().join(format!("initial-{basename}"));
        init_project(
            &release,
            &original,
            &InitOptions {
                template: Some("rust".to_string()),
                name: Some("product".to_string()),
                ..Default::default()
            },
            &NoResolver,
        )
        .unwrap();
        let target = f.path().join(basename);
        fs::rename(original, &target).unwrap();
        // Rust composition must not parse unrelated root npm metadata either.
        put_json(
            &target,
            "package.json",
            &json!({"productMetadata":{"keep":true}}),
        );
        let inventory = capture_compose_inputs(&target).unwrap();
        let out = add_project_component(
            &release,
            &target,
            Role::App,
            ComponentSpec {
                name: "worker".to_string(),
                template: "rust-api".to_string(),
                uses: vec![],
            },
            &UpdateOptions {
                compose_inventory: Some(&inventory),
                ..Default::default()
            },
            &NoResolver,
        )
        .unwrap();
        assert!(out.state.components.contains_key("apps/worker"));
        assert_eq!(
            read_json::<Value>(&target.join("package.json")).unwrap(),
            json!({"productMetadata":{"keep":true}})
        );
    }
}

#[test]
fn node_add_preserves_declared_scoped_namespace_after_checkout_move() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let original = f.path().join("123");
    init_project(
        &release,
        &original,
        &InitOptions {
            template: Some("node".to_string()),
            name: Some("product".to_string()),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let target = f.path().join("Ω renamed");
    fs::rename(original, &target).unwrap();
    let mut root: Value = read_json(&target.join("package.json")).unwrap();
    root["name"] = json!("@team/product");
    root["futureRoot"] = json!({"keep":true});
    put_json(&target, "package.json", &root);
    let mut lock: Value = read_json(&target.join("package-lock.json")).unwrap();
    lock["name"] = root["name"].clone();
    lock["packages"][""] = root.clone();
    put_json(&target, "package-lock.json", &lock);
    let existing = fs::read(target.join("apps/main/package.json")).unwrap();
    let inventory = capture_compose_inputs(&target).unwrap();
    add_project_component(
        &release,
        &target,
        Role::App,
        ComponentSpec {
            name: "worker".to_string(),
            template: "node-api".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let next: Value = read_json(&target.join("package.json")).unwrap();
    assert_eq!(next["name"], "@team/product");
    assert_eq!(next["futureRoot"], root["futureRoot"]);
    assert_eq!(
        fs::read(target.join("apps/main/package.json")).unwrap(),
        existing
    );
    assert_eq!(
        read_json::<Value>(&target.join("apps/worker/package.json")).unwrap()["name"],
        "@team/product-app-worker"
    );
}

#[test]
fn first_node_member_in_rust_project_establishes_a_stable_private_namespace() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("123");
    init_project(
        &release,
        &target,
        &InitOptions {
            template: Some("rust".to_string()),
            name: Some("product".to_string()),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    add_project_component(
        &release,
        &target,
        Role::Library,
        ComponentSpec {
            name: "client".to_string(),
            template: "react-client".to_string(),
            uses: vec![],
        },
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        read_json::<Value>(&target.join("package.json")).unwrap()["name"],
        "devkit-project"
    );
    assert_eq!(
        read_json::<Value>(&target.join("packages/client/package.json")).unwrap()["name"],
        "@devkit-project/package-client"
    );
    let moved = f.path().join("renamed λ");
    fs::rename(target, &moved).unwrap();
    let inventory = capture_compose_inputs(&moved).unwrap();
    add_project_component(
        &release,
        &moved,
        Role::App,
        ComponentSpec {
            name: "web".to_string(),
            template: "react-web".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        read_json::<Value>(&moved.join("apps/web/package.json")).unwrap()["name"],
        "@devkit-project/app-web"
    );
}

#[test]
fn node_add_keeps_an_existing_private_root_manifest_without_a_name() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    let target = f.path().join("123");
    init_project(
        &release,
        &target,
        &InitOptions {
            template: Some("rust".to_string()),
            name: Some("product".to_string()),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    put_json(
        &target,
        "package.json",
        &json!({"private":true,"futureRoot":{"keep":true}}),
    );
    add_project_component(
        &release,
        &target,
        Role::Library,
        ComponentSpec {
            name: "client".to_string(),
            template: "react-client".to_string(),
            uses: vec![],
        },
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap();
    let root: Value = read_json(&target.join("package.json")).unwrap();
    assert!(root.get("name").is_none());
    assert_eq!(root["futureRoot"], json!({"keep":true}));
    assert_eq!(
        read_json::<Value>(&target.join("packages/client/package.json")).unwrap()["name"],
        "@devkit-project/package-client"
    );
}

#[test]
fn accepted_legacy_default_ignore_blocks_only_unsafe_new_sources_without_writes() {
    let legacy=b"**/node_modules/\n**/dist/\n**/target/\n**/release/\n.devkit/cache/\n.devkit/state/\n.env\n.env.*\n!.env.example\n";
    assert_eq!(
        digest(legacy),
        "348c45ac15b10cd537bea277b83411de9885329b6aa53570b04c42590c2e4a92"
    );
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.2.1");
    let target = f.path().join("legacy");
    let initial = init(&release, &target, "rust", PackageMode::Registry);
    put(&target, ".gitignore", legacy);
    for (role, name, template) in [
        (Role::App, "target", "rust-api"),
        (Role::App, "dist", "node-api"),
        (Role::Library, "release", "react-client"),
    ] {
        let before = source_signature(&target, Some(&initial)).unwrap();
        let manifest = fs::read(target.join(MANIFEST_PATH)).unwrap();
        let inventory = capture_compose_inputs(&target).unwrap();
        let error = add_project_component(
            &release,
            &target,
            role,
            ComponentSpec {
                name: name.to_string(),
                template: template.to_string(),
                uses: vec![],
            },
            &UpdateOptions {
                compose_inventory: Some(&inventory),
                ..Default::default()
            },
            &FixtureResolver,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("adjust your project-owned .gitignore"),
            "{error}"
        );
        assert_eq!(source_signature(&target, Some(&initial)).unwrap(), before);
        assert_eq!(fs::read(target.join(MANIFEST_PATH)).unwrap(), manifest);
        assert_eq!(fs::read(target.join(".gitignore")).unwrap(), legacy);
        assert!(!target.join(PENDING_PATH).exists());
    }
    // An ordinary add and explicit update never take ownership of this file.
    let inventory = capture_compose_inputs(&target).unwrap();
    add_project_component(
        &release,
        &target,
        Role::App,
        ComponentSpec {
            name: "worker".to_string(),
            template: "rust-api".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    update_project(&release, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert_eq!(fs::read(target.join(".gitignore")).unwrap(), legacy);
    // A custom owner file is not interpreted by an embedded Git parser.
    let custom = b"# Product-owned policy\n/scratch/\n";
    put(&target, ".gitignore", custom);
    let inventory = capture_compose_inputs(&target).unwrap();
    add_project_component(
        &release,
        &target,
        Role::App,
        ComponentSpec {
            name: "target".to_string(),
            template: "rust-api".to_string(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    assert_eq!(fs::read(target.join(".gitignore")).unwrap(), custom);
}

fn isolated_git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_TEMPLATE_DIR",
    ] {
        command.env_remove(name);
    }
    command
}
fn git_ignored(root: &Path, path: &str) -> bool {
    let status = isolated_git(root)
        .args(["check-ignore", "--quiet", "--no-index", path])
        .status()
        .expect("Git is required for this explicit gate");
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => panic!("git check-ignore failed for {path}: {status}"),
    }
}
#[test]
#[ignore = "requires Git; run the compiled native test executable on the Linux host"]
fn git_tracks_output_named_component_sources_and_ignores_scoped_outputs() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.3.0");
    for (name, component, source) in [
        ("target", "apps/target", "Cargo.toml"),
        ("dist", "apps/dist", "package.json"),
        ("release", "packages/release", "package.json"),
    ] {
        let target = f.path().join(format!("component-{name}"));
        if name == "release" {
            init(&release, &target, "rust", PackageMode::Registry);
            add_project_component(
                &release,
                &target,
                Role::Library,
                ComponentSpec {
                    name: name.to_string(),
                    template: "react-client".to_string(),
                    uses: vec![],
                },
                &UpdateOptions::default(),
                &FixtureResolver,
            )
            .unwrap();
        } else {
            init_project(
                &release,
                &target,
                &InitOptions {
                    apps: Some(vec![ComponentSpec {
                        name: name.to_string(),
                        template: if name == "target" {
                            "rust-api".to_string()
                        } else {
                            "node-api".to_string()
                        },
                        uses: vec![],
                    }]),
                    ..Default::default()
                },
                &FixtureResolver,
            )
            .unwrap();
        }
        assert!(
            isolated_git(&target)
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "init",
                    "--quiet",
                    "--template="
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            !git_ignored(&target, &format!("{component}/{source}")),
            "A component source must be eligible for commit: {component}/{source}"
        );
        for output in ["target", "dist", "release"] {
            let file = format!("{component}/{output}/build-output");
            put(&target, &file, b"generated output");
            assert!(
                git_ignored(&target, &file),
                "A scoped generated output must be ignored: {file}"
            );
        }
    }
}

struct RetainRecoveryStage {
    root: PathBuf,
}
impl MutationHooks for RetainRecoveryStage {
    fn after_write(&self, count: usize, _: &str) -> Result<()> {
        if count == 1 {
            put(
                &self.root,
                ".devkit/runtime/devkit.mk",
                b"unknown user bytes",
            );
            bail!("retain stage for explicit recovery");
        }
        Ok(())
    }
}
#[test]
#[ignore = "test-only worker invoked by the Linux strace ordering test"]
fn durability_order_worker() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.2.1");
    let new = fixture_release(f.path(), "0.3.0");
    put(&new, "runtime/nested/helper", b"new managed helper");
    seal(&new, "0.3.0");
    let committed = f.path().join("committed");
    init(&old, &committed, "rust", PackageMode::Registry);
    update_project(&new, &committed, &UpdateOptions::default(), &NoResolver).unwrap();
    let recovered = f.path().join("recovered");
    init(&old, &recovered, "rust", PackageMode::Registry);
    let hooks = RetainRecoveryStage {
        root: recovered.clone(),
    };
    assert!(
        update_project(
            &new,
            &recovered,
            &UpdateOptions {
                hooks: Some(&hooks),
                ..Default::default()
            },
            &NoResolver
        )
        .unwrap_err()
        .to_string()
        .contains("automatic rollback was blocked")
    );
    let journal = read_journal(&recovered).unwrap();
    let file = journal
        .files
        .iter()
        .find(|f| f.path == ".devkit/runtime/devkit.mk")
        .unwrap();
    write_atomic(
        &recovered.join(&file.path),
        &fs::read(recovered.join(file.backup.as_ref().unwrap())).unwrap(),
        file.before.as_ref().unwrap().mode,
    )
    .unwrap();
    assert!(recover_project(&recovered).unwrap());
}

fn successful_sync(line: &str, path: &str) -> bool {
    line.contains("fsync(")
        && line.contains(&format!("<{path}>)"))
        && line.trim_end().ends_with("= 0")
}
fn assert_durable_transaction_trace(trace: &str) {
    let lines = trace.lines().collect::<Vec<_>>();
    let rename = regex::Regex::new(r#"rename\("([^"]+)", "([^"]+)"\)\s+= 0"#).unwrap();
    let mkdir = regex::Regex::new(r#"mkdir\("([^"]+)", [0-7]+\)\s+= 0"#).unwrap();
    let publications = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let c = rename.captures(line)?;
            let path = c.get(2)?.as_str();
            path.ends_with("/.devkit/update-pending.json")
                .then(|| (i, path.to_string()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        publications.len(),
        2,
        "Expected actual successful commit and explicit recovery transactions"
    );
    for (published, pending) in publications {
        let devkit = Path::new(&pending).parent().unwrap().to_str().unwrap();
        let stage_prefix = format!("{devkit}/state/update-");
        let (created, stage) = lines[..published]
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let c = mkdir.captures(line)?;
                let path = c.get(1)?.as_str();
                (path.starts_with(&stage_prefix) && !path[stage_prefix.len()..].contains('/'))
                    .then(|| (i, path.to_string()))
            })
            .next_back()
            .expect("A new durable recovery stage must precede publication");
        for directory in [
            format!("{devkit}/state"),
            stage.clone(),
            format!("{stage}/backup"),
            format!("{stage}/staged"),
        ] {
            let created_at = lines[..published]
                .iter()
                .enumerate()
                .filter_map(|(i, line)| {
                    mkdir
                        .captures(line)
                        .and_then(|c| (c.get(1)?.as_str() == directory).then_some(i))
                })
                .next_back();
            if let Some(created_at) = created_at {
                let parent = Path::new(&directory).parent().unwrap().to_str().unwrap();
                assert!(
                    lines[created_at + 1..published]
                        .iter()
                        .any(|line| successful_sync(line, parent)),
                    "New saved-data directory entry was not durable before pending: {directory}"
                );
            } else {
                assert!(
                    directory.ends_with("/state"),
                    "Saved stage directory creation missing: {directory}"
                );
            }
        }
        assert!(created < published);
        let unlinked = lines
            .iter()
            .enumerate()
            .skip(published + 1)
            .find(|(_, line)| {
                line.contains(&format!("unlink(\"{pending}\")")) && line.trim_end().ends_with("= 0")
            })
            .map(|(i, _)| i)
            .expect("Pending marker was not removed");
        let discarded = lines
            .iter()
            .enumerate()
            .skip(unlinked + 1)
            .find(|(_, line)| {
                line.contains(&stage)
                    && (line.contains("unlink(")
                        || line.contains("unlinkat(")
                        || line.contains("rmdir("))
                    && line.trim_end().ends_with("= 0")
            })
            .map(|(i, _)| i)
            .expect("Recovery data was not discarded");
        assert!(
            lines[unlinked + 1..discarded]
                .iter()
                .any(|line| successful_sync(line, devkit)),
            "Pending removal was not durable before saved stage cleanup"
        );
        if let Some(manifest) = lines
            .iter()
            .enumerate()
            .take(unlinked)
            .skip(published + 1)
            .find_map(|(i, line)| {
                rename.captures(line).and_then(|c| {
                    (c.get(2)?.as_str() == format!("{devkit}/manifest.json")).then_some(i)
                })
            })
        {
            let deleted = format!("{devkit}/runtime/project.mjs");
            let deletion = lines[published + 1..manifest]
                .iter()
                .position(|line| {
                    line.contains(&format!("unlink(\"{deleted}\")"))
                        && line.trim_end().ends_with("= 0")
                })
                .map(|i| i + published + 1)
                .expect("Expected managed deletion before manifest");
            assert!(
                lines[deletion + 1..manifest]
                    .iter()
                    .any(|line| successful_sync(line, &format!("{devkit}/runtime"))),
                "Payload deletion must be durable before final manifest"
            );
            let new_parent = format!("{devkit}/runtime/nested");
            let creation = lines[published + 1..manifest]
                .iter()
                .position(|line| {
                    mkdir
                        .captures(line)
                        .is_some_and(|c| c.get(1).unwrap().as_str() == new_parent)
                })
                .map(|i| i + published + 1)
                .expect("Expected new payload parent before manifest");
            assert!(
                lines[creation + 1..manifest]
                    .iter()
                    .any(|line| successful_sync(line, &format!("{devkit}/runtime"))),
                "New payload parent must be durable before final manifest"
            );
        }
    }
}

#[test]
#[ignore = "requires Linux strace; explicitly run in the pinned strace image"]
fn linux_strace_checks_actual_commit_and_recovery_durability_order() {
    let f = TempDir::new().unwrap();
    let trace = f.path().join("durability.trace");
    let status = Command::new("strace")
        .args([
            "-f",
            "-yy",
            "-s",
            "4096",
            "-e",
            "trace=mkdir,mkdirat,rename,renameat,renameat2,fsync,fdatasync,unlink,unlinkat,rmdir",
            "-o",
        ])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "durability_order_worker",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .status()
        .expect("Linux strace must be available for this explicit gate");
    assert!(status.success(), "Traced transaction worker failed");
    let bytes = fs::read(&trace).unwrap();
    if let Some(output) = std::env::var_os("DEVKIT_TEST_TRACE_OUTPUT") {
        fs::write(output, &bytes).unwrap();
    }
    assert_durable_transaction_trace(std::str::from_utf8(&bytes).unwrap());
}
