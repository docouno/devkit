use super::*;
use devkit::cargo_lock::CargoLockResolver;

struct NoCargoResolver;
impl CargoLockResolver for NoCargoResolver {
    fn resolve_cargo(&self, _: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
        bail!("unexpected external Cargo resolution")
    }
}

fn offline() -> UpdateOptions<'static> {
    UpdateOptions {
        offline: true,
        cargo_resolver: Some(&NoCargoResolver),
        ..Default::default()
    }
}

fn cargo_file(root: &Path, path: &str) -> toml::Value {
    toml::from_str(&fs::read_to_string(root.join(path)).unwrap()).unwrap()
}

#[test]
fn cargo_resolution_travels_with_the_checkout_and_binds_included_config() {
    struct Confirm;
    impl CargoLockResolver for Confirm {
        fn resolve_cargo(&self, _: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    let initial = init(&release, &root, "rust-library", PackageMode::Local);
    let manifest = root.join("packages/core/Cargo.toml");
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(&manifest, format!("# project comment\n{original}")).unwrap();
    put(&root, ".cargo/display.toml", b"[term]\nverbose=true\n");
    for config in [
        b"".as_slice(),
        b"[term]\nverbose=true\n",
        b"include=['display.toml']\n",
    ] {
        put(&root, ".cargo/config.toml", config);
        // Setup and describe use this admission before running their tasks.
        let runtime = admit_runtime_project(&root, "0.6.0").unwrap();
        assert_eq!(runtime.state.cargo_resolution, initial.cargo_resolution);
    }
    let no_op = sync_project(&release, &root, &offline(), &NoResolver).unwrap();
    assert!(no_op.no_op);
    assert_eq!(initial.generation, no_op.state.generation);

    fs::write(&manifest, format!("{original}\n[features]\ndefault = []\n")).unwrap();
    put(
        &root,
        ".cargo/config.toml",
        b"include=[{path='overrides.toml',optional=true}]\n",
    );
    let sync = |preview| {
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                preview,
                cargo_resolver: Some(&Confirm),
                ..offline()
            },
            &NoResolver,
        )
    };
    let prior = fs::read(root.join(MANIFEST_PATH)).unwrap();
    let preview = sync(true).unwrap();
    assert!(preview.files.iter().any(|f| f.path == MANIFEST_PATH));
    assert_eq!(fs::read(root.join(MANIFEST_PATH)).unwrap(), prior);
    let accepted = sync(false).unwrap().state;
    assert_ne!(accepted.cargo_resolution, initial.cargo_resolution);
    put(
        &root,
        ".cargo/overrides.toml",
        b"[resolver]\nincompatible-rust-versions='fallback'\n",
    );
    let err = devkit::native::admit_native_components(&root, &accepted)
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("make sync"), "{err:#}");
    let final_state = sync(false).unwrap().state;
    assert_ne!(accepted.cargo_resolution, final_state.cargo_resolution);
    let copied = temp.path().join("new-checkout");
    copy_tree(&root, &copied, &|p| p.starts_with(".devkit/state"), true).unwrap();
    let after = sync_project(&release, &copied, &offline(), &NoResolver).unwrap();
    assert!(after.no_op);
    assert_eq!(after.state.generation, final_state.generation);
}

