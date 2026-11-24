#[path = "support/runtime.rs"]
mod runtime_fixture;

use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    files::{read_json, write_atomic, write_json},
    graph::{
        LockSeed, assert_product_graphs_preserved, capture_product_graph, read_native_graph,
        seed_workspace_lock,
    },
    model::{
        ComponentState, Environment, PackageMode, ProjectState, Release, Role, ToolingProfile,
    },
    native::{
        NodeLockKind, admit_native_components, assert_input_identities, assert_native_components,
        assert_npmrc_compatibility, collect_native_input_closure, derive_component_dependencies,
        package_lock, parse_and_admit_npmrc, valid_integrity,
    },
    project::{
        ComponentSpec, InitOptions, LockResolver, UpdateOptions, init_project,
        register_project_component, update_project,
    },
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
    let version = "0.5.0";
    runtime_fixture::write_runtime(&release, version, &devkit::files::digest(b"fixture-engine"));
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
            "":node,"node_modules/@docouno/env":{"version":version,"resolved":"https://registry.npmjs.org/@docouno/env/-/env-0.5.0.tgz","integrity":environment_integrity}
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
    let pin = devkit::runtime::PnpmPin::from_source_pins(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap(),
    )
    .unwrap();
    put(&release, "runtime/pnpm.env", &pin.encode());
    write_json(
        &release.join("release.json"),
        &Release {
            schema_version: 1,
            version: version.into(),
            min_supported_from_version: Some("0.4.0".into()),
            rust_version: "1.99.1".into(),
            environment: Environment {
                name: "@docouno/env".into(),
                version: version.into(),
                archive: "npm/env.tgz".into(),
                integrity: environment_integrity,
            },
            files: file_map(&release).unwrap(),
            migrations: Vec::new(),
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
        if root.join("pnpm-lock.yaml").is_file() {
            devkit::pnpm::Lock::read(root)?;
        } else {
            package_lock(root)?;
        }
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
        "origin":{"template":"adopted","version":"0.4.1"},"toolDependencies":{},"dependencies":[]
    })).unwrap()
}
fn state(components: &[(&str, &str)]) -> ProjectState {
    serde_json::from_value(json!({"schemaVersion":2,"version":"0.4.1","releaseIdentity":"0".repeat(64),
        "generation":"00000000-0000-4000-8000-000000000000","packageMode":"registry","managed":{},
        "components":components.iter().map(|(p,t)| (p.to_string(),serde_json::to_value(component(p,t)).unwrap())).collect::<BTreeMap<_,_>>()
    })).unwrap()
}
fn manifest(name: &str, deps: Value, library: bool) -> Value {
    let mut value = json!({"name":name,"version":"1.0.0","dependencies":deps,
        "scripts":{"check":"check","fix":"fix","test":"test","build":"build","dev":"dev"}});
    if library {
        value["private"] = json!(true);
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

#[test]
fn pnpm_yaml_budget_precedes_native_admission_and_identity_rechecks() {
    const WORKER: &str = "DEVKIT_TEST_PNPM_BUDGET_WORKER";
    if std::env::var(WORKER).as_deref() != Ok("1") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pnpm_yaml_budget_precedes_native_admission_and_identity_rechecks",
                "--test-threads=1",
            ])
            .env(WORKER, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let pages = fs::read_to_string("/proc/self/statm")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    // SAFETY: the isolated worker lowers only its own address-space limit;
    // sparse rejected inputs must never allocate their declared GiB size.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let maximum = pages * page_size + 96 * 1024 * 1024;
    let limit = libc::rlimit {
        rlim_cur: maximum,
        rlim_max: maximum,
    };
    // SAFETY: a live rlimit pointer and this disposable worker process only.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) }, 0);
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let mut state = state(&[("apps/main", "node")]);
    state.version = "0.5.0".into();
    node_project(
        root,
        &state,
        &[("apps/main", manifest("main", json!({}), false))],
        json!({}),
    );
    let packages = package_lock(root).unwrap();
    devkit::pnpm::import_legacy_npm_lock_seed(root, &packages)
        .unwrap()
        .write(root)
        .unwrap();
    fs::remove_file(root.join("package-lock.json")).unwrap();
    let mut root_manifest: Value = read_json(&root.join("package.json")).unwrap();
    root_manifest.as_object_mut().unwrap().remove("workspaces");
    put_json(root, "package.json", &root_manifest);
    devkit::pnpm::write_yaml(
        root,
        "pnpm-workspace.yaml",
        &devkit::pnpm::Workspace::new(vec!["apps/main".into()]),
    )
    .unwrap();
    let admitted = admit_native_components(root, &state).unwrap();
    for name in ["pnpm-workspace.yaml", "pnpm-lock.yaml"] {
        let path = root.join(name);
        let original = fs::read(&path).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1024 * 1024 * 1024)
            .unwrap();
        let error = admit_native_components(root, &state).err().unwrap();
        assert!(
            error.to_string().contains("pnpm YAML exceeds byte budget"),
            "{error:#}"
        );
        let error = assert_input_identities(root, &admitted.inputs).unwrap_err();
        assert!(
            error.to_string().contains("pnpm YAML exceeds byte budget"),
            "{error:#}"
        );
        fs::write(path, original).unwrap();
    }
    assert_input_identities(root, &admitted.inputs).unwrap();

    // The public lifecycle snapshot and numeric journal slots must retain the
    // same input budget, even when no native parser is about to read them.
    let (_, project, _) = runtime_project(&root.join("lifecycle"), "node-api");
    let snapshot = devkit::project::AdmissionSnapshot::capture(&project).unwrap();
    let yaml = project.join("pnpm-workspace.yaml");
    let original = fs::read(&yaml).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&yaml)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    assert!(
        snapshot
            .recheck()
            .unwrap_err()
            .to_string()
            .contains("pnpm YAML exceeds byte budget")
    );
    fs::write(&yaml, &original).unwrap();
    snapshot.recheck().unwrap();

    put_json(
        &project,
        "apps/extra/package.json",
        &manifest("extra", json!({}), false),
    );
    let adopted_lock = project.join("apps/extra/pnpm-lock.yaml");
    fs::File::create(&adopted_lock)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    assert!(
        devkit::project::AdmissionSnapshot::capture(&project)
            .err()
            .unwrap()
            .to_string()
            .contains("pnpm YAML exceeds byte budget")
    );
    fs::remove_dir_all(project.join("apps/extra")).unwrap();

    let staged = root.join("transaction-source");
    devkit::files::copy_tree(
        &project,
        &staged,
        &|name| devkit::state::excluded_snapshot_path(name),
        true,
    )
    .unwrap();
    for name in ["pnpm-workspace.yaml", devkit::state::MANIFEST_PATH] {
        let path = staged.join(name);
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }
    let paths = [
        "pnpm-workspace.yaml".to_owned(),
        devkit::state::MANIFEST_PATH.to_owned(),
    ]
    .into_iter()
    .collect();
    let stage = ".devkit/state/update-00000000-0000-4000-8000-000000000001";
    fs::OpenOptions::new()
        .write(true)
        .open(&yaml)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    assert!(
        devkit::transaction::PreparedTransaction::prepare(&project, stage, &staged, &paths, vec![])
            .err()
            .unwrap()
            .to_string()
            .contains("pnpm YAML exceeds byte budget")
    );
    fs::write(&yaml, original).unwrap();
    let tx =
        devkit::transaction::PreparedTransaction::prepare(&project, stage, &staged, &paths, vec![])
            .unwrap();
    write_atomic(&project.join(".devkit/state/task.lock"), b"", 0o644).unwrap();
    let file = tx
        .journal
        .files
        .iter()
        .find(|file| file.path == "pnpm-workspace.yaml")
        .unwrap();
    for saved in [&file.staged, &file.backup] {
        let path = project.join(saved.as_ref().unwrap());
        let bytes = fs::read(&path).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1024 * 1024 * 1024)
            .unwrap();
        assert!(
            tx.commit(&project, None, &|| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("pnpm YAML exceeds byte budget")
        );
        write_json(&project.join(devkit::state::PENDING_PATH), &tx.journal).unwrap();
        assert!(
            devkit::transaction::recover_project(&project)
                .unwrap_err()
                .to_string()
                .contains("pnpm YAML exceeds byte budget")
        );
        fs::write(path, bytes).unwrap();
        fs::remove_file(project.join(devkit::state::PENDING_PATH)).unwrap();
    }
    write_json(&project.join(devkit::state::PENDING_PATH), &tx.journal).unwrap();
    assert!(devkit::transaction::recover_project(&project).unwrap());
    devkit::project::AdmissionSnapshot::capture(&project).unwrap();
}

#[test]
fn npmrc_policy_versions_the_complete_reserved_namespace_and_fetch_allowlist() {
    use devkit::native::parse_manifest_npmrc_policy;
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    for variable in ["PNPM_PRIVATE_TOKEN", "XDG_REGISTRY_TOKEN", "REGISTRY_TOKEN"] {
        put(
            root,
            ".npmrc",
            &format!("//registry.example/:_authToken=${{{variable}}}\n"),
        );
        assert!(parse_manifest_npmrc_policy(root, NodeLockKind::Npm).is_ok());
        assert_eq!(
            parse_manifest_npmrc_policy(root, NodeLockKind::Pnpm).is_ok(),
            variable == "REGISTRY_TOKEN"
        );
    }
    for setting in ["fetch-retries=2", "fetch-timeout=100"] {
        put(root, ".npmrc", setting);
        assert!(
            parse_manifest_npmrc_policy(root, NodeLockKind::Npm)
                .unwrap_err()
                .to_string()
                .contains("Unsupported npm configuration key")
        );
        parse_manifest_npmrc_policy(root, NodeLockKind::Pnpm).unwrap();
    }
    for variable in [
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "PWD",
        "TMPDIR",
        "CI",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "GOMAXPROCS",
        "UV_THREADPOOL_SIZE",
        "LD_TOKEN",
        "DYLD_TOKEN",
        "NODE_TOKEN",
        "NPM_CONFIG_TOKEN",
        "DEVKIT_TOKEN",
        "CARGO_TOKEN",
        "RUST_TOKEN",
        "DOCKER_TOKEN",
        "GIT_TOKEN",
        "SSH_TOKEN",
        "SSL_TOKEN",
        "RAYON_TOKEN",
        "ROLLDOWN_TOKEN",
        "VITEST_TOKEN",
        "TOKIO_TOKEN",
    ] {
        put(
            root,
            ".npmrc",
            &format!("//registry.example/:_authToken=${{{variable}}}\n"),
        );
        for kind in [NodeLockKind::Npm, NodeLockKind::Pnpm] {
            assert!(
                parse_manifest_npmrc_policy(root, kind)
                    .unwrap_err()
                    .to_string()
                    .contains("Reserved environment variable"),
                "{kind:?}: {variable}"
            );
        }
    }
}

