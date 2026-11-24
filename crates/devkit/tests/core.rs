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
    let parsed = SemVer::parse(version).unwrap();
    let min_supported_from_version = if parsed >= SemVer::parse("0.6.0").unwrap() {
        Some("0.5.0".to_owned())
    } else {
        (parsed >= SemVer::parse("0.5.0").unwrap()).then(|| "0.4.0".to_owned())
    };
    write_json(
        &root.join("release.json"),
        &Release {
            schema_version: 1,
            version: version.to_string(),
            min_supported_from_version,
            rust_version: "1.99.1".to_string(),
            environment: Environment {
                name: "@docouno/env".to_string(),
                version: version.to_string(),
                archive: archive.clone(),
                integrity: sri(&fs::read(root.join(archive)).unwrap()),
            },
            files: file_map(root).unwrap(),
            migrations: Vec::new(),
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
    let engine_sha = digest(format!("fixture-engine-{version}").as_bytes());
    let recipe_hash = "b".repeat(64);
    put(
        &root,
        "runtime/toolchains.env",
        format!(
            "DEVKIT_VERSION={version}\nDEVKIT_ENGINE_SHA256={engine_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
        )
        .as_bytes(),
    );
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
        ("ts-library", true),
        ("react-ui", true),
    ] {
        let mut manifest = json!({"name":format!("fixture-{name}"),"version":"1.0.0","private":true,
            "devDependencies":{"@docouno/env":version},"scripts":{"check":"check","fix":"fix","test":"test","build":"build","dev":"dev"}});
        if library {
            manifest["scripts"].as_object_mut().unwrap().remove("dev");
            if ["ts-library", "react-ui"].contains(&name) {
                manifest["files"] = json!(["dist"]);
                manifest["exports"] = json!({".":{"types":"./dist/index.d.mts","default":"./dist/index.mjs"},"./package.json":"./package.json"});
                manifest["scripts"]["package"] = json!("devkit-pack");
            } else {
                manifest["exports"] = json!({".":"./src/index.ts"});
            }
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
    put_json(
        &root,
        "templates/react-web/variants/shared/variant.json",
        &json!({"remove":[]}),
    );
    put(
        &root,
        "templates/react-web/variants/shared/files/src/index.ts",
        b"export { answer } from '__DEVKIT_CLIENT_PACKAGE__';\n",
    );
    put_json(
        &root,
        "templates/electron-react/variants/shared/variant.json",
        &json!({"remove":[]}),
    );
    put(
        &root,
        "templates/electron-react/variants/shared/files/src/index.ts",
        b"export { answer } from '__DEVKIT_CLIENT_PACKAGE__';\n",
    );
    put_json(
        &root,
        "templates/react-web/variants/design-system/variant.json",
        &json!({"remove":[]}),
    );
    put(
        &root,
        "templates/react-web/variants/design-system/files/src/index.ts",
        b"export { answer } from '__DEVKIT_UI_PACKAGE__';\n",
    );
    for (name, bytes) in [
        (
            "Cargo.toml",
            "[package]\nname = \"fixture-api\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        ),
        (
            "Cargo.lock",
            "version = 4\n\n[[package]]\nname = \"fixture-api\"\nversion = \"1.0.0\"\n",
        ),
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
    if SemVer::parse(version).unwrap() >= SemVer::parse("0.5.0").unwrap() {
        let pin = devkit::runtime::PnpmPin::from_source_pins(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap();
        put(&root, "runtime/pnpm.env", pin.encode().as_bytes());
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
        if root.join("pnpm-lock.yaml").is_file() {
            let mut lock: devkit::pnpm::Lock = devkit::pnpm::read_yaml(root, "pnpm-lock.yaml")?;
            let workspace = devkit::pnpm::Workspace::read(root)?;
            for path in workspace.packages {
                let manifest: Value = read_json(&root.join(&path).join("package.json"))?;
                let spec = manifest["devDependencies"]["@docouno/env"]
                    .as_str()
                    .unwrap();
                let base = if let Some(file) = spec.strip_prefix("file:") {
                    format!("file:{}", devkit::native::local_path(root, &path, file)?)
                } else {
                    spec.to_owned()
                };
                let key = format!("@docouno/env@{base}");
                let resolution = if base.starts_with("file:") {
                    json!({"integrity":release.manifest.environment.integrity,"tarball":base})
                } else {
                    json!({"integrity":release.manifest.environment.integrity})
                };
                let mut package = json!({"resolution":resolution});
                if base.starts_with("file:") {
                    package["version"] = json!(release.manifest.version);
                }
                lock.packages
                    .insert(key.clone(), package.as_object().unwrap().clone());
                lock.snapshots.insert(key, serde_json::Map::new());
                lock.importers
                    .get_mut(&path)
                    .unwrap()
                    .dev_dependencies
                    .insert(
                        "@docouno/env".into(),
                        devkit::pnpm::Dependency {
                            specifier: spec.into(),
                            version: base,
                        },
                    );
            }
            lock.prune_unused()?;
            return lock.write(root);
        }
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
    let release = fixture_release(f.path(), "0.5.0");
    let admitted = admit_release(&release).unwrap();
    assert_eq!(
        admitted.manifest.min_supported_from_version.as_deref(),
        Some("0.4.0")
    );
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
    put(&source, "npm/env-0.5.0.tgz", b"changed env bytes");
    assert!(import_npm_payload(&source, &f.path().join("rejected")).is_err());
    assert!(!f.path().join("rejected").exists());
    let mut forged: Value = read_json(&release.join("release.json")).unwrap();
    put(&release, "npm/env-0.5.0.tgz", b"new env archive");
    forged["files"]["npm/env-0.5.0.tgz"]["sha256"] = json!(digest(b"new env archive"));
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
    let old = fixture_release(f.path(), "0.4.1");
    put(&old, "runtime/obsolete-helper.sh", b"old helper\n");
    seal(&old, "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
    let target = f.path().join("rust product with spaces");
    init(&old, &target, "rust", PackageMode::Local);
    assert!(target.join(".devkit/runtime/obsolete-helper.sh").exists());
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
        "apps/api/Cargo.lock",
        b"version = 4\n\n[[package]]\nname = \"custom\"\nversion = \"2.3.4\"\n",
    );
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
        "apps/api/Cargo.lock",
        "Makefile",
        "compose.yaml",
        ".env",
    ];
    let bytes = preserved(&target, &names);
    let outcome = update_project(&new, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert!(outcome.applied);
    assert_eq!(outcome.state.version, "0.5.0");
    assert!(!target.join(".devkit/runtime/obsolete-helper.sh").exists());
    assert_eq!(preserved(&target, &names), bytes);
    let accepted: Value = read_json(&target.join(MANIFEST_PATH)).unwrap();
    assert_eq!(accepted["future"], state["future"]);
    for field in ["future", "origin", "development"] {
        assert_eq!(
            accepted["components"]["apps/api"][field],
            state["components"]["apps/api"][field]
        );
    }
    read_runtime_project(&target, "0.5.0").unwrap();
}

#[test]
fn preview_is_read_only_noop_is_byte_stable_and_local_registry_is_transactional() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
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
    assert!(!target.join(".devkit/npm/env-0.5.0.tgz").exists());
    assert_eq!(
        read_json::<Value>(&target.join("apps/main/package.json")).unwrap()["devDependencies"]["@docouno/env"],
        "0.5.0"
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
    read_runtime_project(&target, "0.5.0").unwrap();
}

#[test]
fn npm_migration_refuses_destination_collisions_before_creating_a_stage() {
    let temporary = TempDir::new().unwrap();
    let old = fixture_release(temporary.path(), "0.4.1");
    let next = fixture_release(temporary.path(), "0.5.0");
    let target = temporary.path().join("consumer");
    let state = init(&old, &target, "node", PackageMode::Local);
    for destination in ["pnpm-lock.yaml", "pnpm-workspace.yaml"] {
        put(&target, destination, b"consumer-owned collision\n");
        let before = source_signature(&target, None).unwrap();
        let error = update_project(
            &next,
            &target,
            &UpdateOptions {
                preview: true,
                ..Default::default()
            },
            &NoResolver,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("migration destination already exists"),
            "{error:#}"
        );
        assert_eq!(source_signature(&target, None).unwrap(), before);
        assert!(
            !fs::read_dir(target.join(".devkit/state"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("update-"))
        );
        if destination == "pnpm-lock.yaml" {
            devkit::graph::read_legacy_npm_product_graph(&target, &state).unwrap();
        }
        fs::remove_file(target.join(destination)).unwrap();
    }
}

#[test]
fn npm_upgrade_migrates_linked_public_ranges_and_config_without_touching_preview_receipts() {
    let temporary = TempDir::new().unwrap();
    let old = fixture_release(temporary.path(), "0.4.1");
    let next = fixture_release(temporary.path(), "0.5.0");
    let target = temporary.path().join("libraries");
    init_project(
        &old,
        &target,
        &InitOptions {
            packages: Some(vec![
                ComponentSpec {
                    name: "core".into(),
                    template: "ts-library".into(),
                    uses: vec![],
                },
                ComponentSpec {
                    name: "ui".into(),
                    template: "ts-library".into(),
                    uses: vec!["core".into()],
                },
                ComponentSpec {
                    name: "peer".into(),
                    template: "ts-library".into(),
                    uses: vec![],
                },
            ]),
            package_mode: Some(PackageMode::Registry),
            name: Some("suite".into()),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let mut lock: Value = read_json(&target.join("package-lock.json")).unwrap();
    for path in ["packages/core", "packages/ui", "packages/peer"] {
        let mut manifest: Value = read_json(&target.join(path).join("package.json")).unwrap();
        manifest["private"] = json!(false);
        if path == "packages/ui" {
            manifest["dependencies"]["@suite/package-core"] = json!("^1.0.0");
        }
        if path == "packages/peer" {
            manifest["peerDependencies"] = json!({"@suite/package-core":"^1.0.0"});
        }
        put_json(&target, &format!("{path}/package.json"), &manifest);
        lock["packages"][path] = manifest;
    }
    let mut root_manifest: Value = read_json(&target.join("package.json")).unwrap();
    root_manifest["dependencies"] = json!({"@suite/package-ui":"file:packages/ui"});
    root_manifest["peerDependencies"] = json!({"@suite/package-peer":"^1.0.0"});
    root_manifest["scripts"] = json!({"custom":"unchanged"});
    put_json(&target, "package.json", &root_manifest);
    lock["packages"][""] = root_manifest;
    put_json(&target, "package-lock.json", &lock);
    put(&target,".npmrc",b"# user configuration\npackage-lock=true\ninstall-strategy=hoisted\nregistry=https://registry.npmjs.org/\n");
    put(
        &target,
        ".devkit/state/node-workspace.lock",
        b"existing-setup-receipt\n",
    );
    let signature = source_signature(&target, None).unwrap();
    update_project(
        &next,
        &target,
        &UpdateOptions {
            preview: true,
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(source_signature(&target, None).unwrap(), signature);
    assert_eq!(
        fs::read(target.join(".devkit/state/node-workspace.lock")).unwrap(),
        b"existing-setup-receipt\n"
    );
    update_project(&next, &target, &UpdateOptions::default(), &FixtureResolver).unwrap();
    assert!(!target.join("package-lock.json").exists());
    assert!(target.join("pnpm-lock.yaml").is_file());
    assert!(target.join("pnpm-workspace.yaml").is_file());
    assert!(!target.join(".devkit/state/node-workspace.lock").exists());
    assert!(
        read_json::<Value>(&target.join("package.json"))
            .unwrap()
            .get("workspaces")
            .is_none()
    );
    assert_eq!(
        read_json::<Value>(&target.join("packages/ui/package.json")).unwrap()["dependencies"]["@suite/package-core"],
        "workspace:^"
    );
    let root_manifest: Value = read_json(&target.join("package.json")).unwrap();
    assert_eq!(
        root_manifest["dependencies"]["@suite/package-ui"],
        "file:packages/ui"
    );
    assert_eq!(root_manifest["scripts"]["custom"], "unchanged");
    assert_eq!(
        root_manifest["peerDependencies"]["@suite/package-peer"],
        "^1.0.0"
    );
    assert_eq!(
        root_manifest["devDependencies"]["@suite/package-peer"],
        "workspace:*"
    );
    let peer_manifest: Value = read_json(&target.join("packages/peer/package.json")).unwrap();
    assert_eq!(
        peer_manifest["peerDependencies"]["@suite/package-core"],
        "^1.0.0"
    );
    assert_eq!(
        peer_manifest["devDependencies"]["@suite/package-core"],
        "workspace:*"
    );
    assert_eq!(
        fs::read_to_string(target.join(".npmrc")).unwrap(),
        "# user configuration\nregistry=https://registry.npmjs.org/\n"
    );
}

#[test]
fn sync_resolves_new_ranges_aliases_and_sections_without_changing_preview_inputs() {
    use std::cell::Cell;
    struct SelectedArtifact(Cell<usize>);
    impl LockResolver for SelectedArtifact {
        fn regenerate(&self, root: &Path, _: &AdmittedRelease, _: Option<&str>) -> Result<()> {
            self.0.set(self.0.get() + 1);
            let mut lock = devkit::pnpm::Lock::read(root)?;
            let manifest: Value = read_json(&root.join("apps/main/package.json"))?;
            for field in devkit::pnpm::PHYSICAL_FIELDS {
                for name in ["fresh", "alias", "optional"] {
                    if let Some(spec) = manifest[field][name].as_str() {
                        lock.importers
                            .get_mut("apps/main")
                            .unwrap()
                            .field_mut(field)
                            .insert(
                                name.into(),
                                devkit::pnpm::Dependency {
                                    specifier: spec.into(),
                                    version: if name == "fresh" {
                                        "1.2.3"
                                    } else {
                                        "fresh@1.2.3"
                                    }
                                    .into(),
                                },
                            );
                    }
                }
            }
            lock.packages.insert(
                "fresh@1.2.3".into(),
                json!({"resolution":{"integrity":sri(b"chosen artifact")}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            lock.snapshots
                .insert("fresh@1.2.3".into(), Default::default());
            lock.write(root)
        }
    }
    let temporary = TempDir::new().unwrap();
    let release = fixture_release(temporary.path(), "0.5.0");
    for optional in [false, true] {
        let peer_target = temporary.path().join(format!("peer-{optional}"));
        init(&release, &peer_target, "node", PackageMode::Registry);
        let mut manifest: Value = read_json(&peer_target.join("apps/main/package.json")).unwrap();
        manifest["peerDependencies"] = json!({"fresh":"^1.0.0"});
        manifest["devDependencies"]["fresh"] = json!("1.2.3");
        if optional {
            manifest["peerDependenciesMeta"] = json!({"fresh":{"optional":true}});
        }
        put_json(&peer_target, "apps/main/package.json", &manifest);
        let before = source_signature(&peer_target, None).unwrap();
        let resolver = SelectedArtifact(Cell::new(0));
        for reconcile_lock in [false, true] {
            sync_project(
                &release,
                &peer_target,
                &UpdateOptions {
                    preview: true,
                    reconcile_lock,
                    ..Default::default()
                },
                &resolver,
            )
            .unwrap();
            assert_eq!(source_signature(&peer_target, None).unwrap(), before);
        }
        assert_eq!(resolver.0.get(), 2);
        sync_project(&release, &peer_target, &UpdateOptions::default(), &resolver).unwrap();
        read_runtime_project(&peer_target, "0.5.0").unwrap();
    }
    let target = temporary.path().join("consumer");
    init(&release, &target, "node", PackageMode::Registry);
    let resolver = SelectedArtifact(Cell::new(0));
    let mut manifest: Value = read_json(&target.join("apps/main/package.json")).unwrap();
    manifest["dependencies"] = json!({"fresh":"^1.0.0","alias":"npm:fresh@^1.0.0"});
    put_json(&target, "apps/main/package.json", &manifest);
    let before = source_signature(&target, None).unwrap();
    sync_project(
        &release,
        &target,
        &UpdateOptions {
            preview: true,
            ..Default::default()
        },
        &resolver,
    )
    .unwrap();
    assert_eq!(resolver.0.get(), 1);
    assert_eq!(source_signature(&target, None).unwrap(), before);
    sync_project(&release, &target, &UpdateOptions::default(), &resolver).unwrap();
    assert_eq!(resolver.0.get(), 2);
    manifest["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("fresh");
    manifest["devDependencies"]["fresh"] = json!("^1.0.0");
    manifest["optionalDependencies"] = json!({"optional":"npm:fresh@^1.0.0"});
    put_json(&target, "apps/main/package.json", &manifest);
    sync_project(&release, &target, &UpdateOptions::default(), &resolver).unwrap();
    assert_eq!(resolver.0.get(), 3);
    read_runtime_project(&target, "0.5.0").unwrap();
}

#[test]
fn sync_preserves_unregistered_directory_snapshots_and_rejects_dangling_links() {
    let temporary = TempDir::new().unwrap();
    let release = fixture_release(temporary.path(), "0.5.0");
    let target = temporary.path().join("consumer");
    init(&release, &target, "node", PackageMode::Registry);
    put_json(
        &target,
        "shared/local/package.json",
        &json!({"name":"local","version":"1.0.0","private":true}),
    );
    let mut manifest: Value = read_json(&target.join("apps/main/package.json")).unwrap();
    manifest["dependencies"] = json!({"local":"file:../../shared/local"});
    put_json(&target, "apps/main/package.json", &manifest);
    let mut lock = devkit::pnpm::Lock::read(&target).unwrap();
    lock.importers
        .get_mut("apps/main")
        .unwrap()
        .dependencies
        .insert(
            "local".into(),
            devkit::pnpm::Dependency {
                specifier: "file:../../shared/local".into(),
                version: "file:shared/local".into(),
            },
        );
    lock.packages.insert(
        "local@file:shared/local".into(),
        json!({"version":"1.0.0","resolution":{"type":"directory","directory":"shared/local"}})
            .as_object()
            .unwrap()
            .clone(),
    );
    lock.snapshots
        .insert("local@file:shared/local".into(), Default::default());
    lock.write(&target).unwrap();
    sync_project(&release, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    read_runtime_project(&target, "0.5.0").unwrap();
    let before = source_signature(&target, None).unwrap();
    let result = sync_project(&release, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert!(result.no_op);
    assert_eq!(source_signature(&target, None).unwrap(), before);
    lock.importers
        .get_mut("apps/main")
        .unwrap()
        .dependencies
        .get_mut("local")
        .unwrap()
        .version = "link:../../shared/local".into();
    lock.write(&target).unwrap();
    let error = read_runtime_project(&target, "0.5.0").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Local pnpm link has no importer")
    );
    lock.importers
        .get_mut("apps/main")
        .unwrap()
        .dependencies
        .get_mut("local")
        .unwrap()
        .version = "file:shared/local".into();
    lock.packages
        .get_mut("local@file:shared/local")
        .unwrap()
        .get_mut("resolution")
        .unwrap()["directory"] = json!("shared/other");
    put_json(
        &target,
        "shared/other/package.json",
        &json!({"name":"local","version":"1.0.0","private":true}),
    );
    lock.write(&target).unwrap();
    let manifest = devkit::native::native_manifest(&target, "apps/main").unwrap();
    assert!(
        lock.validate_importer(&target, "apps/main", &manifest)
            .unwrap_err()
            .to_string()
            .contains("different path")
    );
}

#[test]
fn register_rebases_standalone_local_directory_chains_before_resolution() {
    struct InspectSeed;
    impl LockResolver for InspectSeed {
        fn regenerate(
            &self,
            root: &Path,
            release: &AdmittedRelease,
            registry: Option<&str>,
        ) -> Result<()> {
            let lock = devkit::pnpm::Lock::read(root)?;
            assert_eq!(
                lock.importers["apps/extra"].dependencies["local"].version,
                "file:apps/extra/vendor/local"
            );
            assert_eq!(
                lock.packages["local@file:apps/extra/vendor/local"]["resolution"]["directory"],
                "apps/extra/vendor/local"
            );
            assert_eq!(
                lock.snapshots["local@file:apps/extra/vendor/local"]["dependencies"]["leaf"],
                "file:apps/extra/vendor/leaf"
            );
            FixtureResolver.regenerate(root, release, registry)
        }
    }
    let temporary = TempDir::new().unwrap();
    let release = fixture_release(temporary.path(), "0.5.0");
    let target = temporary.path().join("consumer");
    init(&release, &target, "node", PackageMode::Registry);
    let extra = target.join("apps/extra");
    copy_tree(
        &release.join("templates/node-api"),
        &extra,
        &|_| false,
        false,
    )
    .unwrap();
    let mut manifest: Value = read_json(&extra.join("package.json")).unwrap();
    manifest["name"] = json!("existing-extra");
    manifest["dependencies"] = json!({"local":"file:vendor/local"});
    put_json(&extra, "package.json", &manifest);
    put_json(
        &extra,
        "vendor/local/package.json",
        &json!({"name":"local","version":"1.0.0","private":true,"dependencies":{"leaf":"file:../leaf"}}),
    );
    put_json(
        &extra,
        "vendor/leaf/package.json",
        &json!({"name":"leaf","version":"1.0.0","private":true}),
    );
    let mut lock = devkit::pnpm::import_legacy_npm_lock_seed(
        &extra,
        &devkit::native::package_lock(&extra).unwrap(),
    )
    .unwrap();
    lock.importers.get_mut(".").unwrap().dependencies.insert(
        "local".into(),
        devkit::pnpm::Dependency {
            specifier: "file:vendor/local".into(),
            version: "file:vendor/local".into(),
        },
    );
    for name in ["local", "leaf"] {
        let key = format!("{name}@file:vendor/{name}");
        lock.packages.insert(key.clone(), json!({"version":"1.0.0","resolution":{"type":"directory","directory":format!("vendor/{name}")}}).as_object().unwrap().clone());
        lock.snapshots.insert(
            key,
            if name == "local" {
                json!({"dependencies":{"leaf":"file:vendor/leaf"}})
                    .as_object()
                    .unwrap()
                    .clone()
            } else {
                Default::default()
            },
        );
    }
    lock.write(&extra).unwrap();
    let before = source_signature(&target, None).unwrap();
    let preview = register_project_component(
        &release,
        &target,
        Role::App,
        "extra".into(),
        ToolingProfile::Node,
        &UpdateOptions {
            preview: true,
            ..Default::default()
        },
        &InspectSeed,
    )
    .unwrap();
    assert!(!preview.applied);
    assert_eq!(source_signature(&target, None).unwrap(), before);
    register_project_component(
        &release,
        &target,
        Role::App,
        "extra".into(),
        ToolingProfile::Node,
        &UpdateOptions::default(),
        &InspectSeed,
    )
    .unwrap();
    read_runtime_project(&target, "0.5.0").unwrap();
    assert!(!extra.join("pnpm-lock.yaml").exists());
}

#[test]
fn same_release_accepts_native_renames_and_preserves_root_product_graph() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");
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
    let mut lock = devkit::pnpm::Lock::read(&target).unwrap();
    lock.importers.get_mut(".").unwrap().dependencies.insert(
        "product".into(),
        devkit::pnpm::Dependency {
            specifier: "1.0.0".into(),
            version: "1.0.0".into(),
        },
    );
    let artifact = json!({"resolution":{"integrity":sri(b"product artifact")}})
        .as_object()
        .unwrap()
        .clone();
    lock.packages
        .insert("product@1.0.0".into(), artifact.clone());
    lock.snapshots
        .insert("product@1.0.0".into(), serde_json::Map::new());
    lock.write(&target).unwrap();
    let changed = update_project(
        &release,
        &target,
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap();
    assert!(changed.no_op);
    assert_eq!(initial.generation, changed.state.generation);
    let after = devkit::pnpm::Lock::read(&target).unwrap();
    assert!(after.importers.contains_key("apps/main"));
    assert_eq!(after.packages["product@1.0.0"], artifact);
    assert!(!target.join("package-lock.json").exists());
    assert_ne!(previous, member["name"].as_str().unwrap());
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
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
fn durable_journal2_recovery_preflights_identities_and_rejects_journal1() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
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
        // Journal schemaVersion 1 (< 0.4.0) is rejected without rolling back; schemaVersion 2 recovers.
        let mut raw: Value = read_json(&target.join(PENDING_PATH)).unwrap();
        raw["schemaVersion"] = json!(1);
        put_json(&target, PENDING_PATH, &raw);
        let partial = source_signature(&target, Some(&state)).unwrap();
        assert!(
            recover_project(&target)
                .unwrap_err()
                .to_string()
                .contains("Unsupported update journal")
        );
        assert_eq!(source_signature(&target, Some(&state)).unwrap(), partial);
        raw["schemaVersion"] = json!(2);
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let old = fixture_release(f.path(), "0.5.1");
    let new = fixture_release(f.path(), "0.6.0");
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
    seal(&duplicate, "0.6.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
fn native_lifecycle_lock_is_shared_and_recovery_refuses_a_linked_lock() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");
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
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
    put(&new, "runtime/nested/helper", b"new managed helper\n");
    seal(&new, "0.5.0");
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
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let release = fixture_release(f.path(), "0.5.1");
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
    let release = fixture_release(f.path(), "0.5.0");
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
    let old = fixture_release(f.path(), "0.4.1");
    put(&old, "runtime/obsolete-helper.sh", b"old helper\n");
    seal(&old, "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
    put(&new, "runtime/nested/helper", b"new managed helper");
    seal(&new, "0.5.0");
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
fn successful_unlink(line: &str, path: &str) -> bool {
    (line.contains("unlink(") || line.contains("unlinkat("))
        && line.contains(&format!("\"{path}\""))
        && line.trim_end().ends_with("= 0")
}
fn assert_durable_transaction_trace(trace: &str) {
    let lines = trace.lines().collect::<Vec<_>>();
    let rename = regex::Regex::new(r#"rename(?:at2?)?\((?:AT_FDCWD(?:<[^>]+>)?, )?"([^"]+)", (?:AT_FDCWD(?:<[^>]+>)?, )?"([^"]+)"(?:, [^)]*)?\)\s+= 0"#).unwrap();
    let mkdir =
        regex::Regex::new(r#"mkdir(?:at)?\((?:AT_FDCWD(?:<[^>]+>)?, )?"([^"]+)", [0-7]+\)\s+= 0"#)
            .unwrap();
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
            .find(|(_, line)| successful_unlink(line, &pending))
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
            let deleted = format!("{devkit}/runtime/obsolete-helper.sh");
            let deletion = lines[published + 1..manifest]
                .iter()
                .position(|line| successful_unlink(line, &deleted))
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
    // AArch64 implements the *at syscall family and has no mkdir/rename/unlink opcodes.
    let calls = if cfg!(target_arch = "aarch64") {
        "trace=mkdirat,renameat,renameat2,fsync,fdatasync,unlinkat"
    } else {
        "trace=mkdir,mkdirat,rename,renameat,renameat2,fsync,fdatasync,unlink,unlinkat,rmdir"
    };
    let status = Command::new("strace")
        .args(["-f", "-yy", "-s", "4096", "-e", calls, "-o"])
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

// Ports of the source scenarios retained when the former TS factory was removed.
#[test]
fn author_port_same_component_basename_keeps_both_namespace_orders_and_owner_names() {
    for package_first in [false, true] {
        let f = TempDir::new().unwrap();
        let release = fixture_release(f.path(), "0.5.0");
        let target = f.path().join("consumer");
        let first = if package_first { "starter" } else { "client" };
        init_project(
            &release,
            &target,
            &InitOptions {
                apps: Some(vec![ComponentSpec {
                    name: first.into(),
                    template: "node-api".into(),
                    uses: vec![],
                }]),
                package_mode: Some(PackageMode::Local),
                ..Default::default()
            },
            &FixtureResolver,
        )
        .unwrap();
        let original_path = format!("apps/{first}/package.json");
        let original: Value = read_json(&target.join(&original_path)).unwrap();
        let source_path = format!("apps/{first}/src/index.ts");
        put(&target, &source_path, b"export const owner = 'keep';\n");
        add_project_component(
            &release,
            &target,
            Role::Library,
            ComponentSpec {
                name: "client".into(),
                template: "react-client".into(),
                uses: vec![],
            },
            &UpdateOptions::default(),
            &FixtureResolver,
        )
        .unwrap();
        if package_first {
            let inventory = capture_compose_inputs(&target).unwrap();
            add_project_component(
                &release,
                &target,
                Role::App,
                ComponentSpec {
                    name: "client".into(),
                    template: "node-api".into(),
                    uses: vec![],
                },
                &UpdateOptions {
                    compose_inventory: Some(&inventory),
                    ..Default::default()
                },
                &FixtureResolver,
            )
            .unwrap();
        }
        let app: Value = read_json(&target.join("apps/client/package.json")).unwrap();
        let library: Value = read_json(&target.join("packages/client/package.json")).unwrap();
        assert_ne!(app["name"], library["name"]);
        assert_eq!(
            read_json::<Value>(&target.join(original_path)).unwrap()["name"],
            original["name"]
        );
        assert_eq!(
            fs::read(target.join(source_path)).unwrap(),
            b"export const owner = 'keep';\n"
        );
        let state = read_project_state(&target).unwrap();
        assert_eq!(state.components["apps/client"].role, Role::App);
        assert_eq!(state.components["packages/client"].role, Role::Library);
        devkit::native::assert_native_components(&target, &state).unwrap();
    }
}

#[test]
fn author_port_release_admission_refuses_unsafe_manifest_and_source_shapes() {
    for case in [
        "parent",
        "symlink",
        "environment",
        "mode",
        "schema",
        "min_supported",
    ] {
        let f = TempDir::new().unwrap();
        let release = fixture_release(f.path(), "0.5.0");
        let outside = f.path().join("outside");
        let loader = release.join("runtime/devkit.sh");
        fs::write(&outside, fs::read(&loader).unwrap()).unwrap();
        let sentinel = fs::read(&outside).unwrap();
        let mut manifest: Value = read_json(&release.join("release.json")).unwrap();
        match case {
            "parent" => {
                manifest["files"]["../outside"] = json!({"sha256":"0".repeat(64),"mode":0o644})
            }
            "symlink" => {
                fs::remove_file(&loader).unwrap();
                symlink(&outside, &loader).unwrap();
            }
            "environment" => manifest["environment"]["version"] = json!("0.4.0"),
            "mode" => manifest["files"]["runtime/devkit.sh"]["mode"] = json!(0o777),
            "schema" => manifest["schemaVersion"] = json!(2),
            "min_supported" => manifest["minSupportedFromVersion"] = json!("0.6.0"),
            _ => unreachable!(),
        }
        put_json(&release, "release.json", &manifest);
        let original = fs::read(release.join("release.json")).unwrap();
        assert!(admit_release(&release).is_err(), "{case}");
        let imported = f.path().join("private-cache");
        assert!(
            import_npm_payload(&release, &imported).is_err(),
            "npm import: {case}"
        );
        assert!(
            !imported.exists(),
            "Rejected import published a cache: {case}"
        );
        assert_eq!(fs::read(release.join("release.json")).unwrap(), original);
        assert_eq!(fs::read(&outside).unwrap(), sentinel);
        if case == "symlink" {
            assert_eq!(fs::read_link(&loader).unwrap(), outside);
        }
    }
}

#[test]
fn author_port_opaque_product_links_survive_copy_and_native_update() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
    let source = f.path().join("opaque");
    put(&source, "source.ts", b"export const owner = true;\n");
    put(&source, "node_modules/cache", b"discard cache\n");
    symlink("source.ts", source.join("source-link")).unwrap();
    let strict = f.path().join("strict-copy");
    assert!(copy_tree(&source, &strict, &|_| false, false).is_err());
    let copied = f.path().join("preserved-copy");
    copy_tree(&source, &copied, &|p| p.starts_with("node_modules"), true).unwrap();
    assert_eq!(
        fs::read_link(copied.join("source-link")).unwrap(),
        PathBuf::from("source.ts")
    );
    assert_eq!(
        fs::read(copied.join("source-link")).unwrap(),
        b"export const owner = true;\n"
    );
    assert!(!copied.join("node_modules").exists());
    let target = f.path().join("consumer");
    init(&old, &target, "rust", PackageMode::Local);
    copy_tree(
        &source,
        &target.join("assets"),
        &|p| p.starts_with("node_modules"),
        true,
    )
    .unwrap();
    update_project(&new, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert_eq!(
        fs::read_link(target.join("assets/source-link")).unwrap(),
        PathBuf::from("source.ts")
    );
    assert_eq!(
        fs::read(target.join("assets/source-link")).unwrap(),
        b"export const owner = true;\n"
    );
    assert_eq!(
        fs::read(source.join("source.ts")).unwrap(),
        b"export const owner = true;\n"
    );
    assert!(!target.join("assets/node_modules").exists());
}

#[test]
fn author_port_slug_paths_and_private_atomic_modes_keep_the_original_boundaries() {
    assert_eq!(slug("app-42").unwrap(), "app-42");
    for name in [
        "../app",
        "a--b",
        "a-",
        "App",
        "a b",
        "a;echo",
        &"x".repeat(64),
    ] {
        assert!(slug(name).is_err(), "{name}");
    }
    for name in ["/absolute", "../parent", "a/../b", "a//b", "a\\b", "a\nb"] {
        assert!(relative_path(name).is_err(), "{name}");
    }
    assert_eq!(
        relative_path(".devkit/runtime/devkit.sh").unwrap(),
        ".devkit/runtime/devkit.sh"
    );
    let f = TempDir::new().unwrap();
    let private = f.path().join("private.json");
    write_atomic(&private, b"before", 0o600).unwrap();
    let before = identity(&private).unwrap().unwrap();
    assert_eq!(before.mode, 0o600);
    fs::set_permissions(&private, fs::Permissions::from_mode(0o644)).unwrap();
    assert_ne!(identity(&private).unwrap().unwrap(), before);
    write_atomic(&private, b"after", 0o600).unwrap();
    assert_eq!(fs::read(&private).unwrap(), b"after");
    assert_eq!(identity(&private).unwrap().unwrap().mode, 0o600);
}

fn author_port_payload(root: &Path) -> BTreeMap<String, FileIdentity> {
    file_map(root)
        .unwrap()
        .into_iter()
        .filter(|(p, _)| !p.starts_with(".devkit/state/"))
        .collect()
}

#[test]
fn author_port_tool_profile_and_native_tool_edits_refuse_without_payload_writes() {
    for accepted_profile in [true, false] {
        let f = TempDir::new().unwrap();
        let release = fixture_release(f.path(), "0.5.0");
        let target = f.path().join("consumer");
        init(&release, &target, "node", PackageMode::Local);
        if accepted_profile {
            let mut state: Value = read_json(&target.join(MANIFEST_PATH)).unwrap();
            state["components"]["apps/main"]["toolDependencies"]["@docouno/env"] =
                json!("tampered");
            put_json(&target, MANIFEST_PATH, &state);
        } else {
            let mut manifest: Value = read_json(&target.join("apps/main/package.json")).unwrap();
            manifest["devDependencies"]["@docouno/env"] = json!("9.9.9");
            put_json(&target, "apps/main/package.json", &manifest);
        }
        let before = author_port_payload(&target);
        let error = update_project(
            &release,
            &target,
            &UpdateOptions::default(),
            &FixtureResolver,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains(if accepted_profile {
                "Accepted tooling profile differs"
            } else {
                "Tool dependency conflict"
            }),
            "{error:#}"
        );
        assert_eq!(author_port_payload(&target), before);
        assert!(!target.join(PENDING_PATH).exists());
    }
}

#[test]
fn author_port_register_existing_node_workspace_preserves_owner_identity_and_source() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");
    let target = f.path().join("consumer");
    init(&release, &target, "node", PackageMode::Local);
    copy_tree(
        &target.join("apps/main"),
        &target.join("apps/existing"),
        &|_| false,
        true,
    )
    .unwrap();
    let mut member: Value = read_json(&target.join("apps/existing/package.json")).unwrap();
    member["name"] = json!("@adopt/existing");
    member["owner"] = json!({"keep":true});
    put_json(&target, "apps/existing/package.json", &member);
    put(
        &target,
        "apps/existing/src/index.ts",
        b"export const owner = 'adopted source';\n",
    );
    let mut workspace = devkit::pnpm::Workspace::read(&target).unwrap();
    workspace.packages = vec!["apps/existing".into(), "apps/main".into()];
    devkit::pnpm::write_yaml(&target, "pnpm-workspace.yaml", &workspace).unwrap();
    let mut lock = devkit::pnpm::Lock::read(&target).unwrap();
    let importer = lock.importers["apps/main"].clone();
    lock.importers.insert("apps/existing".into(), importer);
    lock.write(&target).unwrap();
    let adopted = register_project_component(
        &release,
        &target,
        Role::App,
        "existing".into(),
        ToolingProfile::Node,
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        adopted.state.components["apps/existing"].origin.template,
        "adopted"
    );
    assert_eq!(
        read_json::<Value>(&target.join("apps/existing/package.json")).unwrap(),
        member
    );
    assert_eq!(
        fs::read(target.join("apps/existing/src/index.ts")).unwrap(),
        b"export const owner = 'adopted source';\n"
    );
    devkit::native::assert_native_components(&target, &adopted.state).unwrap();
    let before = author_port_payload(&target);
    let error = register_project_component(
        &release,
        &target,
        Role::Library,
        "invalid".into(),
        ToolingProfile::Node,
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap_err();
    assert!(error.to_string().contains("cannot register"), "{error:#}");
    assert_eq!(author_port_payload(&target), before);
    assert!(!target.join(PENDING_PATH).exists());
}

struct AuthorPortResolutionFailure;
impl LockResolver for AuthorPortResolutionFailure {
    fn regenerate(&self, _: &Path, _: &AdmittedRelease, _: Option<&str>) -> Result<()> {
        bail!("author native npm resolution failure")
    }
}

#[test]
fn author_port_new_managed_collision_and_native_failures_preserve_payload_and_pending() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
    put(&new, "runtime/new-helper.sh", b"release helper\n");
    seal(&new, "0.5.0");
    let target = f.path().join("consumer");
    init(&old, &target, "node", PackageMode::Local);
    put(&target, ".devkit/runtime/new-helper.sh", b"owner helper\n");
    let before = author_port_payload(&target);
    let error =
        update_project(&new, &target, &UpdateOptions::default(), &FixtureResolver).unwrap_err();
    assert!(
        error.to_string().contains("New managed file conflicts"),
        "{error:#}"
    );
    assert_eq!(author_port_payload(&target), before);
    assert!(!target.join(PENDING_PATH).exists());
    fs::remove_file(target.join(".devkit/runtime/new-helper.sh")).unwrap();
    let before = author_port_payload(&target);
    let error = update_project(
        &new,
        &target,
        &UpdateOptions::default(),
        &AuthorPortResolutionFailure,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("author native npm resolution failure"),
        "{error:#}"
    );
    assert_eq!(author_port_payload(&target), before);
    assert!(!target.join(PENDING_PATH).exists());
    let mut member: Value = read_json(&target.join("apps/main/package.json")).unwrap();
    member["dependencies"] = json!({"missing-native-input":"file:../../shared/absent-package.tgz"});
    put_json(&target, "apps/main/package.json", &member);
    let before = author_port_payload(&target);
    assert!(update_project(&new, &target, &UpdateOptions::default(), &FixtureResolver).is_err());
    assert_eq!(author_port_payload(&target), before);
    assert!(!target.join(PENDING_PATH).exists());
}

#[test]
fn author_port_mixed_alias_keeps_api_web_wiring_and_edited_web_source_on_update() {
    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.4.1");
    let new = fixture_release(f.path(), "0.5.0");
    for (release, version) in [(&old, "0.4.1"), (&new, "0.5.0")] {
        put(
            release,
            "templates/react-web/src/main.tsx",
            b"export const view = 'template';\n",
        );
        seal(release, version);
    }
    let target = f.path().join("consumer");
    let state = init(&old, &target, "mixed", PackageMode::Local);
    assert_eq!(
        state
            .components
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["apps/api", "apps/web"]
    );
    assert_eq!(
        state.components["apps/web"]
            .development
            .as_ref()
            .unwrap()
            .api
            .as_deref(),
        Some("apps/api")
    );
    put(
        &target,
        "apps/web/src/main.tsx",
        b"// Product-owned entry\nexport const view = 'edited';\n",
    );
    update_project(&new, &target, &UpdateOptions::default(), &FixtureResolver).unwrap();
    assert_eq!(
        fs::read(target.join("apps/web/src/main.tsx")).unwrap(),
        b"// Product-owned entry\nexport const view = 'edited';\n"
    );
    let state = read_project_state(&target).unwrap();
    assert_eq!(
        state.components["apps/web"]
            .development
            .as_ref()
            .unwrap()
            .api
            .as_deref(),
        Some("apps/api")
    );
    devkit::native::assert_native_components(&target, &state).unwrap();
}

#[test]
fn sync_project_offline_fast_path_renames_workspace_package_rotates_generation_and_recovers() {
    struct FailAfterFirstWrite;
    impl MutationHooks for FailAfterFirstWrite {
        fn after_write(&self, count: usize, _path: &str) -> Result<()> {
            if count == 1 {
                bail!("injected sync write interruption")
            }
            Ok(())
        }
    }

    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");
    let target = f.path().join("consumer");
    init(&release, &target, "react-web", PackageMode::Registry);
    let initial = add_project_component(
        &release,
        &target,
        Role::Library,
        ComponentSpec {
            name: "client".into(),
            template: "react-client".into(),
            uses: vec![],
        },
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap()
    .state;
    assert!(initial.components.contains_key("packages/client"));

    // Rename packages/client, bump its version, and wire local file: consumer in apps/web.
    let mut client_pkg: Value = read_json(&target.join("packages/client/package.json")).unwrap();
    let old_name = client_pkg["name"].as_str().unwrap().to_owned();
    let new_name = "@acme/renamed-client";
    client_pkg["name"] = json!(new_name);
    client_pkg["version"] = json!("2.1.0");
    put_json(&target, "packages/client/package.json", &client_pkg);

    let mut web_pkg: Value = read_json(&target.join("apps/web/package.json")).unwrap();
    web_pkg["dependencies"] = json!({ new_name: "file:../../packages/client" });
    put_json(&target, "apps/web/package.json", &web_pkg);

    // Inject mid-commit failure first and prove devkit recover rolls back cleanly!
    let pre_sync_lock = fs::read(target.join("pnpm-lock.yaml")).unwrap();
    let interrupted = sync_project(
        &release,
        &target,
        &UpdateOptions {
            hooks: Some(&FailAfterFirstWrite),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap_err();
    assert!(
        interrupted
            .to_string()
            .contains("injected sync write interruption")
    );
    // Automatic rollback inside tx.commit restores pre-sync files; verify clean state.
    assert!(!target.join(PENDING_PATH).exists());
    assert_eq!(
        fs::read(target.join("pnpm-lock.yaml")).unwrap(),
        pre_sync_lock
    );
    assert!(!recover_project(&target).unwrap());

    // Now run sync_project offline (NoResolver proves Docker is NOT started!).
    let synced = sync_project(&release, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert!(synced.applied);
    assert!(!synced.no_op);
    assert_eq!(synced.state.version, initial.version);
    assert_eq!(synced.state.release_identity, initial.release_identity);
    assert_ne!(synced.state.generation, initial.generation);
    assert_eq!(
        synced.state.components["apps/web"].dependencies,
        vec!["packages/client".to_string()]
    );

    let updated_lock = devkit::pnpm::Lock::read(&target).unwrap();
    let importer = &updated_lock.importers["apps/web"];
    assert!(!importer.dependencies.contains_key(&old_name));
    assert_eq!(
        importer.dependencies[new_name].version,
        "link:../../packages/client"
    );
    assert_eq!(
        read_json::<Value>(&target.join("packages/client/package.json")).unwrap()["version"],
        "2.1.0"
    );
    devkit::native::assert_native_components(&target, &synced.state).unwrap();

    // Repeat sync_project must be a true no-op without rotating generation!
    let repeat = sync_project(&release, &target, &UpdateOptions::default(), &NoResolver).unwrap();
    assert!(repeat.no_op);
    assert!(!repeat.applied);
    assert!(repeat.files.is_empty());
    assert_eq!(repeat.state.generation, synced.state.generation);
}

#[test]
fn reconcile_lock_reports_drift_previews_and_climbs_three_stage_ladder() {
    use devkit::{cli::error_code, graph::ReconcileStrategy};
    use std::cell::Cell;

    struct DriftingResolver {
        min_stage: u8,
        calls: Cell<u8>,
    }
    impl LockResolver for DriftingResolver {
        fn regenerate(
            &self,
            root: &Path,
            release: &AdmittedRelease,
            _: Option<&str>,
        ) -> Result<()> {
            let attempt = self.calls.get() + 1;
            self.calls.set(attempt);
            if self.min_stage == 2 && attempt == 1 {
                bail!("ERR_PNPM_PEER_DEP_ISSUES simulated stage-1 conflict on dev tree");
            }
            if self.min_stage == 3 && attempt < 3 {
                if attempt == 2 {
                    let pruned = devkit::pnpm::Lock::read(root)?;
                    assert!(!pruned.packages.contains_key("stale-dev@1.0.0"));
                    assert!(pruned.packages.contains_key("clsx@2.1.0"));
                }
                bail!("ERR_PNPM_PEER_DEP_ISSUES simulated conflict before clean fallback");
            }
            let workspace = devkit::pnpm::Workspace::read(root)?;
            let mut lock = devkit::pnpm::Lock::default();
            for path in std::iter::once(String::new()).chain(workspace.packages) {
                let manifest: Value = read_json(&root.join(&path).join("package.json"))?;
                let mut importer = devkit::pnpm::Importer::default();
                for field in devkit::pnpm::PHYSICAL_FIELDS {
                    for (name, value) in
                        devkit::native::dependency_table(manifest.as_object().unwrap(), field)?
                    {
                        let specifier = value.as_str().unwrap().to_owned();
                        let version = match name.as_str() {
                            "@docouno/env" => release.manifest.version.clone(),
                            "clsx" => "2.1.1".into(),
                            _ => bail!("Unexpected fixture dependency {name}"),
                        };
                        importer
                            .field_mut(field)
                            .insert(name, devkit::pnpm::Dependency { specifier, version });
                    }
                }
                lock.importers
                    .insert(devkit::pnpm::importer_id(&path).into(), importer);
            }
            for (name, version, integrity) in [
                (
                    "@docouno/env",
                    release.manifest.version.as_str(),
                    release.manifest.environment.integrity.clone(),
                ),
                ("clsx", "2.1.1", sri(b"clsx-2.1.1")),
            ] {
                let key = format!("{name}@{version}");
                lock.packages.insert(
                    key.clone(),
                    json!({"resolution":{"integrity":integrity}})
                        .as_object()
                        .unwrap()
                        .clone(),
                );
                lock.snapshots.insert(key, serde_json::Map::new());
            }
            lock.write(root)
        }
    }

    let f = TempDir::new().unwrap();
    let old = fixture_release(f.path(), "0.4.1");
    let next = fixture_release(f.path(), "0.5.0");
    let target = f.path().join("consumer");
    init(&old, &target, "node", PackageMode::Registry);

    let mut main_pkg: Value = read_json(&target.join("apps/main/package.json")).unwrap();
    main_pkg["dependencies"] = json!({"clsx": "^2.1.0"});
    put_json(&target, "apps/main/package.json", &main_pkg);
    let mut lock: Value = read_json(&target.join("package-lock.json")).unwrap();
    lock["packages"]["apps/main"] = main_pkg.clone();
    lock["packages"]["node_modules/clsx"] = json!({
        "version": "2.1.0",
        "resolved": "https://registry.npmjs.org/clsx/-/clsx-2.1.0.tgz",
        "integrity": sri(b"clsx-2.1.0"),
    });
    lock["packages"]["node_modules/stale-dev"] = json!({
        "version": "1.0.0",
        "resolved": "https://registry.npmjs.org/stale-dev/-/stale-dev-1.0.0.tgz",
        "integrity": sri(b"stale-dev"),
        "dev": true,
    });
    put_json(&target, "package-lock.json", &lock);

    // 1. Without --reconcile-lock -> PRODUCT_GRAPH_DRIFT with package diff and exact hint.
    let err = update_project(
        &next,
        &target,
        &UpdateOptions::default(),
        &DriftingResolver {
            min_stage: 1,
            calls: Cell::new(0),
        },
    )
    .unwrap_err();
    assert_eq!(error_code(&err), "PRODUCT_GRAPH_DRIFT");
    let msg = err.to_string();
    assert!(msg.contains("clsx: 2.1.0 -> 2.1.1"), "{msg}");
    assert!(
        msg.contains("Product lockfile nodes drifted during resolution")
            && msg.contains(
                "inspect with --reconcile-lock --preview and apply with --reconcile-lock"
            ),
        "{msg}"
    );

    // 2. With --reconcile-lock --preview -> read-only report, checkout untouched (prune-dev stage 2).
    let before_bytes = fs::read(target.join("package-lock.json")).unwrap();
    let preview = update_project(
        &next,
        &target,
        &UpdateOptions {
            preview: true,
            reconcile_lock: true,
            ..Default::default()
        },
        &DriftingResolver {
            min_stage: 2,
            calls: Cell::new(0),
        },
    )
    .unwrap();
    assert!(!preview.applied);
    let rec = preview.lock_reconciliation.as_ref().unwrap();
    assert_eq!(rec.strategy, ReconcileStrategy::PruneDev);
    assert!(
        rec.changed_packages
            .iter()
            .any(|p| p.name == "clsx" && p.before_version == "2.1.0" && p.after_version == "2.1.1")
    );
    assert_eq!(
        fs::read(target.join("package-lock.json")).unwrap(),
        before_bytes
    );

    // 3. Apply with --reconcile-lock (exercising stage 3 clean-fallback) -> applied & direct specs intact.
    let applied = update_project(
        &next,
        &target,
        &UpdateOptions {
            reconcile_lock: true,
            ..Default::default()
        },
        &DriftingResolver {
            min_stage: 3,
            calls: Cell::new(0),
        },
    )
    .unwrap();
    assert!(applied.applied);
    assert_eq!(
        applied.lock_reconciliation.as_ref().unwrap().strategy,
        ReconcileStrategy::CleanFallback
    );
    let after_pkg: Value = read_json(&target.join("apps/main/package.json")).unwrap();
    assert_eq!(after_pkg["dependencies"]["clsx"], "^2.1.0");
    devkit::native::assert_native_components(&target, &applied.state).unwrap();

    // 4. Non-ERESOLVE failures (e.g. E401 auth failure or signal exit >= 128) abort at Stage 1 without escalating.
    struct NonConflictResolver {
        signal: bool,
        calls: Cell<u8>,
    }
    impl LockResolver for NonConflictResolver {
        fn regenerate(
            &self,
            _root: &Path,
            _release: &AdmittedRelease,
            _registry: Option<&str>,
        ) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.signal {
                Err(devkit::processes::failure(
                    130,
                    "interrupted by SIGINT during ERESOLVE check",
                ))
            } else {
                bail!(
                    "npm error code E401 Unable to authenticate, your authentication token seems to be invalid"
                )
            }
        }
    }
    for signal in [false, true] {
        let resolver = NonConflictResolver {
            signal,
            calls: Cell::new(0),
        };
        let err = update_project(
            &next,
            &target,
            &UpdateOptions {
                reconcile_lock: true,
                ..Default::default()
            },
            &resolver,
        )
        .unwrap_err();
        assert_eq!(
            resolver.calls.get(),
            1,
            "non-conflict/signal error escalated past stage 1: {err:#}"
        );
    }
}

#[test]
fn zero_app_init_design_system_preset_and_use_flag_write_dependencies_cleanly() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");

    // 1. Single --template ts-library creates a 0-app repository at packages/lib.
    let lib_only = f.path().join("lib-only");
    let lib_state = init_project(
        &release,
        &lib_only,
        &InitOptions {
            template: Some("ts-library".into()),
            package_mode: Some(PackageMode::Registry),
            name: Some("acme".into()),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        lib_state.components.keys().cloned().collect::<Vec<_>>(),
        vec!["packages/lib"]
    );
    assert_eq!(
        lib_state.components["packages/lib"].tooling_profile,
        ToolingProfile::TsLibrary
    );
    assert!(lib_state.components["packages/lib"].development.is_none());
    let apps_compose: Value =
        serde_json::from_slice(&fs::read(lib_only.join(".devkit/compose.apps.yaml")).unwrap())
            .unwrap();
    assert_eq!(apps_compose["services"], json!({}));
    let lib_mk = fs::read_to_string(lib_only.join(".devkit/project.mk")).unwrap();
    assert!(lib_mk.contains("DEVKIT_REGISTERED_APPS := \n"), "{lib_mk}");
    assert!(
        lib_mk.contains("DEVKIT_PACKAGEABLE_PACKAGES := lib\n"),
        "{lib_mk}"
    );
    assert!(
        lib_mk.contains("package/packages/lib: build/packages/lib\n"),
        "{lib_mk}"
    );
    let gitignore = fs::read_to_string(lib_only.join(".gitignore")).unwrap();
    assert!(gitignore.contains("/packages/*/dist/\n"), "{gitignore}");
    assert!(gitignore.contains("/packages/*/release/\n"), "{gitignore}");
    let sig_before = source_signature(&lib_only, Some(&lib_state)).unwrap();
    put(&lib_only, "packages/lib/release/sdk-0.1.0.tgz", b"tarball");
    assert_eq!(
        source_signature(&lib_only, Some(&lib_state)).unwrap(),
        sig_before,
        "profile_outputs must exclude packages/*/release from source_signature"
    );

    // 2. Multi-package 0-app init + add-package --use + add-app --use writes to dependencies.
    let multi = f.path().join("multi");
    init_project(
        &release,
        &multi,
        &InitOptions {
            packages: Some(vec![
                ComponentSpec {
                    name: "core".into(),
                    template: "ts-library".into(),
                    uses: vec![],
                },
                ComponentSpec {
                    name: "ui".into(),
                    template: "react-ui".into(),
                    uses: vec!["core".into()],
                },
            ]),
            package_mode: Some(PackageMode::Registry),
            name: Some("suite".into()),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let ui_pkg: Value = read_json(&multi.join("packages/ui/package.json")).unwrap();
    assert_eq!(ui_pkg["dependencies"]["@suite/package-core"], "workspace:*");
    assert!(
        ui_pkg["devDependencies"]
            .get("@suite/package-core")
            .is_none()
    );

    let compose_inventory = capture_compose_inputs(&multi).unwrap();
    let after_add = add_project_component(
        &release,
        &multi,
        Role::App,
        ComponentSpec {
            name: "playground".into(),
            template: "react-web".into(),
            uses: vec!["ui".into(), "core".into()],
        },
        &UpdateOptions {
            compose_inventory: Some(&compose_inventory),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let play_pkg: Value = read_json(&multi.join("apps/playground/package.json")).unwrap();
    assert_eq!(play_pkg["dependencies"]["@suite/package-ui"], "workspace:*");
    assert_eq!(
        play_pkg["dependencies"]["@suite/package-core"],
        "workspace:*"
    );
    assert!(
        play_pkg["devDependencies"]
            .get("@suite/package-ui")
            .is_none()
    );
    assert_eq!(
        after_add.state.components["packages/ui"].dependencies,
        vec!["packages/core"]
    );
    assert_eq!(
        after_add.state.components["apps/playground"].dependencies,
        vec!["packages/core", "packages/ui"]
    );
    assert_eq!(
        fs::read_to_string(multi.join("apps/playground/src/index.ts")).unwrap(),
        "export { answer } from '@suite/package-ui';\n"
    );

    // 3. --preset design-system scaffolds packages/ui + apps/playground without a backend app.
    let ds = f.path().join("ds");
    let ds_state = init_project(
        &release,
        &ds,
        &InitOptions {
            preset: Some("design-system".into()),
            package_mode: Some(PackageMode::Registry),
            name: Some("system".into()),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        ds_state.components.keys().cloned().collect::<Vec<_>>(),
        vec!["apps/playground", "packages/ui"]
    );
    assert_eq!(
        ds_state.components["packages/ui"].tooling_profile,
        ToolingProfile::ReactUi
    );
    assert_eq!(
        ds_state.components["apps/playground"].dependencies,
        vec!["packages/ui"]
    );
    assert_eq!(
        fs::read_to_string(ds.join("apps/playground/src/index.ts")).unwrap(),
        "export { answer } from '@system/package-ui';\n"
    );
}

#[test]
fn project_make_preserves_rules_and_pins_the_plan_without_packageable_libraries() {
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");
    let target = f.path().join("fullstack-web");
    let state = init_project(
        &release,
        &target,
        &InitOptions {
            preset: Some("fullstack-desktop".into()),
            package_mode: Some(PackageMode::Registry),
            name: Some("legacy-compat".into()),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let project_mk = fs::read_to_string(target.join(".devkit/project.mk")).unwrap();
    let (legacy_mk, token) = project_mk.split_once("DEVKIT_EXPECTED_PLAN := ").unwrap();
    assert_eq!(
        token.trim_end(),
        digest(format!("{}\n{legacy_mk}", state.release_identity))
    );
    assert!(
        !legacy_mk.contains("DEVKIT_PACKAGEABLE_PACKAGES"),
        "non-packageable profile set must not emit DEVKIT_PACKAGEABLE_PACKAGES:\n{legacy_mk}"
    );
    assert!(
        !legacy_mk.contains("package/packages/"),
        "non-packageable profile set must not emit library package targets:\n{legacy_mk}"
    );
    assert_eq!(
        legacy_mk,
        "# Managed component registration.\n\
         DEVKIT_REGISTERED_APPS := api desktop web\n\
         DEVKIT_REGISTERED_PACKAGES := client\n\
         DEVKIT_DESKTOP_APPS := desktop\n\
         setup-desktop: setup-node-workspace\n\
         check-desktop: check/packages/client build/packages/client\n\
         test-desktop: build/packages/client\n\
         build-desktop: build/packages/client\n\
         dev-desktop: build/packages/client\n\
         package-desktop: build-desktop\n\
         setup-web: setup-node-workspace\n\
         check-web: check/packages/client build/packages/client\n\
         test-web: build/packages/client\n\
         build-web: build/packages/client\n\
         dev-web: build/packages/client\n\
         setup/packages/client: setup-node-workspace\n"
    );

    // Adding a TsLibrary introduces DEVKIT_PACKAGEABLE_PACKAGES and package/packages/<slug>.
    add_project_component(
        &release,
        &target,
        Role::Library,
        ComponentSpec {
            name: "sdk".into(),
            template: "ts-library".into(),
            uses: vec![],
        },
        &UpdateOptions::default(),
        &FixtureResolver,
    )
    .unwrap();
    let updated_mk = fs::read_to_string(target.join(".devkit/project.mk")).unwrap();
    assert!(
        updated_mk.contains("DEVKIT_PACKAGEABLE_PACKAGES := sdk\n"),
        "{updated_mk}"
    );
    assert!(
        updated_mk.contains("package/packages/sdk: build/packages/sdk\n"),
        "{updated_mk}"
    );
}

#[test]
fn applicable_migrations_filters_by_semver_window_profile_and_component_origin() {
    let temp = TempDir::new().unwrap();
    let rel_040 = fixture_release(temp.path(), "0.4.0");
    let rel_050 = fixture_release(temp.path(), "0.5.0");
    put(
        &rel_050,
        "docs/migrations/0.5.0.md",
        b"# Migration 0.5.0\n\n## Glob Scripts and Single-Pass Checks\n\nBody.\n\n## Rust Notes\n\nBody.\n",
    );
    remove_file(&rel_050.join("release.json")).unwrap();
    let archive = "npm/env-0.5.0.tgz".to_string();
    write_json(
        &rel_050.join("release.json"),
        &Release {
            schema_version: 1,
            version: "0.5.0".into(),
            min_supported_from_version: Some("0.4.0".into()),
            rust_version: "1.99.1".into(),
            environment: Environment {
                name: "@docouno/env".into(),
                version: "0.5.0".into(),
                archive: archive.clone(),
                integrity: sri(&fs::read(rel_050.join(&archive)).unwrap()),
            },
            files: file_map(&rel_050).unwrap(),
            migrations: vec![
                MigrationEntry {
                    id: "glob-scripts-050".into(),
                    version: "0.5.0".into(),
                    severity: MigrationSeverity::Recommended,
                    title: "Adopt glob lint/format scripts".into(),
                    summary: "Update package.json lint and check scripts.".into(),
                    profiles: vec!["web".into(), "node".into()],
                    doc_path: "docs/migrations/0.5.0.md".into(),
                    anchor: "glob-scripts-and-single-pass-checks".into(),
                },
                MigrationEntry {
                    id: "rust-notes-050".into(),
                    version: "0.5.0".into(),
                    severity: MigrationSeverity::Info,
                    title: "Rust notes".into(),
                    summary: "Applies to Rust components.".into(),
                    profiles: vec!["rust".into()],
                    doc_path: "docs/migrations/0.5.0.md".into(),
                    anchor: "rust-notes".into(),
                },
            ],
        },
    )
    .unwrap();
    let admitted_050 = devkit::release::admit_release(&rel_050).unwrap();

    // 1. Web-only 0.4.0 project receives glob-scripts-050 but NOT rust-notes-050.
    let web_proj = temp.path().join("web-proj");
    let web_state_040 = init_project(
        &rel_040,
        &web_proj,
        &InitOptions {
            apps: Some(vec![ComponentSpec {
                name: "web".into(),
                template: "react-web".into(),
                uses: vec![],
            }]),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    let applicable = devkit::project::applicable_migrations(&web_state_040, &admitted_050).unwrap();
    assert_eq!(applicable.len(), 1);
    assert_eq!(applicable[0].id, "glob-scripts-050");
    assert!(
        applicable[0]
            .local_path
            .ends_with("docs/migrations/0.5.0.md#glob-scripts-and-single-pass-checks")
    );
    assert_eq!(
        applicable[0].url,
        "https://github.com/docouno/devkit/blob/v0.5.0/docs/migrations/0.5.0.md#glob-scripts-and-single-pass-checks"
    );

    // 2. Same-version update (0.5.0 -> 0.5.0) returns 0 migrations.
    let mut web_state_050 = web_state_040.clone();
    web_state_050.version = "0.5.0".into();
    assert!(
        devkit::project::applicable_migrations(&web_state_050, &admitted_050)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn umask_worktree_modes_and_transient_sockets_pass_admission_and_sync_without_churn() {
    use std::os::unix::net::UnixListener;

    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.5.1");
    let proj = temp.path().join("umask-proj");
    init_project(
        &release,
        &proj,
        &InitOptions {
            apps: Some(vec![
                ComponentSpec {
                    name: "api".into(),
                    template: "rust-api".into(),
                    uses: vec![],
                },
                ComponentSpec {
                    name: "web".into(),
                    template: "react-web".into(),
                    uses: vec!["sdk".into()],
                },
            ]),
            packages: Some(vec![ComponentSpec {
                name: "sdk".into(),
                template: "ts-library".into(),
                uses: vec![],
            }]),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    // Normalize FixtureResolver's synthetic lockfile to seed_workspace_lock's canonical shape first.
    let initial = sync_project(&release, &proj, &UpdateOptions::default(), &FixtureResolver)
        .unwrap()
        .state;

    // Test both Ubuntu/Debian group-writable worktree modes (umask 0002 -> 0664/0775)
    // and restrictive modes (umask 0077 -> 0600/0700).
    for (reg_mode, exec_mode) in [(0o664, 0o775), (0o600, 0o700)] {
        for (rel, id) in file_map(&proj).unwrap() {
            if rel.starts_with(".devkit/state/") {
                continue;
            }
            let new_mode = if id.mode & 0o111 != 0 {
                exec_mode
            } else {
                reg_mode
            };
            fs::set_permissions(proj.join(rel), fs::Permissions::from_mode(new_mode)).unwrap();
        }

        // Admission succeeds despite non-0644/0755 modes when sha256 + exec class match.
        admit_runtime_project(&proj, "0.5.1").unwrap();

        // Untouched sync is a true no-op (does not mint a new generation or rewrite files).
        let noop = sync_project(&release, &proj, &UpdateOptions::default(), &NoResolver).unwrap();
        assert!(noop.no_op);
        assert!(!noop.applied);
        assert!(noop.files.is_empty());
        assert_eq!(noop.state.generation, initial.generation);
    }

    // Flipping executable class on a managed file MUST still fail with Managed file conflict.
    let tc_path = proj.join(".devkit/runtime/toolchains.env");
    fs::set_permissions(&tc_path, fs::Permissions::from_mode(0o755)).unwrap();
    let err = match admit_runtime_project(&proj, "0.5.1") {
        Ok(_) => panic!("expected Managed file conflict when executable bit flips"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("Managed file conflict"));
    fs::set_permissions(&tc_path, fs::Permissions::from_mode(0o664)).unwrap();

    // Editing one product package.json under 0664 modes only reports changed files in --preview,
    // never churning untouched .devkit/* files.
    for (rel, id) in file_map(&proj).unwrap() {
        if rel.starts_with(".devkit/state/") {
            continue;
        }
        let m = if id.mode & 0o111 != 0 { 0o775 } else { 0o664 };
        fs::set_permissions(proj.join(rel), fs::Permissions::from_mode(m)).unwrap();
    }
    let mut sdk_pkg: Value = read_json(&proj.join("packages/sdk/package.json")).unwrap();
    sdk_pkg["description"] = json!("Updated under umask 0002");
    put_json(&proj, "packages/sdk/package.json", &sdk_pkg);
    fs::set_permissions(
        proj.join("packages/sdk/package.json"),
        fs::Permissions::from_mode(0o664),
    )
    .unwrap();

    // Also place a live Unix domain socket and a FIFO inside the project tree to verify
    // source_signature and copy_tree ignore transient IPC nodes cleanly.
    let sock_path = proj.join("apps/web/transient.sock");
    let _listener = UnixListener::bind(&sock_path).unwrap();
    let fifo_path = proj.join("apps/web/transient.fifo");
    let c_fifo = std::ffi::CString::new(fifo_path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);

    let preview = sync_project(
        &release,
        &proj,
        &UpdateOptions {
            preview: true,
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert!(!preview.no_op);
    assert!(!preview.applied);
    assert!(
        !preview
            .files
            .iter()
            .any(|f| f.path.starts_with(".devkit/runtime/")),
        "untouched .devkit/runtime/* files must not appear in preview: {:?}",
        preview.files
    );

    let applied =
        sync_project(&release, &proj, &UpdateOptions::default(), &FixtureResolver).unwrap();
    assert!(applied.applied);
    admit_runtime_project(&proj, "0.5.1").unwrap();
}

#[test]
fn project_mutations_enforce_supported_floor_and_release_min_from_before_lock() {
    use devkit::cli::error_code;

    let temp = TempDir::new().unwrap();
    let rel_060 = fixture_release(temp.path(), "0.6.0");

    for (label, manifest_val) in [
        ("schema1", json!({"schemaVersion": 1, "version": "0.1.0"})),
        ("v021", json!({"schemaVersion": 2, "version": "0.2.1"})),
        ("v032", json!({"schemaVersion": 2, "version": "0.3.2"})),
        ("v041", json!({"schemaVersion": 2, "version": "0.4.1"})),
    ] {
        let proj = temp.path().join(label);
        fs::create_dir_all(proj.join(".devkit")).unwrap();
        put_json(&proj, MANIFEST_PATH, &manifest_val);
        let snapshot = file_map(&proj).unwrap();

        let upd_err =
            update_project(&rel_060, &proj, &UpdateOptions::default(), &NoResolver).unwrap_err();
        assert_eq!(error_code(&upd_err), "UNSUPPORTED_UPGRADE_PATH");
        assert!(
            upd_err
                .to_string()
                .contains("npx @docouno/devkit@0.5.1 update . --to 0.5.1")
        );

        let sync_err =
            sync_project(&rel_060, &proj, &UpdateOptions::default(), &NoResolver).unwrap_err();
        assert_eq!(error_code(&sync_err), "UNSUPPORTED_PROJECT_VERSION");

        let add_err = add_project_component(
            &rel_060,
            &proj,
            Role::App,
            ComponentSpec {
                name: "extra".into(),
                template: "rust-api".into(),
                uses: vec![],
            },
            &UpdateOptions::default(),
            &NoResolver,
        )
        .unwrap_err();
        assert_eq!(error_code(&add_err), "UNSUPPORTED_PROJECT_VERSION");

        let reg_err = register_project_component(
            &rel_060,
            &proj,
            Role::Library,
            "shared".into(),
            ToolingProfile::Rust,
            &UpdateOptions::default(),
            &NoResolver,
        )
        .unwrap_err();
        assert_eq!(error_code(&reg_err), "UNSUPPORTED_PROJECT_VERSION");

        assert!(!proj.join(".devkit/state").exists(), "{label}");
        assert_eq!(file_map(&proj).unwrap(), snapshot, "{label}");
    }

    // An individual release can require a newer starting point than the CLI floor.
    let rel_strict = fixture_release(temp.path(), "0.6.1");
    let mut strict_json: Value = read_json(&rel_strict.join("release.json")).unwrap();
    strict_json["minSupportedFromVersion"] = json!("0.5.1");
    put_json(&rel_strict, "release.json", &strict_json);

    let proj_050 = temp.path().join("proj-050");
    fs::create_dir_all(proj_050.join(".devkit")).unwrap();
    put_json(
        &proj_050,
        MANIFEST_PATH,
        &json!({"schemaVersion": 2, "version": "0.5.0"}),
    );
    let strict_err = update_project(
        &rel_strict,
        &proj_050,
        &UpdateOptions::default(),
        &NoResolver,
    )
    .unwrap_err();
    assert_eq!(error_code(&strict_err), "UNSUPPORTED_UPGRADE_PATH");
    assert!(!proj_050.join(".devkit/state").exists());
}

#[test]
fn fullstack_preset_and_api_add_are_transactional_and_separate_from_library_dependencies() {
    struct ApiInventory;
    impl ComposeNormalizer for ApiInventory {
        fn normalize(&self, _: &Path, _: &[String]) -> Result<Value> {
            Ok(json!({"services":{"api":{},"web":{}}}))
        }
    }
    let f = TempDir::new().unwrap();
    let release = fixture_release(f.path(), "0.5.0");
    let root = f.path().join("connected");
    let state = init_project(
        &release,
        &root,
        &InitOptions {
            preset: Some("fullstack-web".into()),
            package_mode: Some(PackageMode::Local),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        state
            .components
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["apps/api", "apps/web"]
    );
    assert_eq!(
        state.components["apps/web"]
            .development
            .as_ref()
            .unwrap()
            .api
            .as_deref(),
        Some("apps/api")
    );
    assert!(state.components["apps/web"].dependencies.is_empty());
    let projection = devkit::projections::render_app_compose_value(&state).unwrap();
    assert_eq!(
        projection["services"]["web"]["depends_on"]["api"],
        json!({"condition":"service_started","required":true})
    );
    assert_eq!(
        projection["services"]["api"]["env_file"],
        json!([".devkit/runtime/api.env"])
    );
    assert!(
        projection["services"]["api"]["environment"]
            .get("HOST")
            .is_none()
    );
    assert!(
        projection["services"]["api"]["environment"]
            .get("PORT")
            .is_none()
    );
    assert!(projection["services"]["web"].get("env_file").is_none());
    let mut historical = state.clone();
    historical.version = "0.4.1".into();
    assert!(
        devkit::projections::render_app_compose_value(&historical).unwrap()["services"]["web"]
            .get("depends_on")
            .is_none()
    );
    for api in ["missing", "web", "added"] {
        let before = author_port_payload(&root);
        let inventory = admit_compose_inventory(&root, &ApiInventory).unwrap();
        assert!(
            add_project_component(
                &release,
                &root,
                Role::App,
                ComponentSpec {
                    name: "added".into(),
                    template: "react-web".into(),
                    uses: vec![]
                },
                &UpdateOptions {
                    api: Some(api.into()),
                    compose_inventory: Some(&inventory),
                    ..Default::default()
                },
                &FixtureResolver
            )
            .is_err()
        );
        assert_eq!(author_port_payload(&root), before);
        assert!(!root.join("apps/added").exists());
        assert!(!root.join(PENDING_PATH).exists());
    }
    let inventory = admit_compose_inventory(&root, &ApiInventory).unwrap();
    let outcome = add_project_component(
        &release,
        &root,
        Role::App,
        ComponentSpec {
            name: "added".into(),
            template: "react-web".into(),
            uses: vec![],
        },
        &UpdateOptions {
            api: Some("api".into()),
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &FixtureResolver,
    )
    .unwrap();
    assert_eq!(
        outcome.state.components["apps/added"]
            .development
            .as_ref()
            .unwrap()
            .api
            .as_deref(),
        Some("apps/api")
    );
    assert!(
        outcome.state.components["apps/added"]
            .dependencies
            .is_empty()
    );
    let mut state = outcome.state;
    state.components.get_mut("apps/api").unwrap().dependencies = vec!["packages/server".into()];
    state.components.get_mut("apps/added").unwrap().dependencies = vec!["packages/ui".into()];
    let make = devkit::projections::render_project_make(&state);
    assert!(make.contains("dev-added: build/packages/server build/packages/ui\n"));
    assert!(!make.contains("build-added: build/packages/server"));
}