#[test]
fn rust_templates_share_one_lock_and_sync_reconciles_local_changes_without_a_resolver() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    let state = init_project(
        &release,
        &root,
        &InitOptions {
            template: Some("rust-library".into()),
            offline: true,
            cargo_resolver: Some(&NoCargoResolver),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    assert!(state.components.contains_key("packages/core"));
    let comment = "# workspace owned by the product\n";
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    put(
        &root,
        "Cargo.toml",
        format!("{comment}{manifest}").as_bytes(),
    );
    for (role, slug, template, uses) in [
        (Role::Library, "protocol", "rust-library", vec!["core"]),
        (Role::App, "server", "rust-api", vec!["protocol"]),
        (Role::App, "worker", "rust-api", vec![]),
    ] {
        let inventory = capture_compose_inputs(&root).unwrap();
        let spec = ComponentSpec {
            name: slug.into(),
            template: template.into(),
            uses: uses.iter().copied().map(String::from).collect(),
        };
        let options = UpdateOptions {
            compose_inventory: Some(&inventory),
            idempotent: true,
            ..offline()
        };
        let outcome =
            add_project_component(&release, &root, role, spec.clone(), &options, &NoResolver)
                .unwrap();
        assert!(outcome.applied);
        let repeat =
            add_project_component(&release, &root, role, spec, &options, &NoResolver).unwrap();
        assert!(repeat.no_op);
        assert_eq!(outcome.state.generation, repeat.state.generation);
    }
    for (path, name) in [
        ("packages/core", "lib-core"),
        ("packages/protocol", "lib-protocol"),
        ("apps/server", "app-server"),
        ("apps/worker", "app-worker"),
    ] {
        let member = cargo_file(&root, &format!("{path}/Cargo.toml"));
        assert_eq!(member["package"]["name"].as_str(), Some(name));
        for authority in ["Cargo.lock", "rust-toolchain.toml", "rustfmt.toml"] {
            assert!(root.join(authority).is_file());
            assert!(!root.join(path).join(authority).exists());
        }
    }
    let server = cargo_file(&root, "apps/server/Cargo.toml");
    assert_eq!(
        server["dependencies"]["lib-protocol"]["path"].as_str(),
        Some("../../packages/protocol")
    );
    let protocol = cargo_file(&root, "packages/protocol/Cargo.toml");
    assert_eq!(
        protocol["dependencies"]["lib-core"]["path"].as_str(),
        Some("../core")
    );
    let state = read_project_state(&root).unwrap();
    assert!(
        state.components["apps/server"]
            .dependencies
            .contains(&"packages/protocol".to_string())
    );
    assert_eq!(
        state.components["packages/protocol"].dependencies,
        ["packages/core"]
    );
    assert!(state.components["apps/worker"].dependencies.is_empty());
    assert!(
        fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .starts_with(comment)
    );

    let original = fs::read_to_string(root.join("apps/worker/Cargo.toml")).unwrap();
    let mut worker: toml_edit::DocumentMut = original.parse().unwrap();
    worker["dependencies"]["lib-protocol"]["path"] = toml_edit::value("../../packages/protocol");
    put(
        &root,
        "apps/worker/Cargo.toml",
        worker.to_string().as_bytes(),
    );
    let synced = sync_project(&release, &root, &offline(), &NoResolver).unwrap();
    assert!(synced.applied);
    assert!(
        synced.state.components["apps/worker"]
            .dependencies
            .contains(&"packages/protocol".to_string())
    );
    // Remove the only edge and a registered member in one edit.
    put(&root, "apps/worker/Cargo.toml", original.as_bytes());
    let mut workspace: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let members = workspace["workspace"]["members"].as_array_mut().unwrap();
    let worker = members
        .iter()
        .position(|v| v.as_str() == Some("apps/worker"))
        .unwrap();
    members.remove(worker);
    put(&root, "Cargo.toml", workspace.to_string().as_bytes());
    let restored = sync_project(&release, &root, &offline(), &NoResolver).unwrap();
    assert!(restored.applied);
    assert!(
        restored.state.components["apps/worker"]
            .dependencies
            .is_empty()
    );
    assert!(
        cargo_file(&root, "Cargo.toml")["workspace"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str() == Some("apps/worker"))
    );
    assert!(
        sync_project(&release, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

#[test]
fn register_adopts_standalone_cargo_in_rust_and_node_projects() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for (template, with_lock) in [("node-api", true), ("rust-api", true), ("rust-api", false)] {
        let root = temp.path().join(format!("{template}-{with_lock}"));
        init(&release, &root, template, PackageMode::Local);
        let pnpm = (template == "node-api").then(|| {
            preserved(
                &root,
                &["package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml"],
            )
        });
        let path = root.join("packages/shared");
        let manifest =
            b"# keep formatting\n[package]\nname='owner-lib'\nversion='2.3.4'\nedition='2024'\n";
        put(&path, "Cargo.toml", manifest);
        put(&path, "src/lib.rs", b"pub fn identity() {}\n");
        if with_lock {
            put(
                &path,
                "Cargo.lock",
                b"version = 4\n[[package]]\nname='owner-lib'\nversion='2.3.4'\n",
            );
        }
        // Predeclaring a new component before register is a valid adoption workflow.
        if template == "rust-api" {
            let mut workspace: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
                .unwrap()
                .parse()
                .unwrap();
            workspace["workspace"]["members"]
                .as_array_mut()
                .unwrap()
                .push("packages/shared");
            put(&root, "Cargo.toml", workspace.to_string().as_bytes());
        }
        let register = || {
            register_project_component(
                &release,
                &root,
                Role::Library,
                "shared".into(),
                ToolingProfile::Rust,
                &UpdateOptions {
                    idempotent: true,
                    ..offline()
                },
                &NoResolver,
            )
        };
        let first = register().unwrap();
        assert!(first.applied);
        assert!(register().unwrap().no_op);
        assert_eq!(fs::read(path.join("Cargo.toml")).unwrap(), manifest);
        assert!(!path.join("Cargo.lock").exists());
        assert!(
            cargo_file(&root, "Cargo.lock")["package"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pkg| pkg["name"].as_str() == Some("owner-lib")
                    && pkg["version"].as_str() == Some("2.3.4"))
        );
        if let Some(original) = pnpm {
            assert_eq!(
                preserved(
                    &root,
                    &["package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml"]
                ),
                original
            );
        }
        put(&path, "Cargo.lock", b"unexpected shadow\n");
        let corrupt = author_port_payload(&root);
        assert!(register().is_err());
        assert_eq!(author_port_payload(&root), corrupt);
    }
}

#[test]
fn register_discovers_multiple_members_and_rejects_ambiguous_use() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("group");
    init(&release, &root, "rust-api", PackageMode::Local);
    for name in ["wire", "types"] {
        let path = root.join("packages/protocol/crates").join(name);
        put(
            &path,
            "Cargo.toml",
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n").as_bytes(),
        );
        put(&path, "src/lib.rs", b"pub fn answer() -> u32 { 42 }\n");
    }
    register_project_component(
        &release,
        &root,
        Role::Library,
        "protocol".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap();
    let workspace = cargo_file(&root, "Cargo.toml");
    let members = workspace["workspace"]["members"].as_array().unwrap();
    for name in ["wire", "types"] {
        let path = format!("packages/protocol/crates/{name}");
        assert!(members.iter().any(|m| m.as_str() == Some(path.as_str())));
    }
    let before = author_port_payload(&root);
    let err = add_project_component(
        &release,
        &root,
        Role::Library,
        ComponentSpec {
            name: "other".into(),
            template: "rust-library".into(),
            uses: vec!["protocol".into()],
        },
        &offline(),
        &NoResolver,
    )
    .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("wire") && message.contains("types"),
        "{message}"
    );
    assert_eq!(author_port_payload(&root), before);
}

#[test]
fn external_dependency_changes_resolve_in_staging_and_preserve_user_files_on_failure() {
    struct Resolver {
        calls: std::cell::Cell<usize>,
        checkout: PathBuf,
        fail: bool,
    }
    impl CargoLockResolver for Resolver {
        fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(!offline);
            assert_ne!(root, self.checkout);
            self.calls.set(self.calls.get() + 1);
            let manifest = cargo_file(root, "packages/core/Cargo.toml");
            let mut lock =
                String::from("version = 4\n[[package]]\nname='lib-core'\nversion='1.0.0'\n");
            if let Some(req) = manifest.get("dependencies").and_then(|d| d.get("external")) {
                let version = req.as_str().unwrap();
                lock.push_str(&format!("dependencies=['external']\n[[package]]\nname='external'\nversion='{version}'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\n", "a".repeat(64)));
            }
            put(root, "Cargo.lock", lock.as_bytes());
            if self.fail {
                bail!("fixture resolver failed after staging Cargo.lock");
            }
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    let original = fs::read_to_string(root.join("packages/core/Cargo.toml")).unwrap();
    let success = Resolver {
        calls: 0.into(),
        checkout: root.clone(),
        fail: false,
    };
    for version in [Some("1.0.0"), Some("2.0.0"), None] {
        let mut manifest: toml_edit::DocumentMut = original.parse().unwrap();
        if let Some(version) = version {
            manifest["dependencies"]["external"] = toml_edit::value(version);
        }
        put(
            &root,
            "packages/core/Cargo.toml",
            manifest.to_string().as_bytes(),
        );
        let before = author_port_payload(&root);
        let failure = Resolver {
            calls: 0.into(),
            checkout: root.clone(),
            fail: true,
        };
        let opts = |resolver| UpdateOptions {
            cargo_resolver: Some(resolver),
            ..Default::default()
        };
        let err = sync_project(&release, &root, &opts(&failure), &NoResolver).unwrap_err();
        assert!(
            format!("{err:#}").contains("fixture resolver failed"),
            "{err:#}"
        );
        assert_eq!(failure.calls.get(), 1);
        assert_eq!(author_port_payload(&root), before);
        let outcome = sync_project(&release, &root, &opts(&success), &NoResolver).unwrap();
        assert!(outcome.applied);
        assert!(
            sync_project(&release, &root, &offline(), &NoResolver)
                .unwrap()
                .no_op
        );
    }
    assert_eq!(success.calls.get(), 3);
}

fn init_legacy_rust_pair(release: &Path, root: &Path) {
    init_project(
        release,
        root,
        &InitOptions {
            apps: Some(
                ["server", "worker"]
                    .into_iter()
                    .map(|name| ComponentSpec {
                        name: name.into(),
                        template: "rust-api".into(),
                        uses: vec![],
                    })
                    .collect(),
            ),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
}

#[test]
fn migration_preview_and_apply_reject_conflicts_without_changing_the_checkout() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init_legacy_rust_pair(&old, &root);
    let conflict = |paths: &[&str]| {
        let before = author_port_payload(&root);
        for preview in [true, false] {
            let err = update_project(
                &next,
                &root,
                &UpdateOptions {
                    preview,
                    ..offline()
                },
                &NoResolver,
            )
            .unwrap_err();
            let msg = format!("{err:#}");
            for path in paths {
                assert!(msg.contains(path), "missing {path}: {msg}");
            }
            assert_eq!(author_port_payload(&root), before);
            assert!(!root.join(PENDING_PATH).exists());
        }
    };
    conflict(&["apps/server/Cargo.toml", "apps/worker/Cargo.toml"]);
    for name in ["Cargo.lock", "Cargo.toml"] {
        let path = root.join("apps/worker").join(name);
        fs::write(
            &path,
            fs::read_to_string(&path)
                .unwrap()
                .replace("fixture-api", "worker-owned"),
        )
        .unwrap();
    }
    put(&root, "Cargo.lock", b"already owned by the consumer");
    conflict(&["Cargo.lock"]);
    remove_file(&root.join("Cargo.lock")).unwrap();
    let config = fs::read(root.join("apps/worker/rustfmt.toml")).unwrap();
    put(&root, "apps/worker/rustfmt.toml", b"edition = '2021'\n");
    conflict(&["rustfmt.toml"]);
    put(&root, "apps/worker/rustfmt.toml", &config);
    let worker = fs::read_to_string(root.join("apps/worker/Cargo.toml")).unwrap();
    put(
        &root,
        "apps/worker/Cargo.toml",
        format!("{worker}\n[profile.release]\nstrip=true\n").as_bytes(),
    );
    conflict(&["profile"]);
    put(&root, "apps/worker/Cargo.toml", worker.as_bytes());
    put(
        &root,
        "apps/worker/.cargo/config.toml",
        b"[net]\noffline=true\n",
    );
    conflict(&[".cargo/config.toml"]);
    remove_file(&root.join("apps/worker/.cargo/config.toml")).unwrap();

    let server = fs::read_to_string(root.join("apps/server/Cargo.toml")).unwrap();
    let manifest = |original: &str, app: &str, lint: &str, inherit_readme: bool| {
        let readme = if inherit_readme {
            "readme.workspace = true"
        } else {
            ""
        };
        let default_readme = if inherit_readme { "readme = true" } else { "" };
        format!(
            "{original}{readme}\n[workspace] # {app} header\nresolver = '3' # {app} resolver\n\n# {app} metadata\n[workspace.package]\n{default_readme}\nlicense = 'MIT' # {app} license\n[workspace.lints.rust]\n{lint} = 'forbid'\n[lints]\nworkspace = true\n"
        )
    };
    let set_server = |lint, readme| {
        put(
            &root,
            "apps/server/Cargo.toml",
            manifest(&server, "server", lint, readme).as_bytes(),
        );
    };
    put(
        &root,
        "apps/worker/Cargo.toml",
        manifest(&worker, "worker", "unsafe_code", true).as_bytes(),
    );
    put(&root, "apps/worker/README.md", b"Worker crate readme");
    set_server("unused_variables", false);
    conflict(&["lints", "apps/server/Cargo.toml", "apps/worker/Cargo.toml"]);
    set_server("unsafe_code", true);
    conflict(&["readme", "apps/server/Cargo.toml", "apps/worker/Cargo.toml"]);
    set_server("unsafe_code", false);
    // Independent registries can patch in the same unused local crate. Cargo
    // records one unused row per origin after the workspaces are combined.
    put(
        &root,
        "vendor/helper/Cargo.toml",
        b"[package]\nname='helper'\nversion='1.0.0'\nedition='2024'\n",
    );
    put(
        &root,
        "vendor/helper/src/lib.rs",
        b"pub fn answer() -> u8 { 42 }\n",
    );
    for app in ["server", "worker"] {
        let dir = root.join("apps").join(app);
        let manifest = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        put(&dir, "Cargo.toml", format!("{manifest}\n[patch.'https://{app}.example.test/index']\nhelper = {{path='../../vendor/helper'}}\n").as_bytes());
        let lock = fs::read_to_string(dir.join("Cargo.lock")).unwrap();
        put(
            &dir,
            "Cargo.lock",
            format!("{lock}\n[[patch.unused]]\nname='helper'\nversion='1.0.0'\n").as_bytes(),
        );
    }

    let before = author_port_payload(&root);
    let preview = update_project(
        &next,
        &root,
        &UpdateOptions {
            preview: true,
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    assert_eq!(author_port_payload(&root), before);
    let changed: std::collections::BTreeSet<_> =
        preview.files.iter().map(|f| f.path.as_str()).collect();
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "rustfmt.toml",
        "apps/server/Cargo.lock",
        "apps/worker/rustfmt.toml",
    ] {
        assert!(changed.contains(path), "{path}: {changed:?}");
    }
    let applied = update_project(&next, &root, &offline(), &NoResolver).unwrap();
    assert!(applied.applied);
    let lock = cargo_file(&root, "Cargo.lock");
    let unused = lock["patch"]["unused"].as_array().unwrap();
    assert_eq!(unused.len(), 2);
    assert_eq!(unused[0], unused[1]);
    let output = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let settings = cargo_file(&root, "Cargo.toml");
    assert_eq!(
        settings["workspace"]["package"]["readme"].as_str(),
        Some("apps/worker/README.md")
    );
    assert_eq!(
        settings["workspace"]["lints"]["rust"]["unsafe_code"].as_str(),
        Some("forbid")
    );
    for label in ["header", "resolver", "metadata", "license"] {
        let text = format!("# server {label}");
        assert_eq!(output.matches(&text).count(), 1, "{text}: {output}");
    }

    assert!(
        sync_project(&next, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

#[test]
fn migration_coalesces_equivalent_patch_sources_without_losing_selected_packages() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init_legacy_rust_pair(&old, &root);
    let manifest = |app, dep, origin, key| {
        format!(
            "[package]\nname='{app}'\nversion='1.0.0'\nedition='2024'\n[dependencies]\n{dep}='1'\n[patch.{origin}] # {app} source\n{key}={{package='{dep}',path='../../vendor/{dep}'}} # {app} selection\n"
        )
    };
    let index = "'https://github.com/rust-lang/crates.io-index'";
    for (app, dep, origin) in [("server", "p", "crates-io"), ("worker", "q", index)] {
        let dir = root.join("apps").join(app);
        put(
            &dir,
            "Cargo.toml",
            manifest(app, dep, origin, "chosen").as_bytes(),
        );
        put(&dir, "Cargo.lock", format!(
            "version=4\n[[package]]\nname='{app}'\nversion='1.0.0'\ndependencies=['{dep}']\n[[package]]\nname='{dep}'\nversion='1.0.0'\n"
        ).as_bytes());
        let vendor = root.join("vendor").join(dep);
        put(
            &vendor,
            "Cargo.toml",
            format!("[package]\nname='{dep}'\nversion='1.0.0'\nedition='2024'\n").as_bytes(),
        );
        put(&vendor, "src/lib.rs", b"");
    }
    let before = author_port_payload(&root);
    for preview in [true, false] {
        let error = update_project(
            &next,
            &root,
            &UpdateOptions {
                preview,
                ..offline()
            },
            &NoResolver,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        for expected in ["chosen", "apps/server/Cargo.toml", "apps/worker/Cargo.toml"] {
            assert!(message.contains(expected), "{message}");
        }
        assert_eq!(author_port_payload(&root), before);
        assert!(!root.join(PENDING_PATH).exists());
    }
    put(
        &root,
        "apps/worker/Cargo.toml",
        manifest("worker", "q", index, "q").as_bytes(),
    );
    let before = author_port_payload(&root);
    let preview = update_project(
        &next,
        &root,
        &UpdateOptions {
            preview: true,
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    assert_eq!(author_port_payload(&root), before);
    let applied = update_project(&next, &root, &offline(), &NoResolver).unwrap();
    assert!(applied.applied);
    for path in ["Cargo.toml", "Cargo.lock"] {
        let planned = preview.files.iter().find(|f| f.path == path).unwrap();
        let written = applied.files.iter().find(|f| f.path == path).unwrap();
        assert_eq!(planned.after, written.after);
    }

    let manifest = cargo_file(&root, "Cargo.toml");
    assert_eq!(manifest["patch"].as_table().unwrap().len(), 1);
    let patch = &manifest["patch"]["crates-io"];
    let text = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let lock = cargo_file(&root, "Cargo.lock");
    let packages = lock["package"].as_array().unwrap();
    assert_eq!(packages.len(), 4);
    for (app, dep, key) in [("server", "p", "chosen"), ("worker", "q", "q")] {
        assert_eq!(
            patch[key]["path"].as_str().unwrap(),
            format!("vendor/{dep}")
        );
        let owner = packages
            .iter()
            .find(|p| p["name"].as_str() == Some(app))
            .unwrap();
        assert_eq!(
            owner["dependencies"].as_array().unwrap()[0].as_str(),
            Some(dep)
        );
        assert_eq!(
            text.matches(&format!("# {app} selection")).count(),
            1,
            "{text}"
        );
    }
    assert_eq!(text.matches("# server source").count(), 1, "{text}");
    assert!(
        sync_project(&next, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

#[test]
fn migrate_nested_workspace_preserves_inheritance_and_excludes_external_helpers() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("nested");
    let initial = init(&old, &root, "rust-api", PackageMode::Registry);
    put(
        &root,
        "apps/api/Cargo.toml",
        br#"[workspace]
members = ["crates/server"]
resolver = "2"

[workspace.package]
version = "2.4.0"

[workspace.dependencies]
# Path follows the location of the owning workspace.
helper = { path = "../../vendor/helper" }
"#,
    );
    put(
        &root,
        "apps/api/crates/server/Cargo.toml",
        br#"[package]
name = "my-server"
version.workspace = true
edition = "2021"
workspace = "../.." # shared component settings
[dependencies]
helper.workspace = true
"#,
    );
    put(
        &root,
        "apps/api/crates/server/src/main.rs",
        b"fn main() { helper::answer(); }\n",
    );
    put(
        &root,
        "vendor/helper/Cargo.toml",
        b"[package]\nname='helper'\nversion='1.2.3'\nedition='2021'\n",
    );
    put(
        &root,
        "vendor/helper/src/lib.rs",
        b"pub fn answer() -> u32 { 42 }\n",
    );
    put(&root,"apps/api/Cargo.lock",b"version=3\n[[package]]\nname='my-server'\nversion='2.4.0'\ndependencies=['helper']\n[[package]]\nname='helper'\nversion='1.2.3'\n");
    devkit::native::admit_native_components(&root, &initial).unwrap();
    let source = preserved(
        &root,
        &[
            "apps/api/crates/server/src/main.rs",
            "vendor/helper/Cargo.toml",
            "vendor/helper/src/lib.rs",
        ],
    );
    let outcome = update_project(&next, &root, &offline(), &NoResolver).unwrap();
    assert!(outcome.applied);
    assert_eq!(
        source,
        preserved(
            &root,
            &[
                "apps/api/crates/server/src/main.rs",
                "vendor/helper/Cargo.toml",
                "vendor/helper/src/lib.rs"
            ]
        )
    );
    let workspace = cargo_file(&root, "Cargo.toml");
    let ws = &workspace["workspace"];
    assert_eq!(
        ws["members"].as_array().unwrap(),
        &[toml::Value::from("apps/api/crates/server")]
    );
    assert_eq!(
        ws["exclude"].as_array().unwrap(),
        &[toml::Value::from("vendor/helper")]
    );
    assert_eq!(ws["package"]["version"].as_str(), Some("2.4.0"));
    assert_eq!(ws["resolver"].as_str(), Some("2"));
    assert_eq!(
        ws["dependencies"]["helper"]["path"].as_str(),
        Some("vendor/helper")
    );
    assert!(!root.join("apps/api/Cargo.toml").exists());
    let member = fs::read_to_string(root.join("apps/api/crates/server/Cargo.toml")).unwrap();
    assert!(member.contains("# shared component settings"));
    assert!(
        fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .contains("# Path follows")
    );
    assert_eq!(
        cargo_file(&root, "apps/api/crates/server/Cargo.toml")["package"]["workspace"].as_str(),
        Some("../../../..")
    );
    assert_eq!(
        cargo_file(&root, "Cargo.lock")["version"].as_integer(),
        Some(4)
    );
    assert!(
        sync_project(&next, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

#[test]
fn adding_a_seed_with_an_older_compatible_registry_version_preserves_the_live_selection() {
    const SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
    fn registry_lock(names: &[&str], version: &str) -> String {
        let mut lock = String::from("version = 4\n");
        for name in names {
            lock.push_str(&format!(
                "[[package]]\nname='{name}'\nversion='1.0.0'\ndependencies=['external']\n"
            ));
        }
        lock.push_str(&format!(
            "[[package]]\nname='external'\nversion='{version}'\nsource='{SOURCE}'\nchecksum='{}'\n",
            "c".repeat(64)
        ));
        lock
    }
    struct Resolved<'a>(&'a str);
    impl CargoLockResolver for Resolved<'_> {
        fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(!offline);
            put(
                root,
                "Cargo.lock",
                registry_lock(&["app-api", "app-jobs"], self.0).as_bytes(),
            );
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let template = release.join("templates/rust-api");
    let manifest = fs::read_to_string(template.join("Cargo.toml")).unwrap();
    put(
        &template,
        "Cargo.toml",
        format!("{manifest}\n[dependencies]\nexternal='1.0.0'\n").as_bytes(),
    );
    put(
        &template,
        "Cargo.lock",
        registry_lock(&["fixture-api"], "1.0.0").as_bytes(),
    );
    seal(&release, "0.6.0");
    let root = temp.path().join("consumer");
    init_project(
        &release,
        &root,
        &InitOptions {
            template: Some("rust-api".into()),
            offline: true,
            cargo_resolver: Some(&NoCargoResolver),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    put(
        &root,
        "Cargo.lock",
        registry_lock(&["app-api"], "1.0.1").as_bytes(),
    );
    let before = author_port_payload(&root);
    let inventory = capture_compose_inputs(&root).unwrap();
    let add = |options: &UpdateOptions<'_>| {
        add_project_component(
            &release,
            &root,
            Role::App,
            ComponentSpec {
                name: "jobs".into(),
                template: "rust-api".into(),
                uses: vec![],
            },
            options,
            &NoResolver,
        )
    };
    assert!(
        add(&UpdateOptions {
            compose_inventory: Some(&inventory),
            ..offline()
        })
        .is_err()
    );
    assert_eq!(before, author_port_payload(&root));
    let reverted = add(&UpdateOptions {
        compose_inventory: Some(&inventory),
        cargo_resolver: Some(&Resolved("1.0.0")),
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        format!("{reverted:#}").contains("selection"),
        "{reverted:#}"
    );
    assert_eq!(before, author_port_payload(&root));
    assert!(
        add(&UpdateOptions {
            compose_inventory: Some(&inventory),
            cargo_resolver: Some(&Resolved("1.0.1")),
            ..Default::default()
        })
        .unwrap()
        .applied
    );
    assert!(
        sync_project(&release, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

fn set_cargo_members(root: &Path, members: &[&str]) {
    let mut workspace: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    workspace["workspace"]["members"] =
        toml_edit::value(members.iter().copied().collect::<toml_edit::Array>());
    put(root, "Cargo.toml", workspace.to_string().as_bytes());
}

#[test]
fn sync_restores_accepted_paths_and_never_substitutes_an_unlisted_fixture() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-api", PackageMode::Local);

    let mut workspace: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    workspace["workspace"]["package"]["version"] = toml_edit::value("2.3.4");
    put(&root, "Cargo.toml", workspace.to_string().as_bytes());
    let member = "packages/protocol/crates/wire";
    let sibling = "packages/protocol/crates/types";
    let manifest = "[package]\nname='wire'\nversion.workspace=true\nedition='2024'\n";
    put(&root.join(member), "Cargo.toml", manifest.as_bytes());
    put(&root.join(member), "src/lib.rs", b"pub fn wire() {}\n");
    put(
        &root.join(sibling),
        "Cargo.toml",
        manifest.replace("wire", "types").as_bytes(),
    );
    put(&root.join(sibling), "src/lib.rs", b"pub fn types() {}\n");
    register_project_component(
        &release,
        &root,
        Role::Library,
        "protocol".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap();
    let proof = read_project_state(&root).unwrap().cargo_resolution.unwrap();
    let lock = fs::read(root.join("Cargo.lock")).unwrap();

    // The fixture and even a new package at the group's root can have the
    // same locked identity as the real crate. Neither was ever registered.
    for candidate in ["packages/protocol", "packages/protocol/tests/fixtures/wire"] {
        put(&root.join(candidate), "Cargo.toml", manifest.as_bytes());
        put(
            &root.join(candidate),
            "src/lib.rs",
            b"pub fn fixture() {}\n",
        );
    }
    put(
        &root,
        "packages/protocol/tests/invalid/Cargo.toml",
        b"invalid fixture [[\n",
    );
    set_cargo_members(&root, &["apps/api"]);
    let sync = || sync_project(&release, &root, &offline(), &NoResolver);
    let restored = sync().unwrap();
    assert!(restored.applied);
    assert_eq!(
        restored
            .state
            .cargo_resolution
            .as_ref()
            .unwrap()
            .inputs
            .members,
        proof.inputs.members
    );
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock);
    assert!(sync().unwrap().no_op);

    let parked = temp.path().join("parked-wire");
    fs::rename(root.join(member), &parked).unwrap();
    set_cargo_members(&root, &["apps/api"]);
    let before = author_port_payload(&root);
    let error = format!("{:#}", sync().unwrap_err());
    assert!(
        error.contains(member)
            && error.contains("workspace.members")
            && error.contains("make sync"),
        "{error}"
    );
    assert_eq!(author_port_payload(&root), before);
    assert!(!root.join(PENDING_PATH).exists());

    // Explicitly move one crate and remove the other. The group remains
    // registered, and its new nonempty member list is the project's choice.
    let moved = "packages/protocol/crates/new-wire";
    fs::rename(parked, root.join(moved)).unwrap();
    set_cargo_members(&root, &["apps/api", moved]);
    let changed = sync().unwrap();
    let paths = changed
        .state
        .cargo_resolution
        .as_ref()
        .unwrap()
        .inputs
        .members
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(paths, ["apps/api", moved]);
    assert!(sync().unwrap().no_op);
}

#[test]
fn sync_cannot_substitute_a_direct_package_for_a_nested_member_in_an_older_checkout() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-api", PackageMode::Local);
    let member = "packages/protocol/crates/wire";
    put(
        &root.join(member),
        "Cargo.toml",
        b"[package]\nname='wire'\nversion='1.0.0'\nedition='2024'\n",
    );
    put(&root.join(member), "src/lib.rs", b"pub fn wire() {}\n");
    register_project_component(
        &release,
        &root,
        Role::Library,
        "protocol".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap();
    let mut state = read_project_state(&root).unwrap();
    state.cargo_resolution = None;
    write_json(&root.join(MANIFEST_PATH), &state).unwrap();
    let impostor = "packages/protocol";
    put(
        &root.join(impostor),
        "Cargo.toml",
        &fs::read(root.join(member).join("Cargo.toml")).unwrap(),
    );
    put(
        &root.join(impostor),
        "src/lib.rs",
        b"pub fn different_root() {}\n",
    );
    set_cargo_members(&root, &["apps/api"]);
    let before = author_port_payload(&root);
    let sync = || sync_project(&release, &root, &offline(), &NoResolver);
    let error = format!("{:#}", sync().unwrap_err());
    assert!(
        error.contains("packages/protocol") && error.contains("workspace.members"),
        "{error}"
    );
    assert_eq!(author_port_payload(&root), before);

    assert!(!root.join(PENDING_PATH).exists());
    set_cargo_members(&root, &["apps/api", member]);
    let repaired = sync().unwrap();
    let paths = repaired
        .state
        .cargo_resolution
        .unwrap()
        .inputs
        .members
        .into_keys()
        .collect::<Vec<_>>();
    assert_eq!(paths, ["apps/api", member]);
    assert!(sync().unwrap().no_op);

    let admitted =
        devkit::native::admit_native_components(&root, &read_project_state(&root).unwrap())
            .unwrap();
    let ancestor = format!("{impostor}/Cargo.toml");
    assert_eq!(
        admitted.rust_inputs.files[&ancestor],
        identity(&root.join(&ancestor)).unwrap()
    );
    let shadow = fs::read_to_string(root.join(&ancestor)).unwrap() + "[workspace]\n";
    put(&root, &ancestor, shadow.as_bytes());
    assert!(admitted.recheck(&root).is_err());
    let error = format!("{:#}", sync().unwrap_err());
    assert!(
        error.contains(&format!("Nested Cargo workspace at {ancestor}")),
        "{error}"
    );
}

#[test]
fn sync_requires_the_direct_member_path_in_an_older_checkout_with_lost_membership() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    let mut state = init(&release, &root, "rust-library", PackageMode::Local);
    let member = "packages/core";
    state.cargo_resolution = None;
    write_json(&root.join(MANIFEST_PATH), &state).unwrap();
    set_cargo_members(&root, &[]);
    let before = author_port_payload(&root);
    let sync = || sync_project(&release, &root, &offline(), &NoResolver);
    let error = format!("{:#}", sync().unwrap_err());
    assert!(
        error.contains(member)
            && error.contains("workspace.members")
            && error.contains("make sync"),
        "{error}"
    );
    assert_eq!(author_port_payload(&root), before);
    assert!(!root.join(PENDING_PATH).exists());

    set_cargo_members(&root, &[member]);
    let repaired = sync().unwrap();
    assert!(repaired.applied);
    assert_eq!(
        repaired
            .state
            .cargo_resolution
            .unwrap()
            .inputs
            .members
            .into_keys()
            .collect::<Vec<_>>(),
        [member]
    );
    assert!(sync().unwrap().no_op);
}

#[test]
fn missing_root_cargo_lock_is_rejected_on_add_and_register() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    fs::remove_file(root.join("Cargo.lock")).unwrap();
    let err = add_project_component(
        &release,
        &root,
        Role::App,
        ComponentSpec {
            name: "api".into(),
            template: "rust-api".into(),
            uses: vec![],
        },
        &offline(),
        &NoResolver,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("Missing root Cargo.lock"),
        "{err:#}"
    );
    assert!(!root.join("Cargo.lock").exists());
    assert!(!root.join("apps/api").exists());

    put(
        &root,
        "packages/extra/Cargo.toml",
        b"[package]\nname='extra'\nversion='0.1.0'\nedition='2024'\n",
    );
    put(&root, "packages/extra/src/lib.rs", b"pub fn e() {}\n");
    let reg_err = register_project_component(
        &release,
        &root,
        Role::Library,
        "extra".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap_err();
    assert!(
        format!("{reg_err:#}").contains("Missing root Cargo.lock"),
        "{reg_err:#}"
    );
    assert!(!root.join("Cargo.lock").exists());
}

#[test]
fn register_respects_listed_members_skips_fixtures_and_excludes_deep_target() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    let state = init(&release, &root, "rust-library", PackageMode::Local);

    // 1. snapshot_excluded_at handles deep nested target dirs and vendor target dirs,
    //    while preserving source directories named `target`.
    put(
        &root,
        "apps/api/crates/server/Cargo.toml",
        b"[package]\nname='server'\nversion='0.1.0'\nedition='2024'\n",
    );
    put(
        &root,
        "vendor/helper/Cargo.toml",
        b"[package]\nname='helper'\nversion='0.1.0'\n",
    );
    assert!(snapshot_excluded_at(
        &root,
        &state,
        "apps/api/crates/server/target/debug/target-info"
    ));
    assert!(snapshot_excluded_at(
        &root,
        &state,
        "vendor/helper/target/debug/lib.rlib"
    ));
    assert!(!snapshot_excluded_at(&root, &state, "apps/api/src/target"));
    fs::remove_dir_all(root.join("apps/api")).unwrap();
    fs::remove_dir_all(root.join("vendor")).unwrap();

    // 2. Multi-crate discovery skips tests/fixtures and does not descend inside a found crate.
    put(
        &root,
        "packages/multi/crates/alpha/Cargo.toml",
        b"[package]\nname='alpha'\nversion='0.1.0'\nedition='2024'\n",
    );
    put(
        &root,
        "packages/multi/crates/alpha/src/lib.rs",
        b"pub fn a() {}\n",
    );
    put(
        &root,
        "packages/multi/crates/alpha/tests/fixture/Cargo.toml",
        b"[package]\nname='fixture-a'\nversion='0.1.0'\nedition='2024'\n",
    );
    put(
        &root,
        "packages/multi/tests/fixture/Cargo.toml",
        b"[package]\nname='fixture-b'\nversion='0.1.0'\nedition='2024'\n",
    );
    put(
        &root,
        "packages/multi/target/debug/junk.bin",
        b"legacy-build-output",
    );
    let registered = register_project_component(
        &release,
        &root,
        Role::Library,
        "multi".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap();
    assert!(registered.applied);
    let members: Vec<_> = cargo_file(&root, "Cargo.toml")["workspace"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        members,
        vec!["packages/core", "packages/multi/crates/alpha"]
    );

    // 3. Explicit pre-listed workspace.members is preserved without auto-discovering unlisted siblings.
    put(
        &root,
        "packages/explicit/crates/chosen/Cargo.toml",
        b"[package]\nname='chosen'\nversion='0.1.0'\nedition='2024'\n",
    );
    put(
        &root,
        "packages/explicit/crates/chosen/src/lib.rs",
        b"pub fn c() {}\n",
    );
    put(
        &root,
        "packages/explicit/crates/unlisted/Cargo.toml",
        b"[package]\nname='unlisted'\nversion='0.1.0'\nedition='2024'\n",
    );
    set_cargo_members(
        &root,
        &[
            "packages/core",
            "packages/multi/crates/alpha",
            "packages/explicit/crates/chosen",
        ],
    );
    let registered_explicit = register_project_component(
        &release,
        &root,
        Role::Library,
        "explicit".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap();
    assert!(registered_explicit.applied);
    let members: Vec<_> = cargo_file(&root, "Cargo.toml")["workspace"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        members,
        vec![
            "packages/core",
            "packages/multi/crates/alpha",
            "packages/explicit/crates/chosen",
        ]
    );
}

#[test]
fn clean_sync_noop_skips_staging_and_state_validation_checks_cargo_resolution() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    let mut state = init(&release, &root, "rust-library", PackageMode::Local);

    // Clean sync returns no_op without creating any staging directory.
    let state_dir = root.join(".devkit/state");
    let snapshot_dir = |dir: &Path| -> std::collections::BTreeSet<_> {
        if dir.is_dir() {
            fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect()
        } else {
            std::collections::BTreeSet::new()
        }
    };
    let outcome = sync_project(&release, &root, &offline(), &NoResolver).unwrap();
    assert!(outcome.no_op && !outcome.applied);
    let after_first = snapshot_dir(&state_dir);
    assert!(
        after_first
            .iter()
            .all(|name| name.to_string_lossy() == "task.lock"),
        "unexpected staging entries in .devkit/state: {after_first:?}"
    );
    let outcome2 = sync_project(&release, &root, &offline(), &NoResolver).unwrap();
    assert!(outcome2.no_op && !outcome2.applied);
    assert_eq!(snapshot_dir(&state_dir), after_first);

    // Invalid edition in a featureless member is not accepted by local fast-path.
    put(
        &root,
        "packages/core/Cargo.toml",
        b"[package]\nname='lib-core'\nversion='1.0.0'\nedition='2099'\n",
    );
    assert!(sync_project(&release, &root, &offline(), &NoResolver).is_err());

    // validate_project_state validates cargo_resolution digest/version and preserves unknown extra fields.
    let mut valid_with_extra = state.clone();
    valid_with_extra
        .cargo_resolution
        .as_mut()
        .unwrap()
        .extra
        .insert("futureField".into(), serde_json::json!(true));
    validate_project_state(&valid_with_extra).unwrap();
    let roundtrip: ProjectState =
        serde_json::from_str(&serde_json::to_string(&valid_with_extra).unwrap()).unwrap();
    assert_eq!(
        roundtrip
            .cargo_resolution
            .as_ref()
            .unwrap()
            .extra
            .get("futureField"),
        Some(&serde_json::json!(true))
    );

    state.cargo_resolution.as_mut().unwrap().lock = "not-a-sha256".into();
    assert!(validate_project_state(&state).is_err());
}

#[test]
fn root_profile_changes_are_not_stripped_and_cross_version_update_checks_proof() {
    use std::cell::Cell;
    struct CountingResolver {
        updates: Cell<usize>,
        validates: Cell<usize>,
    }
    impl CargoLockResolver for CountingResolver {
        fn resolve_cargo(&self, _: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
            self.updates.set(self.updates.get() + 1);
            Ok(())
        }
        fn validate_locked(&self, _: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
            self.validates.set(self.validates.get() + 1);
            Ok(())
        }
    }

    let temp = TempDir::new().unwrap();
    let release_060 = fixture_release(temp.path(), "0.6.0");
    let release_061 = fixture_release(temp.path(), "0.6.1");
    let root = temp.path().join("consumer");
    init(&release_060, &root, "rust-library", PackageMode::Local);

    // 1. Cross-version update 0.6.0 -> 0.6.1 with unchanged proof skips Cargo resolver completely.
    let counter = CountingResolver {
        updates: Cell::new(0),
        validates: Cell::new(0),
    };
    let updated = update_project(
        &release_061,
        &root,
        &UpdateOptions {
            cargo_resolver: Some(&counter),
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    assert!(updated.applied);
    assert_eq!(counter.updates.get(), 0);
    assert_eq!(counter.validates.get(), 0);

    // 2. Editing root [profile.release] is never stripped from inputs.policy (must NOT be a no-op).
    let base_manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    put(
        &root,
        "Cargo.toml",
        format!("{base_manifest}\n[profile.release]\nopt-level = 1\n").as_bytes(),
    );
    let synced_profile = sync_project(
        &release_061,
        &root,
        &UpdateOptions {
            cargo_resolver: Some(&counter),
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    assert!(synced_profile.applied);
    assert_eq!(counter.validates.get(), 1);
    assert_eq!(counter.updates.get(), 0);

    // Changing only [profile.release] value (without touching members/exclude) must perturb policy.
    put(
        &root,
        "Cargo.toml",
        format!("{base_manifest}\n[profile.release]\nopt-level = 2\n").as_bytes(),
    );
    let synced_profile2 = sync_project(
        &release_061,
        &root,
        &UpdateOptions {
            cargo_resolver: Some(&counter),
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    assert!(synced_profile2.applied);
    assert_eq!(counter.validates.get(), 2);
    assert_eq!(counter.updates.get(), 0);
}

#[test]
fn docker_cargo_resolver_enforces_ro_cache_masks_ca_and_cgroup_retry() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    put(
        &release,
        "runtime/rust.Dockerfile",
        format!("FROM rust:1.99.1-slim-bookworm@sha256:{}\n", "a".repeat(64)).as_bytes(),
    );
    seal(&release, "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    put(&root, ".env", b"SECRET=1\n");
    put(&root, "packages/core/.env", b"COMPONENT_SECRET=1\n");
    put(
        &root,
        ".npmrc",
        b"//registry.npmjs.org/:_authToken=secret\n",
    );
    let ca = temp.path().join("custom-ca.pem");
    fs::write(
        &ca,
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let cache = temp.path().join("cache");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let docker_log = temp.path().join("docker.log");
    let fail_once = temp.path().join("fail-once");
    fs::write(&fail_once, "1").unwrap();
    let docker_script = fake_bin.join("docker");
    let script = format!(
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "info" ]; then
  printf '{{"OSType":"linux","Architecture":"{arch}","NCPU":4,"MemTotal":4294967296,"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUShares":true,"PidsLimit":true}}\n'
  exit 0
fi
if [ "$1" = "context" ]; then
  case "$2" in
    show) printf 'default\n' ;;
    inspect) printf 'default\ntcp://127.0.0.1:2375\n' ;;
    *) exit 90 ;;
  esac
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
  exit 0
fi
if [ "$1" = "run" ]; then
  if [ -f "{fail_once}" ]; then
    rm -f "{fail_once}"
    printf 'Error response from daemon: cannot enter cgroupv2 "/sys/fs/cgroup/docker" with domain controllers\n' >&2
    exit 125
  fi
  case "$*" in
    *"metadata --locked --all-features --format-version=1"*)
      printf '{{"packages":[{{"name":"lib-core","version":"1.0.0","source":null,"manifest_path":"/workspace/packages/core/Cargo.toml"}}]}}\n'
      exit 0
      ;;
  esac
  exit 0
fi
exit 0
"#,
        log = docker_log.display(),
        fail_once = fail_once.display(),
        arch = std::env::consts::ARCH,
    );
    fs::write(&docker_script, script).unwrap();
    fs::set_permissions(&docker_script, fs::Permissions::from_mode(0o755)).unwrap();
    let path_env = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cargo_workspace::docker_cargo_resolver_worker",
            "--ignored",
        ])
        .env("PATH", &path_env)
        .env("DEVKIT_CACHE_DIR", &cache)
        .env("DEVKIT_CA_CERT", &ca)
        .env("DEVKIT_TEST_RELEASE", &release)
        .env("DEVKIT_TEST_TARGET", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "worker failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&docker_log).unwrap();
    let runs: Vec<&str> = log.lines().filter(|l| l.starts_with("run ")).collect();
    // 1. Update attempt 1 failed with cgroupv2 error -> attempt 2 retried without --cpus/--memory.
    // 2. Validate offline mounted /cache/cargo RO without CA cert.
    // 3. Paths offline mounted /cache/cargo RO without CA cert.
    // 4. Paths online mounted /cache/cargo RW with CA cert.
    assert_eq!(runs.len(), 5, "expected 5 docker run lines:\n{log}");
    assert!(runs[0].contains("--cpus") || runs[0].contains("--memory"));
    assert!(!runs[1].contains("--cpus") && !runs[1].contains("--memory"));
    assert!(runs[1].contains("--tmpfs /tmp:rw,noexec,nosuid,nodev,mode=1777,size="));
    assert!(!runs[1].contains("exec,mode=1777"));
    assert!(runs[1].contains("type=bind,\"source=/dev/null\",\"target=/workspace/.env\",readonly"));
    assert!(runs[1].contains(
        "type=bind,\"source=/dev/null\",\"target=/workspace/packages/core/.env\",readonly"
    ));
    assert!(
        runs[1].contains("type=bind,\"source=/dev/null\",\"target=/workspace/.npmrc\",readonly")
    );
    assert!(runs[1].contains("RUSTC=/usr/local/cargo/bin/rustc"));
    assert!(runs[1].contains("CARGO_NET_GIT_FETCH_WITH_CLI=false"));
    assert!(runs[1].contains("RUSTFLAGS="));
    assert!(runs[1].contains("CARGO_HTTP_CAINFO=/devkit/ca.pem"));
    assert!(runs[1].contains("\"target=/cache/cargo\" "));
    assert!(!runs[1].contains("\"target=/cache/cargo\",readonly"));
    assert!(runs[2].contains("\"target=/cache/cargo\",readonly"));
    assert!(!runs[2].contains("CARGO_HTTP_CAINFO"));
    assert!(runs[3].contains("\"target=/cache/cargo\",readonly"));
    assert!(!runs[3].contains("CARGO_HTTP_CAINFO"));
    assert!(runs[4].contains("\"target=/cache/cargo\" "));
    assert!(!runs[4].contains("\"target=/cache/cargo\",readonly"));
    assert!(runs[4].contains("CARGO_HTTP_CAINFO=/devkit/ca.pem"));
}

#[test]
#[ignore = "worker invoked by docker_cargo_resolver_enforces_ro_cache_masks_ca_and_cgroup_retry"]
fn docker_cargo_resolver_worker() {
    use devkit::cargo_lock::{CargoLockResolver, DockerCargoLockResolver};
    let release_dir = PathBuf::from(std::env::var("DEVKIT_TEST_RELEASE").unwrap());
    let root = PathBuf::from(std::env::var("DEVKIT_TEST_TARGET").unwrap());
    let cache = PathBuf::from(std::env::var("DEVKIT_CACHE_DIR").unwrap());
    let release = admit_release(&release_dir).unwrap();
    let resolver = DockerCargoLockResolver::default();
    // Online update (triggers reactive cgroupv2 retry on attempt 1 -> succeeds on attempt 2).
    resolver.resolve_cargo(&root, &release, false).unwrap();
    // Offline validate (RO /cache/cargo, no CA cert).
    resolver.validate_locked(&root, &release, true).unwrap();
    // Paths offline (RO /cache/cargo, no CA cert, parses metadata manifest_path -> packages/core).
    let selected = resolver
        .selected_local_paths(&root, &release, true)
        .unwrap();
    assert!(selected.contains("packages/core"), "{selected:?}");
    // Paths online (RW /cache/cargo + CA cert so cargo metadata can fetch newly locked crates).
    let selected_online = resolver
        .selected_local_paths(&root, &release, false)
        .unwrap();
    assert!(
        selected_online.contains("packages/core"),
        "{selected_online:?}"
    );
    // Symlinked sensitive file is refused fail-closed before launching Docker.
    let sym = root.join(".env.local");
    std::os::unix::fs::symlink(root.join("packages/core/Cargo.toml"), &sym).unwrap();
    let sym_err = resolver
        .validate_locked(&root, &release, true)
        .unwrap_err()
        .to_string();
    assert!(
        sym_err.contains("Refusing symlinked sensitive file"),
        "{sym_err}"
    );
    fs::remove_file(&sym).unwrap();
    // Poisoned CARGO_HOME config.toml is rejected before launching Docker.
    let poison = cache.join("cargo/config.toml");
    fs::write(&poison, "[env]\nEVIL='1'\n").unwrap();
    let err = resolver
        .validate_locked(&root, &release, true)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("Refusing unexpected Cargo configuration in shared cache"),
        "{err}"
    );
    fs::remove_file(&poison).unwrap();
}

#[path = "cargo_reconcile.rs"]
mod cargo_reconcile;

#[path = "cargo_validation.rs"]
mod cargo_validation;