#[test]
fn directory_snapshots_project_physical_component_roles_dependencies_and_cycles() {
    for target in ["apps/secondary", "packages/core"] {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        let mut state = state(&[("apps/main", "node"), (target, "node")]);
        state.version = "0.5.0".into();
        if target.starts_with("packages/") {
            state.components.get_mut("apps/main").unwrap().dependencies = vec![target.into()];
        }
        let relative = if target.starts_with("apps/") {
            "../secondary"
        } else {
            "../../packages/core"
        };
        node_project(
            root,
            &state,
            &[
                (
                    "apps/main",
                    manifest("main", json!({"target":format!("file:{relative}")}), false),
                ),
                (
                    target,
                    manifest("target", json!({}), target.starts_with("packages/")),
                ),
            ],
            json!({}),
        );
        let mut lock =
            devkit::pnpm::import_legacy_npm_lock_seed(root, &package_lock(root).unwrap()).unwrap();
        let key = format!("target@file:{target}");
        lock.importers
            .get_mut("apps/main")
            .unwrap()
            .dependencies
            .get_mut("target")
            .unwrap()
            .version = format!("file:{target}");
        lock.packages.insert(
            key.clone(),
            json!({"resolution":{"type":"directory","directory":target}})
                .as_object()
                .unwrap()
                .clone(),
        );
        lock.snapshots.insert(key, serde_json::Map::new());
        lock.write(root).unwrap();
        fs::remove_file(root.join("package-lock.json")).unwrap();
        devkit::project::sync_node_workspace(root, &state, None).unwrap();
        if target.starts_with("apps/") {
            assert!(
                admit_native_components(root, &state)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("Application dependency is forbidden")
            );
        } else {
            assert_eq!(
                admit_native_components(root, &state).unwrap().dependencies["apps/main"],
                vec![target.to_owned()]
            );
            assert_eq!(
                devkit::native::local_dependency_map(root, &state, "apps/main").unwrap()["target"],
                target
            );
            let mut core: Value = read_json(&root.join("packages/core/package.json")).unwrap();
            core["dependencies"]["main"] = json!("file:../../apps/main");
            put_json(root, "packages/core/package.json", &core);
            let entry = devkit::pnpm::Dependency {
                specifier: "file:../../apps/main".into(),
                version: "file:apps/main".into(),
            };
            lock.importers
                .get_mut("packages/core")
                .unwrap()
                .dependencies
                .insert("main".into(), entry);
            lock.packages.insert(
                "main@file:apps/main".into(),
                json!({"resolution":{"type":"directory","directory":"apps/main"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            lock.snapshots
                .insert("main@file:apps/main".into(), serde_json::Map::new());
            lock.write(root).unwrap();
            assert!(
                derive_component_dependencies(root, &state)
                    .unwrap_err()
                    .to_string()
                    .contains("Application dependency is forbidden")
            );
        }
    }
}

#[test]
fn directory_snapshot_library_edges_cannot_hide_a_component_cycle() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let mut state = state(&[("packages/a", "node"), ("packages/b", "node")]);
    state.version = "0.5.0".into();
    node_project(
        root,
        &state,
        &[
            ("packages/a", manifest("a", json!({"b":"file:../b"}), true)),
            ("packages/b", manifest("b", json!({"a":"file:../a"}), true)),
        ],
        json!({}),
    );
    let mut lock =
        devkit::pnpm::import_legacy_npm_lock_seed(root, &package_lock(root).unwrap()).unwrap();
    for (from, name) in [("packages/a", "b"), ("packages/b", "a")] {
        let target = format!("packages/{name}");
        lock.importers
            .get_mut(from)
            .unwrap()
            .dependencies
            .get_mut(name)
            .unwrap()
            .version = format!("file:{target}");
        let key = format!("{name}@file:{target}");
        lock.packages.insert(
            key.clone(),
            json!({"resolution":{"type":"directory","directory":target}})
                .as_object()
                .unwrap()
                .clone(),
        );
        lock.snapshots.insert(key, serde_json::Map::new());
    }
    lock.write(root).unwrap();
    fs::remove_file(root.join("package-lock.json")).unwrap();
    devkit::project::sync_node_workspace(root, &state, None).unwrap();
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("Component dependency cycle")
    );
}

#[test]
fn legacy_authority_ignores_shadow_pnpm_files_and_keeps_its_exact_npmrc_policy() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[("apps/main", manifest("main", json!({}), false))],
        json!({}),
    );
    let before = admit_native_components(root, &state).unwrap().inputs;
    for name in [
        "pnpm-workspace.yaml",
        "pnpm-lock.yaml",
        ".pnpmfile.cjs",
        "apps/main/.pnpmfile.mjs",
    ] {
        put(root, name, "unrelated legacy product file\n");
    }
    let after = admit_native_components(root, &state).unwrap().inputs;
    assert_eq!(before.digest, after.digest);
    read_native_graph(root, NodeLockKind::Npm).unwrap();
    put(
        root,
        ".npmrc",
        "package-lock=true\ninstall-strategy=hoisted\n",
    );
    admit_native_components(root, &state).unwrap();
    for setting in [
        "node-linker=isolated",
        "package-import-method=hardlink",
        "strict-peer-dependencies=true",
        "auto-install-peers=true",
    ] {
        put(root, ".npmrc", setting);
        assert!(
            admit_native_components(root, &state)
                .err()
                .unwrap()
                .to_string()
                .contains("Unsupported npm configuration key"),
            "{setting}"
        );
    }
}

#[test]
fn pnpm_admission_tracks_workspace_links_and_rejects_competing_locks_and_configuration() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let mut state = state(&[("apps/web", "node"), ("packages/ui", "node")]);
    state.version = "0.5.0".into();
    state.components.get_mut("apps/web").unwrap().dependencies = vec!["packages/ui".into()];
    node_project(
        root,
        &state,
        &[
            (
                "apps/web",
                manifest("web", json!({"ui":"workspace:*"}), false),
            ),
            ("packages/ui", manifest("ui", json!({}), true)),
        ],
        json!({}),
    );
    let packages = package_lock(root).unwrap();
    devkit::project::sync_node_workspace(root, &state, None).unwrap();
    let lock = devkit::pnpm::import_legacy_npm_lock_seed(root, &packages).unwrap();
    lock.write(root).unwrap();
    fs::remove_file(root.join("package-lock.json")).unwrap();
    let admitted = admit_native_components(root, &state).unwrap();
    assert_eq!(admitted.dependencies["apps/web"], vec!["packages/ui"]);
    assert!(admitted.node_inputs.files.contains_key("pnpm-lock.yaml"));
    assert!(
        admitted
            .node_inputs
            .files
            .contains_key("pnpm-workspace.yaml")
    );
    put(root, "package-lock.json", "{}");
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("Competing root lockfile")
    );
    fs::remove_file(root.join("package-lock.json")).unwrap();
    put(
        root,
        ".npmrc",
        "//registry.npmjs.org/:_authToken=${PNPM_FORBIDDEN}\n",
    );
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("Reserved environment variable")
    );
    fs::remove_file(root.join(".npmrc")).unwrap();
    assert_eq!(admitted.node_inputs.files[".pnpmfile.cjs"], None);
    for directory in ["", "apps/web/", "packages/ui/"] {
        for name in [".pnpmfile.cjs", ".pnpmfile.mjs", ".pnpmfile.js"] {
            let name = format!("{directory}{name}");
            put(
                root,
                &name,
                "throw new Error('unsupported hook must not execute');\n",
            );
            assert!(assert_input_identities(root, &admitted.node_inputs).is_err());
            assert!(
                admit_native_components(root, &state)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("Unsupported pnpm hook")
            );
            fs::remove_file(root.join(name)).unwrap();
        }
    }
    put(root, "apps/web/pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("workspace context: apps/web/pnpm-lock.yaml")
    );
}
fn rust(root: &Path, path: &str, contents: &str, registered: bool) {
    put(root, &format!("{path}/Cargo.toml"), contents);
    if registered {
        // Seed the std-only package identity, as Cargo does. Tests selecting
        // dependencies or workspace members supply their explicit lock graph.
        let manifest: toml::Value = toml::from_str(contents).unwrap();
        let packages = manifest.get("package").map_or_else(Vec::new, |package| {
            vec![json!({"name":package["name"].as_str().unwrap(),
                "version":package.get("version").and_then(toml::Value::as_str).unwrap_or("0.0.0")})]
        });
        selected_cargo_lock(root, path, json!(packages));
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
        assert_npmrc_compatibility(
            root,
            &second.directories,
            NodeLockKind::for_state(&state).unwrap()
        )
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
fn cargo_virtual_workspace_context_does_not_change_component_lock_authority() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = state(&[("apps/api", "rust"), ("packages/common", "rust")]);
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
    selected_cargo_lock(
        root,
        "apps/api",
        json!([{"name":"api","version":"1.0.0","dependencies":["common"]},{"name":"common","version":"1.0.0"}]),
    );
    for version in ["0.4.1", "0.5.0"] {
        state.version = version.into();
        assert!(
            derive_component_dependencies(root, &state)
                .unwrap_err()
                .to_string()
                .contains("component-root workspace/lock authority")
        );
    }
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
    assert_product_graphs_preserved(
        &before,
        &read_native_graph(root, NodeLockKind::Npm).unwrap(),
    )
    .unwrap();
    let mut changed = relocated.clone();
    changed["packages"]["node_modules/shared"]["integrity"] = json!(sri(9));
    put_json(root, "package-lock.json", &changed);
    assert!(
        assert_product_graphs_preserved(
            &before,
            &read_native_graph(root, NodeLockKind::Npm).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("artifact changed")
    );
    let mut split = relocated.clone();
    split["packages"]["node_modules/b/node_modules/shared"] =
        split["packages"]["node_modules/shared"].clone();
    put_json(root, "package-lock.json", &split);
    assert!(
        assert_product_graphs_preserved(
            &before,
            &read_native_graph(root, NodeLockKind::Npm).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("shared dependency split")
    );
    let mut topology = relocated;
    topology["packages"]["node_modules/b"]["dependencies"]["extra"] = json!("1.0.0");
    topology["packages"]["node_modules/extra"] = artifact("extra", json!({}));
    put_json(root, "package-lock.json", &topology);
    assert!(
        assert_product_graphs_preserved(
            &before,
            &read_native_graph(root, NodeLockKind::Npm).unwrap()
        )
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
    assert_product_graphs_preserved(
        &before,
        &read_native_graph(root, NodeLockKind::Npm).unwrap(),
    )
    .unwrap();
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
            assert_product_graphs_preserved(
                &before,
                &read_native_graph(root, NodeLockKind::Npm).unwrap()
            )
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
        let mut lock = devkit::pnpm::Lock::read(&root).unwrap();
        let mut member: Value = read_json(&root.join("apps/main/package.json")).unwrap();
        if location == "root" {
            let mut manifest: Value = read_json(&root.join("package.json")).unwrap();
            manifest["dependencies"] = json!({"missing-required":"1.0.0"});
            put_json(&root, "package.json", &manifest);
        } else if location == "transitive" {
            member["dependencies"] = json!({"parent":"1.0.0"});
            lock.importers
                .get_mut("apps/main")
                .unwrap()
                .dependencies
                .insert(
                    "parent".into(),
                    devkit::pnpm::Dependency {
                        specifier: "1.0.0".into(),
                        version: "1.0.0".into(),
                    },
                );
            lock.packages.insert(
                "parent@1.0.0".into(),
                json!({"resolution":{"integrity":sri(1)}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            lock.snapshots.insert(
                "parent@1.0.0".into(),
                json!({"dependencies":{"missing-required":"1.0.0"}})
                    .as_object()
                    .unwrap()
                    .clone(),
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
        lock.write(&root).unwrap();
        let diagnostic = if location == "transitive" {
            "Native lock target is missing"
        } else {
            "Native lock omits dependency"
        };
        let before = source_signature(&root, Some(&state)).unwrap();
        assert!(
            admit_native_components(&root, &state)
                .err()
                .unwrap()
                .to_string()
                .contains(diagnostic),
            "{location}"
        );
        assert!(
            admit_runtime_project(&root, "0.5.0")
                .err()
                .unwrap()
                .to_string()
                .contains(diagnostic),
            "{location}"
        );
        let error = update_project(&release, &root, &UpdateOptions::default(), &RefuseResolver)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains(diagnostic),
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
    read_native_graph(root, NodeLockKind::Npm).unwrap();
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
            read_native_graph(root, NodeLockKind::Npm)
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
        let admitted = admit_runtime_project(&root, "0.5.0").unwrap();
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
            admit_runtime_project(&root, "0.5.0")
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
            admit_runtime_project(&root, "0.5.0")
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
    let admitted = admit_runtime_project(&root, "0.5.0").unwrap();
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
        admit_runtime_project(&root, "0.5.0")
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
        let admitted = if form == "workspace-patch" {
            // The enclosing workspace is still part of the conservative input
            // closure, but its root lock cannot admit this registered member.
            assert!(
                admit_native_components(root, &state)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("component-root workspace/lock authority")
            );
            None
        } else {
            Some(admit_native_components(root, &state).unwrap())
        };
        let inputs = admitted.as_ref().map_or_else(
            || collect_native_input_closure(root, &state).unwrap(),
            |admitted| admitted.inputs.clone(),
        );
        let recheck = || match &admitted {
            Some(admitted) => admitted.recheck(root),
            None => assert_input_identities(root, &inputs),
        };
        assert!(
            inputs.files["vendor/patched/Cargo.toml"].is_some(),
            "{form}"
        );
        assert!(inputs.files["vendor/leaf/Cargo.toml"].is_some(), "{form}");
        assert_eq!(
            inputs.files["vendor/patched/.cargo/config.toml"], None,
            "{form}"
        );
        put(root, "vendor/leaf/.cargo/config.toml", "[build]\njobs=1\n");
        assert!(recheck().is_err(), "{form}");
        fs::remove_file(root.join("vendor/leaf/.cargo/config.toml")).unwrap();
        recheck().unwrap();
        put(
            root,
            "vendor/patched/Cargo.toml",
            "[package]\nname=\"patched\"\nversion=\"1.0.0\"\nedition=\"2021\"\n",
        );
        assert!(recheck().is_err(), "{form}");
        assert_ne!(
            inputs.digest,
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

fn selected_cargo_lock(root: &Path, path: &str, packages: Value) {
    put(
        root,
        &format!("{path}/Cargo.lock"),
        &toml::to_string(&json!({"version":4,"package":packages})).unwrap(),
    );
}

#[test]
fn internal_cargo_roots_require_an_exact_locked_package() {
    for path in ["apps/main", "packages/main"] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let mut state = state(&[(path, "rust")]);
        state.version = "0.5.0".into();
        rust(
            root,
            path,
            "[package]\nname=\"main\"\nversion=\"1.0.0\"\n",
            true,
        );
        profiles(root, &state);
        selected_cargo_lock(root, path, json!([{"name":"main","version":"1.0.0"}]));
        assert!(admit_native_components(root, &state).is_ok());
        for (case, contents) in [
            ("malformed", "not a Cargo lock".to_owned()),
            ("missing-records", "version = 4\n".to_owned()),
            ("wrong-root", toml::to_string(&json!({"version":4,"package":[{"name":"wrong","version":"0.0.0"}]})).unwrap()),
            ("wrong-version", toml::to_string(&json!({"version":4,"package":[{"name":"main","version":"2.0.0"}]})).unwrap()),
            ("registry-root", toml::to_string(&json!({"version":4,"package":[{"name":"main","version":"1.0.0","source":"registry+https://example.invalid/index"}]})).unwrap()),
            ("duplicate-root", toml::to_string(&json!({"version":4,"package":[{"name":"main","version":"1.0.0"},{"name":"main","version":"1.0.0"}]})).unwrap()),
        ] {
            put(root, &format!("{path}/Cargo.lock"), &contents);
            assert!(
                admit_native_components(root, &state).is_err(),
                "{path}/{case}: admitted a lock without a unique matching local package root"
            );
            assert_eq!(fs::read_to_string(root.join(path).join("Cargo.lock")).unwrap(), contents);
        }
    }
}

#[test]
fn internal_cargo_chain_validates_every_reachable_lock_reference() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let path = "apps/main";
    let mut state = state(&[(path, "rust")]);
    state.version = "0.5.0".into();
    rust(
        root,
        path,
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[dependencies]\ndep079={path=\"vendor/dep079\"}\n",
        true,
    );
    let mut packages = vec![json!({"name":"main","version":"1.0.0","dependencies":["dep079"]})];
    for index in 0..80 {
        let name = format!("dep{index:03}");
        let dependency = if index == 0 {
            String::new()
        } else {
            format!(
                "[dependencies]\ndep{previous:03}={{path=\"../dep{previous:03}\"}}\n",
                previous = index - 1
            )
        };
        rust(
            root,
            &format!("{path}/vendor/{name}"),
            &format!("[package]\nname=\"{name}\"\nversion=\"1.0.0\"\n{dependency}"),
            false,
        );
        let mut package = json!({"name":name,"version":"1.0.0"});
        if index > 0 {
            package["dependencies"] = json!([format!("dep{:03}", index - 1)]);
        }
        packages.push(package);
    }
    profiles(root, &state);
    selected_cargo_lock(root, path, json!(packages));
    assert!(
        admit_native_components(root, &state)
            .unwrap()
            .effective_dependencies[path]
            .is_empty()
    );
    for case in [
        "missing-record",
        "missing-reference",
        "ambiguous-reference",
        "wrong-local-identity",
    ] {
        let mut broken = packages.clone();
        match case {
            "missing-record" => {
                broken.remove(41);
            }
            "missing-reference" => {
                broken[41]["dependencies"] = json!(["absent"]);
            }
            "ambiguous-reference" => {
                broken[41]["dependencies"] = json!(["remote"]);
                broken.push(json!({"name":"remote","version":"1.0.0","source":"registry+https://example.invalid/index"}));
                broken.push(json!({"name":"remote","version":"2.0.0","source":"registry+https://example.invalid/index"}));
            }
            "wrong-local-identity" => {
                broken[41]["version"] = json!("2.0.0");
            }
            _ => unreachable!(),
        }
        selected_cargo_lock(root, path, json!(broken));
        let before = fs::read(root.join(path).join("Cargo.lock")).unwrap();
        assert!(
            admit_native_components(root, &state).is_err(),
            "{case}: skipped an internal reachable lock record"
        );
        assert_eq!(
            fs::read(root.join(path).join("Cargo.lock")).unwrap(),
            before
        );
    }
}

#[test]
fn internal_cargo_workspace_members_use_the_component_lock_authority() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = state(&[("apps/main", "rust")]);
    state.version = "0.5.0".into();
    rust(
        root,
        "apps/main",
        "[workspace]\nmembers=[\"member\"]\n[workspace.package]\nversion=\"1.0.0\"\n",
        true,
    );
    rust(
        root,
        "apps/main/member",
        "[package]\nname=\"member\"\nversion.workspace=true\n",
        false,
    );
    profiles(root, &state);
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"member","version":"1.0.0"}]),
    );
    assert!(
        admit_native_components(root, &state)
            .unwrap()
            .effective_dependencies["apps/main"]
            .is_empty()
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"wrong","version":"1.0.0"}]),
    );
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("does not uniquely identify workspace package member")
    );
    // A valid package record in a member-local lock does not change the lock
    // authority Cargo actually chooses from an enclosing workspace.
    put(root, "Cargo.toml", "[workspace]\nmembers=[\"apps/main\"]\n");
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n",
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0"}]),
    );
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("component-root workspace/lock authority")
    );
}

#[test]
fn internal_cargo_unused_declarations_remain_inputs_without_selected_edges() {
    for path in ["apps/main", "packages/main"] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let mut state = state(&[(path, "rust")]);
        state.version = "0.5.0".into();
        rust(
            root,
            path,
            "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[workspace.dependencies]\nunused={path=\"vendor/unused\"}\n[patch.crates-io]\nunused={path=\"vendor/unused\"}\n",
            true,
        );
        rust(
            root,
            &format!("{path}/vendor/unused"),
            "[package]\nname=\"unused\"\nversion=\"1.0.0\"\n",
            false,
        );
        profiles(root, &state);
        let admission = admit_native_components(root, &state).unwrap();
        assert!(admission.effective_dependencies[path].is_empty());
        assert!(admission.inputs.files[&format!("{path}/vendor/unused/Cargo.toml")].is_some());
        put(
            root,
            &format!("{path}/vendor/unused/Cargo.toml"),
            "[package]\nname=\"unused\"\nversion=\"2.0.0\"\n",
        );
        assert!(admission.recheck(root).is_err());
        assert!(
            admit_native_components(root, &state)
                .unwrap()
                .effective_dependencies[path]
                .is_empty()
        );
    }
}

fn selected_cargo_fixture(root: &Path, role: Role) -> ProjectState {
    let target = if role == Role::App {
        "apps/helper"
    } else {
        "packages/helper"
    };
    let mut state = state(&[("apps/main", "rust"), (target, "rust")]);
    state.version = "0.5.0".into();
    let relative = if role == Role::App {
        "../helper"
    } else {
        "../../packages/helper"
    };
    rust(
        root,
        "apps/main",
        &format!(
            "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper=\"=1.0.0\"\n[patch.crates-io]\nhelper={{path=\"{relative}\"}}\n"
        ),
        true,
    );
    rust(
        root,
        target,
        "[package]\nname=\"helper\"\nversion=\"1.0.0\"\n[workspace]\n",
        true,
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0"}]),
    );
    selected_cargo_lock(root, target, json!([{"name":"helper","version":"1.0.0"}]));
    profiles(root, &state);
    state
}

#[test]
fn inherited_virtual_workspace_source_guard_matches_direct_declaration() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "rust")]);
    put(
        root,
        "Cargo.toml",
        "[workspace]\nmembers=[\"apps/main\"]\n[workspace.dependencies]\nexternal={git=\"file:///outside\"}\n",
    );
    rust(
        root,
        "apps/main",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\nworkspace=\"../..\"\n[dependencies]\nexternal={workspace=true}\n",
        true,
    );
    profiles(root, &state);
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("Unsupported local Cargo git source")
    );
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[dependencies]\nexternal={git=\"file:///outside\"}\n",
    );
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("Unsupported local Cargo git source")
    );
}

#[test]
fn selected_cargo_patch_role_and_projection_follow_selected_graph() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = selected_cargo_fixture(root, Role::Library);
    state.components.get_mut("apps/main").unwrap().dependencies = vec!["packages/helper".into()];
    let native = admit_native_components(root, &state).unwrap();
    assert_eq!(native.dependencies["apps/main"], vec!["packages/helper"]);
    assert_eq!(
        native.effective_dependencies["apps/main"],
        vec!["packages/helper"]
    );
    state.version = "0.4.1".into();
    state
        .components
        .get_mut("apps/main")
        .unwrap()
        .dependencies
        .clear();
    profiles(root, &state);
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("projection is stale")
    );
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = selected_cargo_fixture(root, Role::App);
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("Application dependency is forbidden")
    );
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper={path=\"../helper\"}\n",
    );
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("Application dependency is forbidden")
    );
}

#[test]
fn unregistered_override_reaching_registered_component_uses_selected_graph() {
    for role in [Role::App, Role::Library] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let state = selected_cargo_fixture(root, role);
        let target = if role == Role::App {
            "apps/helper"
        } else {
            "packages/helper"
        };
        put(
            root,
            "apps/main/Cargo.toml",
            "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nvendor=\"=1.0.0\"\n[patch.crates-io]\nvendor={path=\"../../vendor/provider\"}\n",
        );
        override_crate(root, "vendor/provider", "vendor");
        put(
            root,
            "vendor/provider/Cargo.toml",
            &format!(
                "[package]\nname=\"vendor\"\nversion=\"1.0.0\"\n[dependencies]\nhelper={{path=\"../../{target}\"}}\n"
            ),
        );
        selected_cargo_lock(
            root,
            "apps/main",
            json!([
                {"name":"main","version":"1.0.0","dependencies":["vendor"]},
                {"name":"vendor","version":"1.0.0","dependencies":["helper"]},
                {"name":"helper","version":"1.0.0"}
            ]),
        );
        let result = derive_component_dependencies(root, &state);
        if role == Role::App {
            assert!(
                result.is_err(),
                "transitive selected override app edge was skipped"
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Application dependency is forbidden")
            );
        } else {
            assert_eq!(result.unwrap()["apps/main"], vec![target]);
        }
    }
}

#[test]
fn graph_authority_ordinary_direct_and_transitive_paths_follow_selected_edges() {
    let mut wrong = Vec::new();
    for role in [Role::App, Role::Library] {
        for transitive in [false, true] {
            let f = TempDir::new().unwrap();
            let root = f.path();
            let state = selected_cargo_fixture(root, role);
            let target = if role == Role::App {
                "apps/helper"
            } else {
                "packages/helper"
            };
            if transitive {
                put(
                    root,
                    "apps/main/Cargo.toml",
                    "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nvendor={path=\"../../vendor/provider\"}\n",
                );
                override_crate(root, "vendor/provider", "vendor");
                put(
                    root,
                    "vendor/provider/Cargo.toml",
                    &format!(
                        "[package]\nname=\"vendor\"\nversion=\"1.0.0\"\n[dependencies]\nhelper={{path=\"../../{target}\"}}\n"
                    ),
                );
                selected_cargo_lock(
                    root,
                    "apps/main",
                    json!([{"name":"main","version":"1.0.0","dependencies":["vendor"]},{"name":"vendor","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0"}]),
                );
            } else {
                put(
                    root,
                    "apps/main/Cargo.toml",
                    &format!(
                        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper={{path=\"../../{target}\"}}\n"
                    ),
                );
            }
            let result = derive_component_dependencies(root, &state);
            if role == Role::App {
                if !result.as_ref().is_err_and(|error| {
                    error
                        .to_string()
                        .contains("Application dependency is forbidden")
                }) {
                    wrong.push(format!("app transitive={transitive}: {result:?}"));
                }
            } else if !result
                .as_ref()
                .is_ok_and(|map| map["apps/main"] == vec![target])
            {
                wrong.push(format!("library transitive={transitive}: {result:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("; "));
}

#[test]
fn graph_authority_unused_workspace_path_is_input_not_modern_edge() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = selected_cargo_fixture(root, Role::Library);
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[workspace.dependencies]\nhelper={path=\"../../packages/helper\"}\n",
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0"}]),
    );
    assert!(
        collect_native_input_closure(root, &state).unwrap().files["packages/helper/Cargo.toml"]
            .is_some()
    );
    assert!(
        derive_component_dependencies(root, &state).unwrap()["apps/main"].is_empty(),
        "unused workspace definition activated an effective edge"
    );
    let admitted = admit_native_components(root, &state).unwrap();
    assert!(admitted.effective_dependencies["apps/main"].is_empty());
    state.version = "0.4.1".into();
    state.components.get_mut("apps/main").unwrap().dependencies = vec!["packages/helper".into()];
    profiles(root, &state);
    assert!(
        admit_native_components(root, &state)
            .err()
            .unwrap()
            .to_string()
            .contains("projection is stale")
    );
}

#[test]
fn graph_authority_component_workspace_inheritance_uses_full_lock_records() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = selected_cargo_fixture(root, Role::Library);
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[workspace.dependencies]\nhelper={path=\"../../packages/helper\"}\n[dependencies]\nhelper={workspace=true}\n",
    );
    for version in ["0.4.1", "0.5.0"] {
        state.version = version.into();
        state.components.get_mut("apps/main").unwrap().dependencies =
            vec!["packages/helper".into()];
        profiles(root, &state);
        let admitted = admit_native_components(root, &state).unwrap();
        assert_eq!(admitted.dependencies["apps/main"], vec!["packages/helper"]);
        assert_eq!(
            admitted.effective_dependencies["apps/main"],
            vec!["packages/helper"]
        );
    }
}

#[test]
fn cargo_lock_reachable_edges_control_activity_source_and_ambiguity() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = selected_cargo_fixture(root, Role::App);
    // A local package record alone is not an active dependency edge.
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0"},{"name":"helper","version":"1.0.0"}]),
    );
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/main"].is_empty());
    // An equal name/version in a registry remains registry-selected.
    selected_cargo_lock(
        root,
        "apps/main",
        json!([
            {"name":"main","version":"1.0.0","dependencies":["helper 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)"]},
            {"name":"helper","version":"1.0.0"},
            {"name":"helper","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index"}
        ]),
    );
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/main"].is_empty());
    selected_cargo_lock(
        root,
        "apps/main",
        json!([
            {"name":"main","version":"1.0.0","dependencies":["helper 2.0.0"]},
            {"name":"helper","version":"2.0.0","source":"registry+https://github.com/rust-lang/crates.io-index"}
        ]),
    );
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/main"].is_empty());
    // Unused patch to the source root must not produce an app->self edge.
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[patch.crates-io]\nunrelated={path=\".\"}\n",
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0"}]),
    );
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/main"].is_empty());
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper=\"1.0.0\"\n[patch.crates-io]\nhelper={path=\"../helper\"}\n[patch.'https://example.invalid/other']\nhelper={path=\"../../vendor/helper\"}\n",
    );
    override_crate(root, "vendor/helper", "helper");
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0"}]),
    );
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("Ambiguous local Cargo target")
    );
}

#[test]
fn compact_local_cargo_reference_uses_lock_source_without_manifest_guessing() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = selected_cargo_fixture(root, Role::Library);
    selected_cargo_lock(
        root,
        "apps/main",
        json!([
            {"name":"main","version":"1.0.0","dependencies":["helper 1.0.0","helper 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)"]},
            {"name":"helper","version":"1.0.0"},
            {"name":"helper","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index"}
        ]),
    );
    assert_eq!(
        derive_component_dependencies(root, &state).unwrap()["apps/main"],
        vec!["packages/helper"]
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([
            {"name":"main","version":"1.0.0","dependencies":["helper"]},
            {"name":"helper","version":"1.0.0"},
            {"name":"helper","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index"}
        ]),
    );
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("missing or ambiguous")
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([
            {"name":"main","version":"1.0.0","dependencies":["helper 1.0.0"]},
            {"name":"helper","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index"},
            {"name":"helper","version":"1.0.0","source":"git+https://example.invalid/repository#0123456789abcdef"}
        ]),
    );
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("missing or ambiguous")
    );
}

#[test]
fn workspace_member_seeds_find_edges_without_creating_internal_app_edges() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = selected_cargo_fixture(root, Role::Library);
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\nmembers=[\"member\"]\n[workspace.package]\nversion=\"2.0.0\"\n[patch.crates-io]\nhelper={path=\"../../packages/helper\"}\n",
    );
    override_crate(root, "apps/main/member", "member");
    put(
        root,
        "apps/main/member/Cargo.toml",
        "[package]\nname=\"member\"\nversion.workspace=true\n[dependencies]\nhelper=\"1.0.0\"\nmain={path=\"..\"}\n",
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([
            {"name":"main","version":"1.0.0"},
            {"name":"member","version":"2.0.0","dependencies":["main","helper"]},
            {"name":"helper","version":"1.0.0"}
        ]),
    );
    state.components.get_mut("apps/main").unwrap().dependencies = vec!["packages/helper".into()];
    let native = admit_native_components(root, &state).unwrap();
    assert_eq!(
        native.effective_dependencies["apps/main"],
        vec!["packages/helper"]
    );
}

#[test]
fn cargo_paths_registered_selection_is_actionably_uncertain_and_cycles_are_checked() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let first_state = selected_cargo_fixture(root, Role::Library);
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper=\"1.0.0\"\n",
    );
    put(
        root,
        "apps/main/.cargo/config.toml",
        "paths=[\"../../packages/helper\"]\n",
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{ "name":"main","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index"}]),
    );
    assert!(
        derive_component_dependencies(root, &first_state)
            .unwrap_err()
            .to_string()
            .contains("Unsupported Cargo config paths override")
    );
    fs::remove_file(root.join("apps/main/.cargo/config.toml")).unwrap();
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0"}]),
    );
    assert!(derive_component_dependencies(root, &first_state).unwrap()["apps/main"].is_empty());
    put(
        root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper={path=\"../../packages/helper\"}\n",
    );
    selected_cargo_lock(
        root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0"}]),
    );
    let lock_path = root.join("apps/main/Cargo.lock");
    let lock = fs::read_to_string(&lock_path)
        .unwrap()
        .replace("version = 4", "version = 3");
    put(root, "apps/main/Cargo.lock", &lock);
    assert_eq!(
        derive_component_dependencies(root, &first_state).unwrap()["apps/main"],
        vec!["packages/helper"]
    );
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut state = state(&[
        ("apps/main", "rust"),
        ("packages/a", "rust"),
        ("packages/b", "rust"),
    ]);
    state.version = "0.5.0".into();
    rust(
        root,
        "apps/main",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n",
        true,
    );
    for (name, other) in [("a", "b"), ("b", "a")] {
        rust(
            root,
            &format!("packages/{name}"),
            &format!(
                "[package]\nname=\"{name}\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\n{other}=\"1.0.0\"\n[patch.crates-io]\n{other}={{path=\"../{other}\"}}\n"
            ),
            true,
        );
        selected_cargo_lock(
            root,
            &format!("packages/{name}"),
            json!([{"name":name,"version":"1.0.0","dependencies":[other]},{"name":other,"version":"1.0.0"}]),
        );
    }
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("cycle")
    );
}

#[test]
fn full_runtime_rejects_patch_app_edge_and_synchronizes_modern_library_edge() {
    let f = TempDir::new().unwrap();
    let release = runtime_release(f.path());
    let root = f.path().join("consumer");
    let before = init_project(
        &release,
        &root,
        &InitOptions {
            apps: Some(vec![
                ComponentSpec {
                    name: "main".into(),
                    template: "rust-api".into(),
                    uses: vec![],
                },
                ComponentSpec {
                    name: "helper".into(),
                    template: "rust-api".into(),
                    uses: vec![],
                },
            ]),
            name: Some("native-fixture".into()),
            ..Default::default()
        },
        &SeedFixtureResolver,
    )
    .unwrap();
    put(
        &root,
        "apps/main/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper=\"=1.0.0\"\n[patch.crates-io]\nhelper={path=\"../helper\"}\n",
    );
    put(
        &root,
        "apps/helper/Cargo.toml",
        "[package]\nname=\"helper\"\nversion=\"1.0.0\"\n[workspace]\n",
    );
    put(&root, "apps/helper/src/lib.rs", "pub fn helper() {}\n");
    selected_cargo_lock(
        &root,
        "apps/main",
        json!([{"name":"main","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0"}]),
    );
    selected_cargo_lock(
        &root,
        "apps/helper",
        json!([{"name":"helper","version":"1.0.0"}]),
    );
    assert!(
        admit_runtime_project(&root, "0.5.0")
            .err()
            .unwrap()
            .to_string()
            .contains("Application dependency is forbidden")
    );
    assert!(
        update_project(&release, &root, &UpdateOptions::default(), &RefuseResolver)
            .err()
            .unwrap()
            .to_string()
            .contains("Application dependency is forbidden")
    );
    assert_eq!(
        devkit::state::read_project_state(&root).unwrap().generation,
        before.generation
    );
    let f = TempDir::new().unwrap();
    let (release, root, _) = runtime_project(f.path(), "rust");
    rust(
        &root,
        "packages/helper",
        "[package]\nname=\"helper\"\nversion=\"1.0.0\"\n[workspace]\n",
        true,
    );
    put(&root, "packages/helper/src/lib.rs", "pub fn helper() {}\n");
    selected_cargo_lock(
        &root,
        "packages/helper",
        json!([{"name":"helper","version":"1.0.0"}]),
    );
    register_project_component(
        &release,
        &root,
        Role::Library,
        "helper".into(),
        ToolingProfile::Rust,
        &UpdateOptions::default(),
        &RefuseResolver,
    )
    .unwrap();
    put(
        &root,
        "apps/api/Cargo.toml",
        "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nhelper=\"=1.0.0\"\n[patch.crates-io]\nhelper={path=\"../../packages/helper\"}\n",
    );
    selected_cargo_lock(
        &root,
        "apps/api",
        json!([{"name":"main","version":"1.0.0","dependencies":["helper"]},{"name":"helper","version":"1.0.0"}]),
    );
    let before = devkit::state::read_project_state(&root).unwrap();
    assert!(
        admit_runtime_project(&root, "0.5.0")
            .err()
            .unwrap()
            .to_string()
            .contains("projection is stale")
    );
    let next = update_project(&release, &root, &UpdateOptions::default(), &RefuseResolver).unwrap();
    assert!(!next.no_op);
    assert_ne!(next.state.generation, before.generation);
    assert_eq!(
        next.state.components["apps/api"].dependencies,
        vec!["packages/helper"]
    );
    assert_eq!(
        admit_runtime_project(&root, "0.5.0")
            .unwrap()
            .native
            .effective_dependencies["apps/api"],
        vec!["packages/helper"]
    );
}

#[test]
#[ignore = "requires Cargo inside the pinned Rust 1.99.1 validation container"]
fn pinned_cargo_metadata_confirms_ordinary_graph_and_unused_patch_neutrality() {
    fn metadata(root: &Path, component: &str) -> Value {
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
            .current_dir(root.join(component))
            .env("CARGO_HOME", std::env::var_os("CARGO_HOME").unwrap())
            .env("CARGO_TARGET_DIR", root.join(".metadata-target"))
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
    for (name, role, transitive, unused) in [
        ("direct-app", Role::App, false, false),
        ("direct-library", Role::Library, false, false),
        ("transitive-app", Role::App, true, false),
        ("transitive-library", Role::Library, true, false),
        ("unused-workspace-library", Role::Library, false, true),
    ] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let mut state = selected_cargo_fixture(root, role);
        let target = if role == Role::App {
            "apps/helper"
        } else {
            "packages/helper"
        };
        let mut manifest = "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n".to_owned();
        if transitive {
            manifest.push_str("[dependencies]\nvendor={path=\"../../vendor/provider\"}\n");
            override_crate(root, "vendor/provider", "vendor");
            put(
                root,
                "vendor/provider/Cargo.toml",
                &format!(
                    "[package]\nname=\"vendor\"\nversion=\"1.0.0\"\n[dependencies]\nhelper={{path=\"../../{target}\"}}\n"
                ),
            );
        } else if unused {
            manifest
                .push_str("[workspace.dependencies]\nhelper={path=\"../../packages/helper\"}\n");
        } else {
            manifest.push_str(&format!(
                "[dependencies]\nhelper={{path=\"../../{target}\"}}\n"
            ));
        }
        put(root, "apps/main/Cargo.toml", &manifest);
        put(root, "apps/main/src/main.rs", "fn main() {}\n");
        put(root, "apps/main/src/lib.rs", "pub fn root() {}\n");
        put(
            root,
            &format!("{target}/src/lib.rs"),
            "pub fn helper() {}\n",
        );
        if role == Role::App {
            put(root, &format!("{target}/src/main.rs"), "fn main() {}\n");
        }
        let before = metadata(root, "apps/main");
        metadata(root, target);
        let before_lock = fs::read(root.join("apps/main/Cargo.lock")).unwrap();
        let expected = if unused {
            vec![]
        } else {
            vec![target.to_owned()]
        };
        for with_unused_patch in [false, true] {
            if with_unused_patch {
                put(
                    root,
                    "apps/main/Cargo.toml",
                    &format!("{manifest}\n[patch.crates-io]\nmain={{path=\".\"}}\n"),
                );
            }
            let current = metadata(root, "apps/main");
            assert_eq!(
                before["resolve"]["nodes"], current["resolve"]["nodes"],
                "unused self patch changed resolve for {name}"
            );
            if role == Role::App {
                assert!(
                    derive_component_dependencies(root, &state)
                        .unwrap_err()
                        .to_string()
                        .contains("Application dependency is forbidden")
                );
            } else {
                assert_eq!(
                    derive_component_dependencies(root, &state).unwrap()["apps/main"],
                    expected,
                    "{name}"
                );
                state.components.get_mut("apps/main").unwrap().dependencies = expected.clone();
                assert_eq!(
                    admit_native_components(root, &state)
                        .unwrap()
                        .effective_dependencies["apps/main"],
                    expected
                );
            }
        }
        assert!(
            collect_native_input_closure(root, &state).unwrap().files
                [&format!("{target}/Cargo.toml")]
                .is_some()
        );
        proof.push(json!({"case":name,"beforeResolveNodes":before["resolve"]["nodes"],"unusedPatchResolveEqual":true,
            "beforeLockSha256":devkit::files::digest(before_lock),"afterLockSha256":devkit::files::digest(fs::read(root.join("apps/main/Cargo.lock")).unwrap()),
            "effectiveExpected":if role==Role::App {json!("app-target-refused")} else {json!(expected)}}));
    }
    if let Ok(directory) = std::env::var("DEVKIT_PROOF_DIR") {
        write_json(&Path::new(&directory).join("cargo-path-authority-proof.json"),&json!({"toolchain":"1.99.1","offline":true,"cases":proof,
            "authority":"Only selected Cargo.lock dependency/replacement edges activate Rust component relationships; raw declarations remain inputs."})).unwrap();
    }
}

#[test]
#[ignore = "requires Cargo inside the pinned Rust 1.99.1 validation container"]
fn pinned_cargo_metadata_proves_selected_component_edges_and_paths_uncertainty() {
    fn metadata(root: &Path, component: &str, locked: bool) -> Value {
        let version = std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&version.stdout).starts_with("cargo 1.99.1 "));
        let mut command = std::process::Command::new("cargo");
        command.args([
            "metadata",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
            "Cargo.toml",
        ]);
        if locked {
            command.arg("--locked");
        }
        let output = command
            .current_dir(root.join(component))
            .env("CARGO_HOME", std::env::var_os("CARGO_HOME").unwrap())
            .env("CARGO_TARGET_DIR", root.join(".metadata-target"))
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{component}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
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
    let mut proof = Vec::new();
    for form in [
        "app-patch",
        "transitive-vendor-app",
        "library-patch",
        "member-only-patch",
        "unused-root-patch",
        "registry-source-control",
        "registered-replace",
        "registered-paths",
        "parallel-local-registry",
    ] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let role = if ["app-patch", "transitive-vendor-app"].contains(&form) {
            Role::App
        } else {
            Role::Library
        };
        let mut state = selected_cargo_fixture(root, role);
        let target = if role == Role::App {
            "apps/helper"
        } else {
            "packages/helper"
        };
        put(root, "apps/main/src/main.rs", "fn main() {}\n");
        put(
            root,
            &format!("{target}/src/lib.rs"),
            "pub fn helper() {}\n",
        );
        if role == Role::App {
            put(root, &format!("{target}/src/main.rs"), "fn main() {}\n");
        }
        if form == "transitive-vendor-app" {
            put(
                root,
                "apps/main/Cargo.toml",
                "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nvendor=\"=1.0.0\"\n[patch.crates-io]\nvendor={path=\"../../vendor/provider\"}\n",
            );
            override_crate(root, "vendor/provider", "vendor");
            put(
                root,
                "vendor/provider/Cargo.toml",
                "[package]\nname=\"vendor\"\nversion=\"1.0.0\"\n[dependencies]\nhelper={path=\"../../apps/helper\"}\n",
            );
        } else if form == "member-only-patch" {
            put(
                root,
                "apps/main/Cargo.toml",
                "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\nmembers=[\"member\"]\n[patch.crates-io]\nhelper={path=\"../../packages/helper\"}\n",
            );
            override_crate(root, "apps/main/member", "member");
            put(
                root,
                "apps/main/member/Cargo.toml",
                "[package]\nname=\"member\"\nversion=\"1.0.0\"\n[dependencies]\nhelper=\"=1.0.0\"\nmain={path=\"..\"}\n",
            );
            put(root, "apps/main/src/lib.rs", "pub fn root() {}\n");
        } else if form == "unused-root-patch" {
            put(
                root,
                "apps/main/Cargo.toml",
                "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[patch.crates-io]\nmain={path=\".\"}\n",
            );
            put(root, "apps/main/src/lib.rs", "pub fn root() {}\n");
        } else if [
            "registry-source-control",
            "registered-replace",
            "registered-paths",
            "parallel-local-registry",
        ]
        .contains(&form)
        {
            let header = if form == "parallel-local-registry" {
                format!(
                    "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nlocal-anyhow={{package=\"anyhow\",path=\"../../packages/helper\"}}\nregistry-anyhow={{package=\"anyhow\",version=\"={anyhow_version}\"}}\n"
                )
            } else {
                format!(
                    "[package]\nname=\"main\"\nversion=\"1.0.0\"\n[workspace]\n[dependencies]\nanyhow=\"={anyhow_version}\"\n"
                )
            };
            let manifest = if form == "registry-source-control" {
                format!(
                    "{header}[patch.'https://example.invalid/other']\nanyhow={{path=\"../../packages/helper\"}}\n"
                )
            } else if form == "registered-replace" {
                format!(
                    "{header}[replace]\n\"anyhow:{anyhow_version}\"={{path=\"../../packages/helper\"}}\n"
                )
            } else if form == "parallel-local-registry" {
                put(root, "apps/main/src/lib.rs", "pub fn root() {}\n");
                format!("{header}[patch.crates-io]\nmain={{path=\".\"}}\n")
            } else {
                header
            };
            put(root, "apps/main/Cargo.toml", &manifest);
            put(
                root,
                "packages/helper/Cargo.toml",
                &format!(
                    "[package]\nname=\"anyhow\"\nversion=\"{anyhow_version}\"\n[workspace]\n[features]\ndefault=[\"std\"]\nstd=[]\nbacktrace=[]\n"
                ),
            );
            if form == "registered-paths" {
                put(
                    root,
                    "apps/main/.cargo/config.toml",
                    "paths=[\"../../packages/helper\"]\n",
                );
            }
        }
        let selected = metadata(root, "apps/main", false);
        metadata(root, target, false);
        if let Ok(directory) = std::env::var("DEVKIT_PROOF_DIR") {
            put(
                Path::new(&directory),
                &format!("selected-lock-{form}.toml"),
                &fs::read_to_string(root.join("apps/main/Cargo.lock")).unwrap(),
            );
        }
        let selected_edges = selected["resolve"]["nodes"].clone();
        if ["app-patch", "transitive-vendor-app"].contains(&form) {
            assert!(
                derive_component_dependencies(root, &state)
                    .unwrap_err()
                    .to_string()
                    .contains("Application dependency is forbidden")
            );
        } else if form == "registered-paths" {
            let lock: toml::Value =
                toml::from_str(&fs::read_to_string(root.join("apps/main/Cargo.lock")).unwrap())
                    .unwrap();
            assert!(lock["package"].as_array().unwrap().iter().any(|package| {
                package["name"].as_str() == Some("anyhow")
                    && package
                        .get("source")
                        .and_then(toml::Value::as_str)
                        .is_some()
            }));
            assert!(
                derive_component_dependencies(root, &state)
                    .unwrap_err()
                    .to_string()
                    .contains("Unsupported Cargo config paths override")
            );
        } else {
            let expected = if ["unused-root-patch", "registry-source-control"].contains(&form) {
                vec![]
            } else {
                vec!["packages/helper".to_owned()]
            };
            assert_eq!(
                derive_component_dependencies(root, &state).unwrap()["apps/main"],
                expected,
                "{form}"
            );
            state.components.get_mut("apps/main").unwrap().dependencies = expected.clone();
            assert_eq!(
                admit_native_components(root, &state)
                    .unwrap()
                    .effective_dependencies["apps/main"],
                expected
            );
            if form == "library-patch" {
                let lock_path = root.join("apps/main/Cargo.lock");
                let old = fs::read_to_string(&lock_path).unwrap();
                put(
                    root,
                    "apps/main/Cargo.lock",
                    &old.replace("version = 4", "version = 3"),
                );
                metadata(root, "apps/main", true);
                assert_eq!(
                    derive_component_dependencies(root, &state).unwrap()["apps/main"],
                    vec!["packages/helper"]
                );
            }
        }
        proof.push(json!({"form":form,"nativeEdges":selected_edges,"lockSha256":devkit::files::digest(fs::read(root.join("apps/main/Cargo.lock")).unwrap())}));
    }
    if let Ok(directory) = std::env::var("DEVKIT_PROOF_DIR") {
        write_json(&Path::new(&directory).join("cargo-selected-graph-proof.json"),&json!({
            "toolchain":"1.99.1","offline":true,"cases":proof,
            "sources":["https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#paths-overrides","https://doc.rust-lang.org/cargo/reference/manifest.html#the-version-field"],
            "lockAuthority":"registered component Cargo.lock; exact IDs and dependency/replacement refs; all component-workspace package seeds"
        })).unwrap();
    }
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

#[test]
fn npm_alias_admission_validates_target_name_and_rejects_spoofed_or_nested_specs() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node"), ("packages/shared", "node")]);
    let mut alias_artifact = artifact("string-width", json!({"name": "string-width"}));
    alias_artifact["version"] = json!("4.2.3");
    alias_artifact["resolved"] =
        json!("https://registry.npmjs.org/string-width/-/string-width-4.2.3.tgz");
    let mut scoped_artifact = json!({
        "name": "@acme/core",
        "version": "1.2.0",
        "resolved": "https://registry.npmjs.org/@acme/core/-/core-1.2.0.tgz",
        "integrity": sri(2),
    });
    node_project(
        root,
        &state,
        &[
            (
                "apps/main",
                manifest(
                    "@test/main",
                    json!({
                        "string-width-cjs": "npm:string-width@^4.2.0",
                        "scoped-alias": "npm:@acme/core@^1.2.0"
                    }),
                    false,
                ),
            ),
            ("packages/shared", manifest("@test/shared", json!({}), true)),
        ],
        json!({
            "node_modules/string-width-cjs": alias_artifact,
            "node_modules/scoped-alias": scoped_artifact,
        }),
    );
    assert_native_components(root, &state).unwrap();
    let before = capture_product_graph(root, &state).unwrap();
    assert_product_graphs_preserved(
        &before,
        &read_native_graph(root, NodeLockKind::Npm).unwrap(),
    )
    .unwrap();

    // Changing the target package under the same alias key & version must fail BOTH
    // native admission and product graph preservation.
    let original_lock: Value = read_json(&root.join("package-lock.json")).unwrap();
    let mut spoofed_lock = original_lock.clone();
    spoofed_lock["packages"]["node_modules/string-width-cjs"]["name"] = json!("evil-pkg");
    spoofed_lock["packages"]["node_modules/string-width-cjs"]["resolved"] =
        json!("https://registry.npmjs.org/evil-pkg/-/evil-pkg-4.2.3.tgz");
    put_json(root, "package-lock.json", &spoofed_lock);
    assert!(
        assert_native_components(root, &state)
            .unwrap_err()
            .to_string()
            .contains("must resolve to registry package string-width")
    );
    put_json(root, "package-lock.json", &original_lock);

    // Changing both manifest alias target and lockfile node name changes product identity.
    let mut renamed_manifest = manifest(
        "@test/main",
        json!({
            "string-width-cjs": "npm:other-width@^4.2.0",
            "scoped-alias": "npm:@acme/core@^1.2.0"
        }),
        false,
    );
    put_json(root, "apps/main/package.json", &renamed_manifest);
    let mut renamed_lock = original_lock.clone();
    renamed_lock["packages"]["apps/main"] = renamed_manifest.clone();
    renamed_lock["packages"]["node_modules/string-width-cjs"]["name"] = json!("other-width");
    renamed_lock["packages"]["node_modules/string-width-cjs"]["resolved"] =
        json!("https://registry.npmjs.org/other-width/-/other-width-4.2.3.tgz");
    put_json(root, "package-lock.json", &renamed_lock);
    assert!(
        assert_product_graphs_preserved(
            &before,
            &read_native_graph(root, NodeLockKind::Npm).unwrap()
        )
        .is_err()
    );

    // Alias cannot target a registered local workspace component.
    renamed_manifest["dependencies"] = json!({"@test/shared": "npm:@test/shared@1.0.0"});
    put_json(root, "apps/main/package.json", &renamed_manifest);
    renamed_lock["packages"]["apps/main"] = renamed_manifest.clone();
    put_json(root, "package-lock.json", &renamed_lock);
    assert!(assert_native_components(root, &state).is_err());

    // Invalid inner specs under npm: must fail closed.
    for bad_spec in [
        "npm:a@npm:b@1.0.0",
        "npm:file:../shared",
        "npm:pkg@file:../shared",
        "npm:pkg@https://example.com/pkg.tgz",
        "npm:pkg@workspace:*",
        "npm:",
    ] {
        let m = manifest("@test/main", json!({"bad": bad_spec}), false);
        put_json(root, "apps/main/package.json", &m);
        let mut l = original_lock.clone();
        l["packages"]["apps/main"] = m;
        l["packages"]["node_modules/bad"] = scoped_artifact.clone();
        put_json(root, "package-lock.json", &l);
        assert!(
            assert_native_components(root, &state).is_err(),
            "expected {bad_spec} to be rejected"
        );
    }
    let _ = &mut scoped_artifact;
}

#[test]
fn npmrc_closed_allowlist_supports_gitlab_registries_and_rejects_secret_leaks() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = state(&[("apps/main", "node")]);
    node_project(
        root,
        &state,
        &[(
            "apps/main",
            manifest("@test/main", json!({"@corp/sdk": "^1.0.0"}), false),
        )],
        json!({
            "node_modules/@corp/sdk": {
                "version": "1.0.0",
                "resolved": "https://gitlab.example.com/api/v4/projects/12/packages/npm/@corp/sdk/-/@corp/sdk-1.0.0.tgz",
                "integrity": sri(3),
            }
        }),
    );
    put(
        root,
        ".npmrc",
        "# Private GitLab npm registry\n\
         package-lock=true\n\
         strict-peer-deps=true\n\
         ignore-scripts=true\n\
         @corp:registry=https://gitlab.example.com/api/v4/projects/12/packages/npm/\n\
         //gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${GITLAB_NPM_TOKEN}\n\
         //gitlab.example.com/api/v4/projects/12/packages/npm/:always-auth=true\n",
    );
    let closure = collect_native_input_closure(root, &state).unwrap();
    let policy = parse_and_admit_npmrc(
        root,
        &closure.directories,
        NodeLockKind::for_state(&state).unwrap(),
    )
    .unwrap();
    assert_eq!(
        policy.scoped_registries.get("corp").map(String::as_str),
        Some("https://gitlab.example.com/api/v4/projects/12/packages/npm/")
    );
    assert!(policy.required_env_vars.contains("GITLAB_NPM_TOKEN"));
    assert_native_components(root, &state).unwrap();

    // IPv6 HTTPS registry and nerf-dart token expansion symmetry.
    put(
        root,
        ".npmrc",
        "@corp:registry=https://[fd00::1]:8443/api/v4/projects/12/packages/npm/\n\
         //[fd00::1]:8443/api/v4/projects/12/packages/npm/:_authToken=${GITLAB_NPM_TOKEN}\n",
    );
    let ipv6_policy = parse_and_admit_npmrc(
        root,
        &closure.directories,
        NodeLockKind::for_state(&state).unwrap(),
    )
    .unwrap();
    assert_eq!(
        ipv6_policy
            .scoped_registries
            .get("corp")
            .map(String::as_str),
        Some("https://[fd00::1]:8443/api/v4/projects/12/packages/npm/")
    );

    for bad_npmrc in [
        "//gitlab.example.com:0/api/v4/projects/12/packages/npm/:_authToken=${GITLAB_NPM_TOKEN}\n",
        "//gitlab.example.com:99999/api/v4/projects/12/packages/npm/:_authToken=${GITLAB_NPM_TOKEN}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=glpat-secret-literal\n",
        "_authToken=${NPM_TOKEN}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${LD_PRELOAD}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${NODE_OPTIONS}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${DEVKIT_CA_CERT}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${RAYON_NUM_THREADS}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${ROLLDOWN_WORKER_THREADS}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${VITEST_MAX_WORKERS}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${TOKIO_WORKER_THREADS}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${GOMAXPROCS}\n",
        "//gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${UV_THREADPOOL_SIZE}\n",
        "strict-ssl=false\n",
        "cafile=/etc/ssl/cert.pem\n",
        "@corp:registry=http://gitlab.example.com/api/v4/packages/npm/\n",
        "@corp:registry=https://user:token@gitlab.example.com/api/v4/packages/npm/\n",
    ] {
        put(root, ".npmrc", bad_npmrc);
        assert!(
            parse_and_admit_npmrc(
                root,
                &closure.directories,
                NodeLockKind::for_state(&state).unwrap()
            )
            .is_err(),
            "expected .npmrc to be rejected: {bad_npmrc}"
        );
    }

    // Restore valid .npmrc and verify credentials/queries in package-lock.json resolved URLs fail closed.
    put(
        root,
        ".npmrc",
        "@corp:registry=https://gitlab.example.com/api/v4/projects/12/packages/npm/\n\
         //gitlab.example.com/api/v4/projects/12/packages/npm/:_authToken=${GITLAB_NPM_TOKEN}\n",
    );
    for bad_resolved in [
        "https://user:token@gitlab.example.com/api/v4/projects/12/packages/npm/@corp/sdk/-/sdk-1.0.0.tgz",
        "https://gitlab.example.com/api/v4/projects/12/packages/npm/@corp/sdk/-/sdk-1.0.0.tgz?private_token=secret",
        "http://gitlab.example.com/api/v4/projects/12/packages/npm/@corp/sdk/-/sdk-1.0.0.tgz",
    ] {
        let mut lock: Value = read_json(&root.join("package-lock.json")).unwrap();
        lock["packages"]["node_modules/@corp/sdk"]["resolved"] = json!(bad_resolved);
        put_json(root, "package-lock.json", &lock);
        assert!(
            assert_native_components(root, &state).is_err(),
            "expected resolved URL to be rejected: {bad_resolved}"
        );
    }
}

#[test]
fn publishable_library_contract_enforces_private_exports_files_semver_and_public_deps() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let mut st = state(&[
        ("apps/web", "node"),
        ("packages/internal", "node"),
        ("packages/core", "node"),
        ("packages/ui", "node"),
    ]);
    st.version = "0.4.0".into();
    st.components
        .get_mut("packages/core")
        .unwrap()
        .tooling_profile = ToolingProfile::TsLibrary;
    st.components
        .get_mut("packages/ui")
        .unwrap()
        .tooling_profile = ToolingProfile::ReactUi;
    st.components.get_mut("packages/ui").unwrap().dependencies = vec!["packages/core".into()];
    st.components.get_mut("apps/web").unwrap().dependencies = vec!["packages/ui".into()];

    let valid_lib = |name: &str, deps: Value| -> Value {
        json!({
            "name": name,
            "version": "1.2.0",
            "private": false,
            "type": "module",
            "files": ["dist"],
            "exports": {
                ".": {
                    "types": "./dist/index.d.mts",
                    "default": "./dist/index.mjs"
                },
                "./package.json": "./package.json"
            },
            "dependencies": deps,
            "scripts": {
                "check": "devkit-verify",
                "fix": "prettier --write .",
                "test": "vitest run",
                "build": "tsdown && devkit-verify --pack",
                "package": "devkit-pack"
            }
        })
    };

    let base_web = manifest("@corp/app-web", json!({"@corp/ui": "^1.2.0"}), false);
    let base_internal = manifest("@corp/internal", json!({}), true);
    let base_core = valid_lib("@corp/core", json!({}));
    let base_ui = valid_lib("@corp/ui", json!({"@corp/core": "^1.2.0"}));

    let write_all = |web: &Value, internal: &Value, core: &Value, ui: &Value| {
        node_project(
            root,
            &st,
            &[
                ("apps/web", web.clone()),
                ("packages/internal", internal.clone()),
                ("packages/core", core.clone()),
                ("packages/ui", ui.clone()),
            ],
            json!({}),
        );
    };

    write_all(&base_web, &base_internal, &base_core, &base_ui);
    let admitted = admit_native_components(root, &st).unwrap();
    assert_eq!(admitted.dependencies["packages/ui"], vec!["packages/core"]);
    assert_eq!(admitted.dependencies["apps/web"], vec!["packages/ui"]);

    // 1. Role::App cannot set "private": false (omitting "private" is valid, false is rejected).
    let mut bad_web = base_web.clone();
    bad_web["private"] = json!(false);
    write_all(&bad_web, &base_internal, &base_core, &base_ui);
    assert!(
        assert_native_components(root, &st)
            .unwrap_err()
            .to_string()
            .contains("cannot set \"private\": false")
    );

    // 2. Role::Library requires explicit boolean "private", and ReactLibrary requires "private": true.
    let mut missing_priv = base_core.clone();
    missing_priv.as_object_mut().unwrap().remove("private");
    write_all(&base_web, &base_internal, &missing_priv, &base_ui);
    assert!(assert_native_components(root, &st).is_err());

    let mut bad_react_lib = base_internal.clone();
    bad_react_lib["private"] = json!(false);
    write_all(&base_web, &bad_react_lib, &base_core, &base_ui);
    assert!(
        assert_native_components(root, &st)
            .unwrap_err()
            .to_string()
            .contains("Internal react-library profile requires \"private\": true")
    );

    // 3. Publishable library validations on "private": false.
    let mut unscoped_restricted = base_core.clone();
    unscoped_restricted["name"] = json!("plain-core");
    unscoped_restricted["publishConfig"] = json!({"access": "restricted"});
    let mut ui_for_plain = base_ui.clone();
    ui_for_plain["dependencies"] = json!({"plain-core": "^1.2.0"});
    write_all(
        &base_web,
        &base_internal,
        &unscoped_restricted,
        &ui_for_plain,
    );
    assert!(
        assert_native_components(root, &st)
            .unwrap_err()
            .to_string()
            .contains("Unscoped publishable library")
    );

    for (label, mutate) in [
        (
            "bad-semver",
            (&|m: &mut Value| m["version"] = json!("01.2.0")) as &dyn Fn(&mut Value),
        ),
        ("empty-files", &|m: &mut Value| m["files"] = json!([])),
        ("files-missing-dist", &|m: &mut Value| {
            m["files"] = json!(["README.md"])
        }),
        ("files-leaks-src", &|m: &mut Value| {
            m["files"] = json!(["dist", "src"])
        }),
        ("files-leaks-tests", &|m: &mut Value| {
            m["files"] = json!(["dist", "tests"])
        }),
        ("files-leaks-parent", &|m: &mut Value| {
            m["files"] = json!(["dist", "../secret"])
        }),
        ("files-leaks-dotenv", &|m: &mut Value| {
            m["files"] = json!(["dist", ".env.production"])
        }),
        ("files-leaks-config", &|m: &mut Value| {
            m["files"] = json!(["dist", "tsconfig.json"])
        }),
        ("string-exports", &|m: &mut Value| {
            m["exports"] = json!("./dist/index.mjs")
        }),
        ("empty-exports", &|m: &mut Value| m["exports"] = json!({})),
        ("src-exports", &|m: &mut Value| {
            m["exports"] = json!({".": "./src/index.ts"})
        }),
        ("missing-package-script", &|m: &mut Value| {
            m["scripts"].as_object_mut().unwrap().remove("package");
        }),
        ("file-dependency", &|m: &mut Value| {
            m["dependencies"] = json!({"@corp/core": "file:../core"})
        }),
        ("private-workspace-dependency", &|m: &mut Value| {
            m["dependencies"] = json!({"@corp/internal": "^1.0.0"})
        }),
        ("private-workspace-peer", &|m: &mut Value| {
            m["peerDependencies"] = json!({"@corp/internal": "^1.0.0"})
        }),
    ] {
        let mut candidate = base_ui.clone();
        mutate(&mut candidate);
        write_all(&base_web, &base_internal, &base_core, &candidate);
        assert!(
            assert_native_components(root, &st).is_err(),
            "expected publishable library rejection for {label}"
        );
    }

    // Restore valid state and confirm clean pass.
    write_all(&base_web, &base_internal, &base_core, &base_ui);
    assert_native_components(root, &st).unwrap();
}

#[test]
fn plain_non_component_directories_and_files_in_namespaces_pass_recheck() {
    let f = TempDir::new().unwrap();
    let (_, root, _) = runtime_project(f.path(), "node");
    assert!(!root.join("packages").exists());
    let admitted = admit_runtime_project(&root, "0.5.0").unwrap();

    // Creating a plain non-component folder and file under packages/ and apps/ passes recheck.
    put(&root, "packages/notes/scratch.md", "temporary notes\n");
    put(&root, "apps/README.md", "apps overview\n");
    admitted.recheck().unwrap();

    // Removing the plain folder/file also passes recheck.
    fs::remove_dir_all(root.join("packages/notes")).unwrap();
    fs::remove_file(root.join("apps/README.md")).unwrap();
    admitted.recheck().unwrap();

    // Creating a symlink inside packages/ still fails recheck.
    std::os::unix::fs::symlink("..", root.join("packages/escape")).unwrap();
    assert!(
        admitted
            .recheck()
            .unwrap_err()
            .to_string()
            .contains("namespace changed")
    );
}

const ROOT_MEMBERS: &str = "[workspace]\nresolver='3'\nmembers=['apps/server/crates/api one','apps/worker','packages/protocol','packages/storage']\nexclude=['vendor']\n[workspace.package]\nversion='1.1.0'\n[workspace.dependencies]\nwire={package='protocol',path='packages/protocol',version='^1'}\n";
const SERVER: &str = "[package]\nname='server'\nversion.workspace=true\nworkspace='../../../..'\nedition='2024'\n[dependencies]\nwire.workspace=true\n";
const PROTOCOL: &str = "[package]\nname='protocol'\nversion='1.1.0'\n[build-dependencies]\nstorage={path='../storage'}\n";
fn root_cargo_fixture(root: &Path) -> ProjectState {
    let mut state = state(&[
        ("apps/server", "rust"),
        ("apps/worker", "rust"),
        ("packages/protocol", "rust"),
        ("packages/storage", "rust"),
    ]);
    state.version = "0.6.0".into();
    state
        .components
        .get_mut("apps/server")
        .unwrap()
        .dependencies = vec!["packages/protocol".into(), "packages/storage".into()];
    state
        .components
        .get_mut("packages/protocol")
        .unwrap()
        .dependencies = vec!["packages/storage".into()];
    profiles(root, &state);
    put(root, "Cargo.toml", ROOT_MEMBERS);
    put(root, "rustfmt.toml", "edition='2024'\n");
    put(
        root,
        "rust-toolchain.toml",
        "[toolchain]\nchannel='1.99.1'\n",
    );
    for (dir, manifest) in [
        ("apps/server/crates/api one", SERVER),
        ("apps/worker", "[package]\nname='worker'\nversion='1.0.0'\n"),
        ("packages/protocol", PROTOCOL),
        (
            "packages/storage",
            "[package]\nname='storage'\nversion='1.0.0'\n",
        ),
    ] {
        rust(root, dir, manifest, false);
        put(root, &format!("{dir}/src/lib.rs"), "");
    }
    selected_cargo_lock(
        root,
        ".",
        json!([
            {"name":"server","version":"1.1.0","dependencies":["protocol"]},
            {"name":"worker","version":"1.0.0"},
            {"name":"protocol","version":"1.1.0","dependencies":["storage"]},
            {"name":"storage","version":"1.0.0"}
        ]),
    );
    state
}

#[test]
fn root_cargo_admits_scoped_selected_edges_and_group_members() {
    use devkit::native::{CargoWorkspaceModel, assert_component_manifests};
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = root_cargo_fixture(root);
    let admitted = admit_native_components(root, &state).unwrap();
    assert_eq!(
        admitted.dependencies["apps/server"],
        ["packages/protocol", "packages/storage"]
    );
    assert!(admitted.dependencies["apps/worker"].is_empty());
    let model = admitted.cargo_workspace.as_ref().unwrap();
    let member = &model.members("apps/server").unwrap()[0];
    assert_eq!(member.fmt_package(), "server");
    assert_eq!(
        member.pkgid().unwrap(),
        "path+file:///workspace/apps/server/crates/api%20one#server@1.1.0"
    );
    assert!(admitted.rust_inputs.directories.is_empty());
    assert_eq!(admitted.rust_inputs.files["apps/server/Cargo.toml"], None);
    assert!(admitted.rust_inputs.files["apps/server/crates/api one/Cargo.toml"].is_some());
    fs::remove_file(root.join("Cargo.lock")).unwrap();
    CargoWorkspaceModel::read(root, &state).unwrap();
    assert_component_manifests(root, &state).unwrap();
    assert!(admitted.recheck(root).is_err());
    put(
        root,
        "Cargo.toml",
        &ROOT_MEMBERS.replace(",'packages/storage'", ""),
    );
    assert_component_manifests(root, &state).unwrap();
    collect_native_input_closure(root, &state).unwrap();
    assert!(CargoWorkspaceModel::read(root, &state).is_err());
}

#[test]
fn root_cargo_rejects_shadowing_ambiguous_membership_and_drift() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = root_cargo_fixture(root);
    let original = admit_native_components(root, &state).unwrap();
    for name in [
        ".rustfmt.toml",
        "rust-toolchain",
        "apps/rust-toolchain.toml",
        "apps/server/Cargo.lock",
        "apps/server/crates/.cargo/config.toml",
        "apps/server/crates/api one/rustfmt.toml",
        "packages/protocol/.cargo/config",
        "packages/.rustfmt.toml",
    ] {
        put(root, name, "");
        let err = assert_native_components(root, &state).unwrap_err();
        assert!(format!("{err:#}").contains(name), "{err:#}");
        assert!(original.recheck(root).is_err());
        fs::remove_file(root.join(name)).unwrap();
    }
    for (file, contents, expected) in [
        (
            "Cargo.toml",
            ROOT_MEMBERS.replace("vendor", "apps"),
            "overlaps",
        ),
        (
            "Cargo.toml",
            ROOT_MEMBERS.replace("packages/storage", "packages/*"),
            "glob",
        ),
        (
            "Cargo.toml",
            ROOT_MEMBERS.replace(",'packages/storage'", ""),
            "Implicit Cargo member",
        ),
        (
            "Cargo.toml",
            format!("{ROOT_MEMBERS}\n[package]\nname='root'"),
            "virtual workspace",
        ),
        (
            "apps/server/crates/api one/Cargo.toml",
            SERVER.replace("../../../..", ".."),
            "Cargo.toml",
        ),
        (
            "packages/storage/Cargo.toml",
            "[package]\nname='protocol'\nversion='3.0.0'".into(),
            "Duplicate workspace package",
        ),
        (
            "packages/storage/Cargo.toml",
            "[lib]\n".into(),
            "Missing [package]",
        ),
        (
            "packages/storage/Cargo.toml",
            "[workspace]\n".into(),
            "Nested Cargo workspace",
        ),
        (
            "Cargo.toml",
            ROOT_MEMBERS.replace("version='^1'", "version='^2'"),
            "Cargo.lock differs",
        ),
        (
            "packages/protocol/Cargo.toml",
            PROTOCOL.replace("storage={path='../storage'}", ""),
            "no longer declared",
        ),
        (
            "packages/protocol/Cargo.toml",
            PROTOCOL.replace("storage={path='../storage'}", "storage='1'"),
            "Cargo.lock differs",
        ),
    ] {
        let before = fs::read_to_string(root.join(file)).unwrap();
        put(root, file, &contents);
        let error = assert_native_components(root, &state).unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{file}: expected {expected}, got {error:#}"
        );
        put(root, file, &before);
    }
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    put(
        root,
        "Cargo.lock",
        &format!("{lock}\n[[package]]\nname='removed'\nversion='1.0.0'\n"),
    );
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("unreachable")
    );
}

#[test]
fn root_cargo_keeps_shadow_candidates_separate_from_selected_packages() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = root_cargo_fixture(root);
    rust(
        root,
        "vendor/unused",
        "[package]\nname='storage'\nversion='1.0.0'\n",
        false,
    );
    let patched = format!("{ROOT_MEMBERS}[patch.crates-io]\nstorage={{path='vendor/unused'}}\n");
    put(root, "Cargo.toml", &patched);
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    put(
        root,
        "Cargo.lock",
        &format!("{lock}\n[[patch.unused]]\nname='storage'\nversion='1.0.0'\n"),
    );
    let admitted = admit_native_components(root, &state).unwrap();
    assert!(
        admitted
            .rust_inputs
            .files
            .contains_key("vendor/unused/Cargo.toml")
    );
    put(
        root,
        "packages/protocol/Cargo.toml",
        &PROTOCOL.replace("storage={path='../storage'}", "storage='1'"),
    );
    assert!(assert_native_components(root, &state).is_err());
    put(
        root,
        "Cargo.toml",
        &patched.replace("vendor/unused", "packages/storage"),
    );
    put(root, "Cargo.lock", &lock);
    assert_native_components(root, &state).unwrap();

    // An admitted workspace dependency may cross Node-owned source boundaries
    // neither as an explicit member nor through an excluded local or patch.
    let mut mixed = state.clone();
    mixed
        .components
        .insert("apps/web".into(), component("apps/web", "node"));
    rust(
        root,
        "apps/web/nested",
        "[package]\nname='embedded'\nversion='1.0.0'\n",
        false,
    );
    for manifest in [
        ROOT_MEMBERS.replace("'apps/worker'", "'apps/worker','apps/web/nested'"),
        format!("{ROOT_MEMBERS}unused={{path='apps/web/nested'}}\n"),
        format!("{ROOT_MEMBERS}[patch.crates-io]\nunused={{path='apps/web/nested'}}\n"),
    ] {
        put(root, "Cargo.toml", &manifest);
        let error = devkit::native::CargoWorkspaceModel::read(root, &mixed).unwrap_err();
        assert!(error.to_string().contains("Node component"), "{error:#}");
    }
}

#[test]
#[ignore = "requires Cargo inside the pinned Rust validation container"]
fn pinned_cargo_metadata_matches_root_members_excludes_and_self_dev_edges() {
    let f = TempDir::new().unwrap();
    let root = f.path();
    let state = root_cargo_fixture(root);
    put(
        root,
        "apps/server/crates/api one/Cargo.toml",
        &format!("{SERVER}[dev-dependencies]\nserver={{path='.'}}\n"),
    );
    fs::remove_file(root.join("Cargo.lock")).unwrap();
    let cargo = |args: &[&str]| {
        let out = std::process::Command::new("cargo")
            .current_dir(root)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("CARGO_TARGET_DIR", root.join("target"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    cargo(&["generate-lockfile", "--offline"]);
    let metadata: Value = serde_json::from_slice(&cargo(&[
        "metadata",
        "--offline",
        "--locked",
        "--format-version",
        "1",
    ]))
    .unwrap();
    let admission = admit_native_components(root, &state).unwrap();
    let model = admission.cargo_workspace.unwrap();
    let members: std::collections::BTreeSet<_> = model
        .components
        .values()
        .flatten()
        .map(|m| root.join(&m.rel_dir).join("Cargo.toml"))
        .collect();
    let actual: std::collections::BTreeSet<_> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| {
            metadata["workspace_members"]
                .as_array()
                .unwrap()
                .contains(&p["id"])
        })
        .map(|p| std::path::PathBuf::from(p["manifest_path"].as_str().unwrap()))
        .collect();
    assert_eq!(actual, members);
    assert_eq!(
        metadata["target_directory"].as_str().unwrap(),
        root.join("target").to_str().unwrap()
    );
}

#[test]
fn mixed_workspace_receipts_bind_only_their_ecosystem_inputs() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut state = root_cargo_fixture(root);
    state
        .components
        .insert("apps/web".into(), component("apps/web", "node"));
    node_project(
        root,
        &state,
        &[("apps/web", manifest("web", json!({}), false))],
        json!({}),
    );
    let packages = package_lock(root).unwrap();
    devkit::project::sync_node_workspace(root, &state, None).unwrap();
    devkit::pnpm::import_legacy_npm_lock_seed(root, &packages)
        .unwrap()
        .write(root)
        .unwrap();
    fs::remove_file(root.join("package-lock.json")).unwrap();
    let a = admit_native_components(root, &state).unwrap();
    put(root, "rustfmt.toml", "edition='2024'\nmax_width=88\n");
    let b = admit_native_components(root, &state).unwrap();
    assert_eq!(a.node_inputs.digest, b.node_inputs.digest);
    assert_ne!(a.rust_inputs.digest, b.rust_inputs.digest);
    assert_eq!(b.inputs.directories, b.node_inputs.directories);
    let mut web: Value = read_json(&root.join("apps/web/package.json")).unwrap();
    web["scripts"]["build"] = json!("node build.mjs");
    put_json(root, "apps/web/package.json", &web);
    let c = admit_native_components(root, &state).unwrap();
    assert_ne!(b.node_inputs.digest, c.node_inputs.digest);
    assert_eq!(b.rust_inputs.digest, c.rust_inputs.digest);
}

#[test]
#[ignore = "requires Cargo and the author cache inside the pinned validation container"]
fn pinned_cargo_metadata_validates_excluded_edges_aliases_and_patch_sources() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let state = root_cargo_fixture(root);
    let worker = "[package]\nname='worker'\nversion='1.0.0'\n[dependencies]\n";
    let helper = "[package]\nname='helper'\nversion='1.0.0'\n[dependencies]\n";
    let worker_path = "apps/worker/Cargo.toml";
    let helper_path = "vendor/helper/Cargo.toml";
    put(root, "vendor/helper/src/lib.rs", "");
    let cargo = |args: &[&str]| {
        std::process::Command::new("cargo")
            .current_dir(root)
            .args(args)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("CARGO_TARGET_DIR", root.join("target"))
            .output()
            .unwrap()
    };
    let generate = || {
        let result = cargo(&["generate-lockfile", "--offline"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    let agrees = |accepted| {
        let result = cargo(&["metadata", "--locked", "--offline", "--format-version", "1"]);
        assert_eq!(
            result.status.success(),
            accepted,
            "Cargo: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let derived = derive_component_dependencies(root, &state);
        assert_eq!(derived.is_ok(), accepted, "Devkit: {derived:?}");
    };
    put(
        root,
        worker_path,
        &format!("{worker}helper={{path='../../vendor/helper'}}\n"),
    );
    put(
        root,
        helper_path,
        &format!(
            "{helper}storage={{path='../../packages/storage',optional=true}}\n[dev-dependencies]\nprotocol={{path='../../packages/protocol'}}\n"
        ),
    );
    generate();
    agrees(true);
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/worker"].is_empty());
    put(
        root,
        helper_path,
        &format!("{helper}storage={{path='../../packages/storage'}}\n"),
    );
    agrees(false);
    generate();
    agrees(true);
    assert_eq!(
        derive_component_dependencies(root, &state).unwrap()["apps/worker"],
        ["packages/storage"]
    );

    // Cargo owns feature activation. Updating the lock explicitly after a
    // feature-only edit selects the optional edge from the excluded crate.
    put(
        root,
        helper_path,
        &format!(
            "{helper}storage={{path='../../packages/storage',optional=true}}\n[features]\nactivate=['dep:storage']\n"
        ),
    );
    generate();
    agrees(true);
    put(
        root,
        worker_path,
        &format!("{worker}helper={{path='../../vendor/helper',features=['activate']}}\n"),
    );
    assert!(
        !cargo(&["metadata", "--locked", "--offline", "--format-version", "1"])
            .status
            .success()
    );
    assert!(
        cargo(&["update", "--workspace", "--offline"])
            .status
            .success()
    );
    agrees(true);

    // Both versions are present in the pinned author's own Cargo.lock/cache.
    put(
        root,
        worker_path,
        &format!(
            "{worker}new={{package='base64',version='=0.23.1'}}\nold={{package='base64',version='=0.22.1'}}\n"
        ),
    );
    generate();
    agrees(true);
    put(
        root,
        worker_path,
        &format!(
            "{worker}new={{package='base64',version='*'}}\n[target.'cfg(windows)'.dependencies]\nnew={{package='base64',version='*'}}\n"
        ),
    );
    agrees(false);
    generate();
    agrees(true);

    // The extra registry URL is never contacted. Cargo's --locked decision
    // and Devkit's graph should both notice when the applicable patch moves.
    put(root, worker_path, &format!("{worker}cfg-if='=1.0.5'\n"));
    rust(
        root,
        "vendor/cfg-if",
        "[package]\nname='cfg-if'\nversion='1.0.5'\n",
        false,
    );
    put(root, "vendor/cfg-if/src/lib.rs", "");
    let config = "[patch.crates-io]\ncfg-if={path='vendor/cfg-if'}\n";
    put(root, ".cargo/config.toml", config);
    generate();
    agrees(true);
    put(
        root,
        ".cargo/config.toml",
        &config.replace("crates-io", "'https://other.example/index'"),
    );
    agrees(false);
}

#[test]
fn root_cargo_binds_git_refs_and_custom_registry_indexes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let state = root_cargo_fixture(root);
    put(
        root,
        ".cargo/config.toml",
        "[registries.internal]\nindex='sparse+https://example.org/index/'\n",
    );
    let worker = "[package]\nname='worker'\nversion='1.0.0'\n[dependencies]\n";
    let manifest = format!(
        "{worker}remote={{git='https://example.org/repo',branch='feat/one+two'}}\ncustom={{version='1',registry='internal'}}\n"
    );
    put(root, "apps/worker/Cargo.toml", &manifest);
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let mut lock: toml::Table = lock.parse().unwrap();
    let packages = lock["package"].as_array_mut().unwrap();
    let worker = packages
        .iter_mut()
        .find(|p| p["name"].as_str() == Some("worker"))
        .unwrap();
    worker.as_table_mut().unwrap().insert(
        "dependencies".into(),
        toml::Value::Array(vec!["remote".into(), "custom".into()]),
    );
    for (name, source) in [
        (
            "remote",
            "git+https://example.org/repo?branch=feat%2Fone%2Btwo#abc",
        ),
        ("custom", "sparse+https://example.org/index/"),
    ] {
        packages.push(
            toml::Value::try_from(json!({"name":name,"version":"1.0.0","source":source})).unwrap(),
        );
    }
    put(root, "Cargo.lock", &toml::to_string(&lock).unwrap());
    assert_native_components(root, &state).unwrap();
    for changed in [
        manifest.replace("feat/one+two", "feat/main"),
        manifest.replace("branch=", "tag="),
        manifest.replace("branch=", "rev="),
    ] {
        put(root, "apps/worker/Cargo.toml", &changed);
        assert!(assert_native_components(root, &state).is_err());
    }
    put(root, "apps/worker/Cargo.toml", &manifest);
    put(
        root,
        ".cargo/config.toml",
        "[registries.internal]\nindex='sparse+https://other.example/index/'\n",
    );
    assert!(assert_native_components(root, &state).is_err());
}

#[test]
#[ignore = "requires the pinned Cargo version and the author's offline cache"]
fn pinned_cargo_metadata_distinguishes_patch_precedence_and_replacement_sources() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut state = root_cargo_fixture(root);
    let provider = "packages/provider";
    state
        .components
        .insert(provider.into(), component(provider, "rust"));
    let root_manifest = ROOT_MEMBERS.replace(
        "'packages/storage']",
        "'packages/storage','packages/provider']",
    );
    let cargo = |args: &[&str]| {
        std::process::Command::new("cargo")
            .current_dir(root)
            .args(args)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("CARGO_TARGET_DIR", root.join("target"))
            .output()
            .unwrap()
    };
    let fresh = || {
        for args in [
            &["generate-lockfile", "--offline"][..],
            &["metadata", "--locked", "--offline", "--format-version", "1"],
        ] {
            let out = cargo(args);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    };
    put(
        root,
        "apps/worker/Cargo.toml",
        "[package]\nname='worker'\nversion='1.0.0'\n[dependencies]\nserde_json='=1.0.154'\n",
    );
    rust(
        root,
        provider,
        "[package]\nname='itoa'\nversion='1.0.18'\n",
        false,
    );
    put(root, "packages/provider/src/lib.rs", "");
    // A registry crate may reach local code through either override mechanism.
    // Cargo compares canonicalized origins for patches and exact package
    // specification URLs for replacements.
    for (table, expected) in [
        (
            "[patch.'https://github.com/RUST-LANG/crates.io-index.git/']\nitoa={path='packages/provider'}\n",
            true,
        ),
        (
            "[replace]\n'itoa:1.0.18'={path='packages/provider'}\n",
            true,
        ),
        (
            "[replace]\n'registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.18'={path='packages/provider'}\n",
            true,
        ),
        (
            "[replace]\n'https://github.com/rust-lang/crates.io-index.git#itoa@1.0.18'={path='packages/provider'}\n",
            false,
        ),
    ] {
        put(root, "Cargo.toml", &format!("{root_manifest}{table}"));
        fresh();
        assert_eq!(
            derive_component_dependencies(root, &state).unwrap()["apps/worker"]
                .contains(&provider.into()),
            expected,
            "{table}"
        );
        if expected {
            put(root, "Cargo.toml", &root_manifest);
            assert!(
                derive_component_dependencies(root, &state).is_err(),
                "{table}"
            );
            assert!(
                !cargo(&["metadata", "--locked", "--offline", "--format-version", "1"])
                    .status
                    .success()
            );
            fresh();
            assert!(derive_component_dependencies(root, &state).unwrap()["apps/worker"].is_empty());
        }
    }
    // The excluded vendor manifests share one local identity. Config selects
    // the directory while both manifests remain tracked inputs.
    rust(
        root,
        "vendor/itoa",
        "[package]\nname='itoa'\nversion='1.0.18'\n",
        false,
    );
    put(root, "vendor/itoa/src/lib.rs", "");
    // Remove the matching member to exercise a non-pinned local ID.
    put(
        root,
        "packages/provider/Cargo.toml",
        "[package]\nname='provider'\nversion='1.0.0'\n",
    );
    rust(
        root,
        "vendor/shadow",
        "[package]\nname='itoa'\nversion='1.0.18'\n",
        false,
    );
    put(root, "vendor/shadow/src/lib.rs", "");
    put(
        root,
        "Cargo.toml",
        &format!("{root_manifest}[patch.crates-io]\nitoa={{path='vendor/shadow'}}\n"),
    );
    put(root, ".cargo/config.toml", "include=['patch.toml']\n");
    put(
        root,
        ".cargo/patch.toml",
        "[patch.'https://github.com/rust-lang/crates.io-index.git']\nitoa={path='vendor/itoa'}\n",
    );
    fresh();
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/worker"].is_empty());
    let inputs = collect_native_input_closure(root, &state).unwrap();
    assert!(inputs.files["vendor/shadow/Cargo.toml"].is_some());
    assert!(inputs.files["vendor/itoa/Cargo.toml"].is_some());

    // Removing the last direct edge leaves real registry records orphaned.
    // Cargo drops them from [[package]] and tracks the unused patch separately.
    put(
        root,
        "apps/worker/Cargo.toml",
        "[package]\nname='worker'\nversion='1.0.0'\n",
    );
    let previous = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let mut stale: toml::Table = previous.parse().unwrap();
    let worker = stale["package"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["name"].as_str() == Some("worker"))
        .unwrap();
    worker.as_table_mut().unwrap().remove("dependencies");
    put(root, "Cargo.lock", &toml::to_string(&stale).unwrap());
    assert!(
        derive_component_dependencies(root, &state)
            .unwrap_err()
            .to_string()
            .contains("unreachable")
    );
    assert!(
        !cargo(&["metadata", "--locked", "--offline", "--format-version", "1"])
            .status
            .success()
    );
    let repaired = cargo(&["metadata", "--offline", "--format-version", "1"]);
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert!(derive_component_dependencies(root, &state).unwrap()["apps/worker"].is_empty());
}

#[test]
fn cargo_config_rejects_executable_hooks_credentials_root_include_diamond_and_local_urls() {
    for (label, config, extra_file, expected) in [
        (
            "paths",
            "paths = ['vendor/a']\n",
            None,
            "Unsupported Cargo config paths override",
        ),
        (
            "env",
            "[env]\nFOO = 'bar'\n",
            None,
            "Unsupported Cargo config section [env]",
        ),
        (
            "alias",
            "[alias]\nx = 'build'\n",
            None,
            "Unsupported Cargo config section [alias]",
        ),
        (
            "http",
            "[http]\ncainfo = '/tmp/ca.pem'\n",
            None,
            "Unsupported Cargo config section [http]",
        ),
        (
            "rustc-wrapper",
            "[build]\nrustc-wrapper = '/bin/sh'\n",
            None,
            "Unsupported Cargo config build.rustc-wrapper",
        ),
        (
            "rustc-workspace-wrapper",
            "[build]\nrustc-workspace-wrapper = '/bin/sh'\n",
            None,
            "Unsupported Cargo config build.rustc-workspace-wrapper",
        ),
        (
            "rustc",
            "[build]\nrustc = '/bin/sh'\n",
            None,
            "Unsupported Cargo config build.rustc",
        ),
        (
            "rustdoc",
            "[build]\nrustdoc = '/bin/sh'\n",
            None,
            "Unsupported Cargo config build.rustdoc",
        ),
        (
            "target-dir",
            "[build]\ntarget-dir = '/tmp/out'\n",
            None,
            "Unsupported Cargo config build.target-dir",
        ),
        (
            "build-dir",
            "[build]\nbuild-dir = '/tmp/out'\n",
            None,
            "Unsupported Cargo config build.build-dir",
        ),
        (
            "build-rustflags",
            "[build]\nrustflags = ['--cap-lints=allow']\n",
            None,
            "Unsupported Cargo config build.rustflags",
        ),
        (
            "build-rustdocflags",
            "[build]\nrustdocflags = ['--cap-lints=allow']\n",
            None,
            "Unsupported Cargo config build.rustdocflags",
        ),
        (
            "target-rustflags",
            "[target.'cfg(all())']\nrustflags = ['-C', 'linker=/bin/sh']\n",
            None,
            "Unsupported Cargo config target.rustflags",
        ),
        (
            "target-rustdocflags",
            "[target.'cfg(all())']\nrustdocflags = ['--cap-lints=allow']\n",
            None,
            "Unsupported Cargo config target.rustdocflags",
        ),
        (
            "git-fetch-with-cli",
            "[net]\ngit-fetch-with-cli = true\n",
            None,
            "Unsupported Cargo config net.git-fetch-with-cli",
        ),
        (
            "credential-providers",
            "[registry]\nglobal-credential-providers = ['cargo:token']\n",
            None,
            "Unsupported Cargo config registry.global-credential-providers",
        ),
        (
            "registry-token",
            "[registry]\ntoken = 'secret'\n",
            None,
            "Unsupported Cargo config registry.token",
        ),
        (
            "registries-credential",
            "[registries.corp]\nindex = 'sparse+https://example.invalid/index/'\ncredential-provider = '/bin/sh'\n",
            None,
            "Unsupported Cargo config registries.credential-provider",
        ),
        (
            "target-runner",
            "[target.x86_64-unknown-linux-gnu]\nrunner = '/bin/sh'\n",
            None,
            "Unsupported Cargo config target.runner",
        ),
        (
            "target-linker",
            "[target.x86_64-unknown-linux-gnu]\nlinker = '/bin/sh'\n",
            None,
            "Unsupported Cargo config target.linker",
        ),
        (
            "absolute-include",
            "include = ['/etc/config.toml']\n",
            None,
            "Cargo config include must be a relative path",
        ),
        (
            "root-level-include",
            "include = ['../root.toml']\n",
            Some(("root.toml", "[patch.crates-io]\n")),
            "must live in a subdirectory below the checkout root",
        ),
        (
            "uppercase-file-registry",
            "[registries.local]\nindex = 'SPARSE+FILE:///tmp/index'\n",
            None,
            "Unsupported local Cargo",
        ),
    ] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let state = state(&[("apps/api", "rust")]);
        rust(
            root,
            "apps/api",
            "[package]\nname='api'\nversion='1.0.0'\n",
            true,
        );
        profiles(root, &state);
        put(root, ".cargo/config.toml", config);
        if let Some((path, body)) = extra_file {
            put(root, path, body);
        }
        let err = format!("{:#}", admit_native_components(root, &state).err().unwrap());
        assert!(err.contains(expected), "{label}: {err}");
    }

    // Diamond include is rejected fail-closed.
    {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let state = state(&[("apps/api", "rust")]);
        rust(
            root,
            "apps/api",
            "[package]\nname='api'\nversion='1.0.0'\n",
            true,
        );
        profiles(root, &state);
        put(
            root,
            ".cargo/config.toml",
            "include = ['a.toml', 'b.toml']\n",
        );
        put(root, ".cargo/a.toml", "include = ['shared.toml']\n");
        put(root, ".cargo/b.toml", "include = ['shared.toml']\n");
        put(root, ".cargo/shared.toml", "[patch.crates-io]\n");
        let err = format!("{:#}", admit_native_components(root, &state).err().unwrap());
        assert!(err.contains("Duplicate Cargo config include"), "{err}");
    }

    // Absolute dependency path, absolute package.workspace, and uppercase FILE: git URL are rejected.
    for (label, manifest) in [
        (
            "absolute-dep-path",
            "[package]\nname='api'\nversion='1.0.0'\n[dependencies]\nhelper={path='/tmp/helper'}\n",
        ),
        (
            "absolute-workspace",
            "[package]\nname='api'\nversion='1.0.0'\nworkspace='/tmp'\n",
        ),
        (
            "uppercase-git-file",
            "[package]\nname='api'\nversion='1.0.0'\n[dependencies]\nhelper={git='FILE:///tmp/repo'}\n",
        ),
        (
            "control-char-git",
            "[package]\nname='api'\nversion='1.0.0'\n[dependencies]\nhelper={git=\"https://example.invalid/repo\\nbar\"}\n",
        ),
    ] {
        let f = TempDir::new().unwrap();
        let root = f.path();
        let state = state(&[("apps/api", "rust")]);
        rust(root, "apps/api", manifest, true);
        profiles(root, &state);
        assert!(
            admit_native_components(root, &state).is_err(),
            "{label} unexpectedly succeeded"
        );
    }
}
