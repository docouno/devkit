use super::*;
use std::{cell::RefCell, collections::BTreeSet};

struct MigrationValidator {
    check: fn(&Path) -> Result<()>,
    seen: RefCell<Vec<Vec<u8>>>,
}

impl MigrationValidator {
    fn new(check: fn(&Path) -> Result<()>) -> Self {
        Self {
            check,
            seen: RefCell::default(),
        }
    }
}

impl CargoLockResolver for MigrationValidator {
    fn resolve_cargo(&self, _: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
        panic!("migration may validate the merged lock but may never resolve new versions");
    }

    fn selected_local_paths(
        &self,
        root: &Path,
        _: &AdmittedRelease,
        offline: bool,
    ) -> Result<BTreeSet<String>> {
        assert!(offline);
        paths(root)
    }

    fn validate_locked(
        &self,
        stage: &Path,
        release: &AdmittedRelease,
        offline: bool,
    ) -> Result<()> {
        assert!(offline);
        assert_eq!(release.manifest.version, "0.6.0");
        assert!(!stage.join("apps/api/Cargo.lock").exists());
        self.seen
            .borrow_mut()
            .push(fs::read(stage.join("Cargo.lock"))?);
        (self.check)(stage)
    }
}

fn update(
    next: &Path,
    root: &Path,
    preview: bool,
    validator: &dyn CargoLockResolver,
) -> Result<MutationOutcome> {
    update_project(
        next,
        root,
        &UpdateOptions {
            preview,
            cargo_resolver: Some(validator),
            ..offline()
        },
        &NoResolver,
    )
}

fn cargo(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        bail!("{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout)
}

fn fetch_locked(root: &Path) -> Result<()> {
    cargo(root, &["fetch", "--locked", "--offline"]).map(drop)
}

fn paths(root: &Path) -> Result<BTreeSet<String>> {
    let metadata: Value = serde_json::from_slice(&cargo(
        root,
        &[
            "metadata",
            "--locked",
            "--offline",
            "--all-features",
            "--format-version=1",
        ],
    )?)?;
    Ok(metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["source"].is_null())
        .map(|p| {
            Path::new(p["manifest_path"].as_str().unwrap())
                .parent()
                .unwrap()
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect())
}

fn selected(root: &Path) -> Result<BTreeMap<String, Value>> {
    let metadata: Value = serde_json::from_slice(&cargo(
        root,
        &["metadata", "--format-version=1", "--locked", "--offline"],
    )?)?;
    Ok(metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            (
                node["id"].as_str().unwrap().to_owned(),
                node["dependencies"].clone(),
            )
        })
        .collect())
}

#[test]
fn migration_validates_custom_registry_inputs_and_preserves_the_exact_checked_lock() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let release_lock = format!(
        "version=4\n[[package]]\nname='fixture-api'\nversion='1.0.0'\ndependencies=['widget']\n\
         [[package]]\nname='widget'\nversion='1.2.3'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\ndependencies=['leaf']\n\
         [[package]]\nname='leaf'\nversion='2.1.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\n",
        "a".repeat(64),
        "b".repeat(64)
    );
    for (release, version, requirement) in [(&old, "0.5.1", "=1.2.3"), (&next, "0.6.0", "1.2.3")] {
        let template = release.join("templates/rust-api");
        put(&template, "Cargo.toml", format!(
            "[package]\nname='fixture-api'\nversion='1.0.0'\nedition='2024'\n[dependencies]\nwidget={{version='{requirement}',features=['encode']}}\n"
        ).as_bytes());
        put(&template, "Cargo.lock", release_lock.as_bytes());
        seal(release, version);
    }

    // The accepted legacy template and its complete registry graph are known.
    let proven = temp.path().join("from-template");
    init(&old, &proven, "rust-api", PackageMode::Local);
    let never = MigrationValidator::new(|_| panic!("verified release seed must be Docker-free"));
    let before = author_port_payload(&proven);
    assert!(
        update(&next, &proven, true, &never)
            .unwrap()
            .state
            .cargo_resolution
            .is_some()
    );
    assert_eq!(author_port_payload(&proven), before);
    assert!(update(&next, &proven, false, &never).unwrap().applied);
    assert!(never.seen.borrow().is_empty());

    // A custom manifest may request another feature from exactly the same
    // registry version. Cargo alone can confirm whether its old graph suffices.
    let root = temp.path().join("custom");
    init(&old, &root, "rust-api", PackageMode::Local);
    let app = root.join("apps/api");
    let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
    put(
        &app,
        "Cargo.toml",
        manifest.replace("'encode'", "'decode'").as_bytes(),
    );
    let before = author_port_payload(&root);
    let refused = MigrationValidator::new(|_| bail!("locked validation failed"));
    let changed = MigrationValidator::new(|stage| {
        // Same selections and checksum fields. Even a comment changes the bytes
        // that the validator was asked to attest.
        let path = stage.join("Cargo.lock");
        let mut bytes = fs::read(&path)?;
        bytes.extend_from_slice(b"\n# rewritten\n");
        fs::write(path, bytes)?;
        Ok(())
    });
    for (validator, expected) in [
        (&refused, "locked validation failed"),
        (&changed, "changed the staged Cargo.lock"),
    ] {
        for preview in [true, false] {
            let error = update(&next, &root, preview, validator).unwrap_err();
            let detail = format!("{error:#}");
            assert!(detail.contains(expected), "{detail}");
            assert_eq!(author_port_payload(&root), before);
            assert!(!root.join(PENDING_PATH).exists());
        }
        assert_eq!(validator.seen.borrow().len(), 2);
    }

    let accepted = MigrationValidator::new(|_| Ok(()));
    let preview = update(&next, &root, true, &accepted).unwrap();
    assert!(!preview.applied);
    assert_eq!(author_port_payload(&root), before);
    let applied = update(&next, &root, false, &accepted).unwrap();
    assert!(applied.applied);
    let published = fs::read(root.join("Cargo.lock")).unwrap();
    assert_eq!(
        &*accepted.seen.borrow(),
        &[published.clone(), published.clone()]
    );
    let proof = applied.state.cargo_resolution.as_ref().unwrap();
    assert_eq!(proof.lock, digest(&published));
    assert_eq!(preview.state.cargo_resolution.as_ref(), Some(proof));
    assert_eq!(
        read_project_state(&root).unwrap().cargo_resolution.as_ref(),
        Some(proof)
    );

    // Package ordering and whitespace may change in the v4 serializer. Every
    // package ID, checksum, and selected dependency edge must survive.
    let records = |value: toml::Value| -> BTreeMap<String, toml::Value> {
        value["package"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| (row["name"].as_str().unwrap().to_owned(), row.clone()))
            .collect()
    };
    assert_eq!(
        records(toml::from_str(&release_lock).unwrap()),
        records(cargo_file(&root, "Cargo.lock"))
    );
    let synced = sync_project(&next, &root, &offline(), &NoResolver).unwrap();
    assert!(synced.no_op);
    assert_eq!(synced.state, applied.state);
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_rejects_a_stale_target_feature_lock_during_migration() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("product");
    init(&old, &root, "rust-api", PackageMode::Local);
    let app = root.join("apps/api");
    let original = fs::read_to_string(app.join("Cargo.toml")).unwrap();
    put(
        &app,
        "Cargo.toml",
        format!("{original}\n[dependencies]\nhelper={{path='../../vendor/helper'}}\n").as_bytes(),
    );
    for (name, deps) in [
        (
            "helper",
            "[target.'cfg(windows)'.dependencies]\nleaf={path='../leaf',optional=true}\n[features]\nextra=['dep:leaf']\n",
        ),
        ("leaf", ""),
    ] {
        let package = root.join("vendor").join(name);
        put(
            &package,
            "Cargo.toml",
            format!("[package]\nname='{name}'\nversion='1.0.0'\nedition='2024'\n{deps}").as_bytes(),
        );
        put(&package, "src/lib.rs", b"pub fn answer() {}\n");
    }
    cargo(&app, &["generate-lockfile", "--offline"]).unwrap();
    let stale = fs::read_to_string(app.join("Cargo.lock")).unwrap();
    assert!(!stale.contains("leaf"), "{stale}");
    let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
    put(
        &app,
        "Cargo.toml",
        manifest
            .replace(
                "path='../../vendor/helper'",
                "path='../../vendor/helper',features=['extra']",
            )
            .as_bytes(),
    );
    let before = author_port_payload(&root);
    let checked = MigrationValidator::new(fetch_locked);
    for preview in [true, false] {
        let err = update(&next, &root, preview, &checked).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("locked") && message.contains("lock file"),
            "{message}"
        );
        assert_eq!(author_port_payload(&root), before);
        assert!(!root.join(PENDING_PATH).exists());
    }
    cargo(&app, &["generate-lockfile", "--offline"]).unwrap();
    assert!(
        fs::read_to_string(app.join("Cargo.lock"))
            .unwrap()
            .contains("leaf")
    );
    let before = author_port_payload(&root);
    assert!(
        update(&next, &root, true, &checked)
            .unwrap()
            .state
            .cargo_resolution
            .is_some()
    );
    assert_eq!(author_port_payload(&root), before);
    let applied = update(&next, &root, false, &checked).unwrap();
    assert!(applied.applied);
    let bytes = fs::read(root.join("Cargo.lock")).unwrap();
    assert_eq!(checked.seen.borrow().len(), 4);
    assert_eq!(checked.seen.borrow()[2..], [bytes.clone(), bytes]);
    assert!(
        sync_project(&next, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_migrates_patches_only_when_selected_edges_are_preserved() -> Result<()> {
    let temp = TempDir::new()?;
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init_legacy_rust_pair(&old, &root);
    // A private Cargo source outside the checkout supplies the test registry offline.
    put(
        temp.path(),
        ".cargo/config.toml",
        b"[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory='registry'\n",
    );
    let remote = temp.path().join("registry/widget-1.0.0");
    let patch = root.join("vendor/widget");
    let package =
        |name, version| format!("[package]\nname='{name}'\nversion='{version}'\nedition='2024'\n");
    put(&remote, "Cargo.toml", package("widget", "1.0.0").as_bytes());
    put(&remote, "src/lib.rs", b"");
    put_json(
        &remote,
        ".cargo-checksum.json",
        &json!({"files": {}, "package": "a".repeat(64)}),
    );
    put(&patch, "src/lib.rs", b"");
    let server = root.join("apps/server");
    let worker = root.join("apps/worker");
    put(
        &worker,
        "Cargo.toml",
        format!(
            "{}[dependencies]\nwidget='=1.0.0'\n",
            package("worker", "0.1.0")
        )
        .as_bytes(),
    );
    cargo(&worker, &["generate-lockfile", "--offline"])?;
    let checked = MigrationValidator::new(fetch_locked);
    for version in ["1.0.0", "2.0.0"] {
        put(&patch, "Cargo.toml", package("widget", version).as_bytes());
        put(&server, "Cargo.toml", format!("{}[dependencies]\nwidget='={version}'\n[patch.crates-io]\nwidget={{path='../../vendor/widget'}}\n", package("server", "0.1.0")).as_bytes());
        cargo(&server, &["generate-lockfile", "--offline"])?;
        let mut expected = selected(&server)?;
        expected.extend(selected(&worker)?);
        let before = author_port_payload(&root);
        if version == "1.0.0" {
            // The worker's registry ID at this exact version would become local.
            for preview in [true, false] {
                let error = update(&next, &root, preview, &checked).unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("apps/server/Cargo.toml")
                        && message.contains("apps/worker/Cargo.toml"),
                    "{message}"
                );
                assert_eq!(author_port_payload(&root), before);
                assert!(!root.join(PENDING_PATH).exists());
            }
        } else {
            let preview = update(&next, &root, true, &checked)?;
            assert_eq!(author_port_payload(&root), before);
            let applied = update(&next, &root, false, &checked)?;
            assert!(applied.applied);
            assert_eq!(
                preview.state.cargo_resolution,
                applied.state.cargo_resolution
            );
            assert_eq!(selected(&root)?, expected);
            assert_eq!(checked.seen.borrow().len(), 2);
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_overrides_share_identity_across_sources() -> Result<()> {
    struct Resolver;
    impl CargoLockResolver for Resolver {
        fn selected_local_paths(
            &self,
            root: &Path,
            _: &AdmittedRelease,
            offline: bool,
        ) -> Result<BTreeSet<String>> {
            assert!(offline);
            paths(root)
        }
        fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(offline);
            cargo(root, &["update", "--workspace", "--offline"]).map(drop)
        }
        fn validate_locked(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(offline);
            fetch_locked(root)
        }
    }

    let temp = TempDir::new()?;
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    copy_tree(
        &next.join("templates/rust-library"),
        &old.join("templates/rust-library"),
        &|_| false,
        false,
    )?;
    for name in ["rustfmt.toml", "rust-toolchain.toml"] {
        write_atomic(
            &old.join("templates/rust-library").join(name),
            &fs::read(old.join("templates/rust-api").join(name))?,
            0o644,
        )?;
    }
    seal(&old, "0.5.1");
    let root = temp.path().join("consumer");
    init_project(
        &old,
        &root,
        &InitOptions {
            packages: Some(
                ["client", "provider", "other"]
                    .into_iter()
                    .map(|p| ComponentSpec {
                        name: p.into(),
                        template: "rust-library".into(),
                        uses: vec![],
                    })
                    .collect(),
            ),
            ..Default::default()
        },
        &NoResolver,
    )?;
    let package = |path: &str, name: &str, extra: &str| {
        put(
            &root,
            &format!("{path}/Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='1.0.0'\nedition='2024'\n{extra}")
                .as_bytes(),
        );
        put(&root, &format!("{path}/src/lib.rs"), b"");
    };
    package(
        "packages/client",
        "client",
        "[dependencies]\nleaf='1'\n[patch.crates-io]\nleaf={path='../../vendor/one'}\n",
    );
    package("packages/provider", "provider", "");
    package(
        "packages/other",
        "other",
        "[patch.'https://example.org/other']\nleaf={path='../../vendor/two'}\n",
    );
    for which in ["one", "two"] {
        package(
            &format!("vendor/{which}"),
            "leaf",
            &format!("[dependencies]\ninner-{which}={{path='../inner-{which}'}}\n"),
        );
        package(
            &format!("vendor/inner-{which}"),
            &format!("inner-{which}"),
            "[dependencies]\nprovider={path='../../packages/provider'}\n",
        );
    }
    for component in ["packages/client", "packages/provider", "packages/other"] {
        cargo(&root.join(component), &["generate-lockfile", "--offline"])?;
    }
    sync_project(&old, &root, &offline(), &NoResolver)?;
    let migrated = update(&next, &root, false, &Resolver)?;
    assert_eq!(
        migrated.state.components["packages/client"].dependencies,
        ["packages/provider"]
    );
    assert!(sync_project(&next, &root, &offline(), &NoResolver)?.no_op);
    let manifest = fs::read_to_string(root.join("Cargo.toml"))?;
    for (selected, unused) in [("one", "two"), ("two", "one")] {
        if selected == "two" {
            let swapped = manifest
                .replace("vendor/one", "vendor/TEMP")
                .replace("vendor/two", "vendor/one")
                .replace("vendor/TEMP", "vendor/two");
            put(&root, "Cargo.toml", swapped.as_bytes());
        }
        let active_inner = format!("vendor/inner-{selected}");
        let unused_inner = format!("vendor/inner-{unused}/Cargo.toml");
        let saved = fs::read(root.join(&unused_inner))?;
        put(&root, &unused_inner, b"invalid dormant manifest");
        cargo(&root, &["update", "--workspace", "--offline"])?;
        let expected = fs::read(root.join("Cargo.lock"))?;
        let outcome = sync_project(
            &next,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&Resolver),
                ..offline()
            },
            &NoResolver,
        )?;
        let selected_path = format!("vendor/{selected}");
        assert_eq!(
            outcome
                .state
                .cargo_resolution
                .unwrap()
                .inputs
                .local_selections,
            Some(BTreeSet::from([selected_path.clone()]))
        );
        assert_eq!(
            outcome.state.components["packages/client"].dependencies,
            ["packages/provider"]
        );
        assert_eq!(
            paths(&root)?,
            BTreeSet::from([
                "packages/client".into(),
                "packages/provider".into(),
                "packages/other".into(),
                selected_path,
                active_inner
            ])
        );
        assert_eq!(fs::read(root.join("Cargo.lock"))?, expected);
        assert_eq!(
            cargo_file(&root, "Cargo.lock")["patch"]["unused"][0]["name"].as_str(),
            Some("leaf")
        );
        assert!(sync_project(&next, &root, &offline(), &NoResolver)?.no_op);
        put(&root, &unused_inner, &saved);
    }
    Ok(())
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_migrates_unused_patch_by_selected_directory() -> Result<()> {
    let temp = TempDir::new()?;
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let validator = MigrationValidator::new(fetch_locked);
    for (case, direct, use_patch) in [
        ("same-directory", "patch", false),
        ("same-override", "patch", false),
        ("different-directory", "direct", false),
        ("conflicting-selections", "direct", true),
    ] {
        let root = temp.path().join(case);
        init_legacy_rust_pair(&old, &root);
        let server = root.join("apps/server");
        let worker = root.join("apps/worker");
        for path in ["patch", direct] {
            let vendor = root.join("vendor").join(path);
            put(
                &vendor,
                "Cargo.toml",
                b"[package]\nname='common'\nversion='1.0.0'\nedition='2024'\n",
            );
            put(&vendor, "src/lib.rs", b"");
        }
        let dep = if case == "same-override" {
            put(
                &root,
                "vendor/leaf/Cargo.toml",
                b"[package]\nname='leaf'\nversion='1.0.0'\nedition='2024'\n",
            );
            put(&root, "vendor/leaf/src/lib.rs", b"");
            let patch = root.join("vendor/patch");
            let manifest = fs::read_to_string(patch.join("Cargo.toml"))?;
            put(
                &patch,
                "Cargo.toml",
                format!("{manifest}[dependencies]\nleaf={{path='../leaf'}}\n").as_bytes(),
            );
            "common='1'\n[patch.crates-io]\n".to_string()
        } else {
            String::new()
        };
        let dependency = if use_patch {
            "[dependencies]\ncommon='1'\n"
        } else {
            ""
        };
        put(&server, "Cargo.toml", format!(
            "[package]\nname='server'\nversion='0.1.0'\nedition='2024'\n{dependency}[patch.crates-io]\ncommon={{path='../../vendor/patch'}}\n"
        ).as_bytes());
        put(&worker, "Cargo.toml", format!(
            "[package]\nname='worker'\nversion='0.1.0'\nedition='2024'\n[dependencies]\n{dep}common={{path='../../vendor/{direct}'}}\n"
        ).as_bytes());
        for app in [&server, &worker] {
            cargo(app, &["generate-lockfile", "--offline"])?;
        }
        assert_eq!(
            cargo_file(&server, "Cargo.lock").get("patch").is_some(),
            !use_patch
        );
        let mut original = selected(&server)?;
        original.extend(selected(&worker)?);
        let before = author_port_payload(&root);
        if use_patch {
            // Each component selected a different directory for the same lock ID.
            for preview in [true, false] {
                let error = update(&next, &root, preview, &validator).unwrap_err();
                let details = format!("{error:#}");
                assert!(
                    details.contains("different directories for common 1.0.0")
                        && details.contains("vendor/patch/Cargo.toml")
                        && details.contains("vendor/direct/Cargo.toml"),
                    "{details}"
                );
                assert_eq!(author_port_payload(&root), before);
                assert!(!root.join(PENDING_PATH).exists());
            }
        } else {
            let planned = update(&next, &root, true, &validator)?;
            assert_eq!(author_port_payload(&root), before);
            let applied = update(&next, &root, false, &validator)?;
            assert!(applied.applied);
            assert_eq!(
                planned.state.cargo_resolution,
                applied.state.cargo_resolution
            );
            assert_eq!(
                cargo_file(&root, "Cargo.lock").get("patch").is_some(),
                direct != "patch"
            );
            fetch_locked(&root)?;
            assert_eq!(selected(&root)?, original);
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_migrates_patch_origins_with_the_original_local_sources() -> Result<()> {
    let temp = TempDir::new()?;
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let validator = MigrationValidator::new(fetch_locked);
    let main = ("crates-io", "one");
    let alpha = ("'https://alpha.example.test/index'", "two");
    let beta = "'https://beta.example.test/index'";
    let names = ["client", "server", "helper", "other"];
    for (case, rules, unused) in [
        ("two", vec![main, alpha], 1),
        ("three", vec![main, alpha, (beta, "three")], 2),
        (
            "four",
            vec![
                main,
                alpha,
                (beta, "two"),
                ("'https://extra.example.test/index'", "one"),
            ],
            2,
        ),
        ("coalesced", vec![main, alpha, alpha, (beta, "two")], 2),
        ("conflicting", vec![main, alpha, (beta, "three")], 0),
    ] {
        let root = temp.path().join(case);
        init_project(
            &old,
            &root,
            &InitOptions {
                apps: Some(
                    names[..rules.len()]
                        .iter()
                        .map(|name| ComponentSpec {
                            name: (*name).into(),
                            template: "rust-api".into(),
                            uses: Vec::new(),
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            &NoResolver,
        )?;
        let mut original = BTreeMap::new();
        for (app, (origin, dir)) in names.iter().zip(rules) {
            let vendor = root.join("vendor").join(dir);
            let leaf = format!("leaf-{dir}");
            let package =
                |name: &str| format!("[package]\nname='{name}'\nversion='1.0.0'\nedition='2024'\n");
            put(
                &vendor,
                "Cargo.toml",
                format!(
                    "{}[dependencies]\n{leaf}={{path='../{leaf}'}}\n",
                    package("common")
                )
                .as_bytes(),
            );
            put(&vendor, "src/lib.rs", b"");
            let leaf_dir = root.join("vendor").join(&leaf);
            put(&leaf_dir, "Cargo.toml", package(&leaf).as_bytes());
            put(&leaf_dir, "src/lib.rs", b"");
            let component = root.join("apps").join(app);
            let deps = match *app {
                "client" => "[dependencies]\ncommon='1'\n",
                "server" if case == "conflicting" => {
                    "[dependencies]\ncommon={path='../../vendor/two'}\n"
                }
                _ => "",
            };
            put(
                &component,
                "Cargo.toml",
                format!(
                    "{}{deps}[patch.{origin}]\ncommon={{path='../../vendor/{dir}'}}\n",
                    package(app)
                )
                .as_bytes(),
            );
            cargo(&component, &["generate-lockfile", "--offline"])?;
            original.extend(selected(&component)?);
        }
        let before = author_port_payload(&root);
        if case == "conflicting" {
            for preview in [true, false] {
                let error = update(&next, &root, preview, &validator).unwrap_err();
                let details = format!("{error:#}");
                assert!(
                    details.contains("different directories for common 1.0.0")
                        && details.contains("vendor/one/Cargo.toml")
                        && details.contains("vendor/two/Cargo.toml"),
                    "{details}"
                );
                assert_eq!(author_port_payload(&root), before);
            }
        } else {
            let planned = update(&next, &root, true, &validator)?;
            assert_eq!(author_port_payload(&root), before);
            let applied = update(&next, &root, false, &validator)?;
            assert_eq!(
                planned.state.cargo_resolution,
                applied.state.cargo_resolution
            );
            let proof = applied.state.cargo_resolution.unwrap();
            assert_eq!(
                proof.inputs.local_selections,
                Some(BTreeSet::from(["vendor/one".to_owned()]))
            );
            assert_eq!(
                cargo_file(&root, "Cargo.lock")["patch"]["unused"]
                    .as_array()
                    .unwrap()
                    .len(),
                unused,
                "{case}"
            );
            fetch_locked(&root)?;
            assert_eq!(selected(&root)?, original, "{case}");
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_migrates_an_unused_registry_patch_with_a_selected_checksum() -> Result<()>
{
    let temp = TempDir::new()?;
    let old = fixture_release(temp.path(), "0.5.1");
    let next = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init_legacy_rust_pair(&old, &root);
    let index = "sparse+https://registry.example.test/index/";
    put(
        &root,
        ".cargo/config.toml",
        format!("[registries.fixture]\nindex='{index}'\n").as_bytes(),
    );
    put(temp.path(), ".cargo/config.toml", format!(
        "[source.fixture]\nregistry='{index}'\nreplace-with='vendored'\n[source.vendored]\ndirectory='registry'\n"
    ).as_bytes());
    let remote = temp.path().join("registry/widget-1.0.0");
    put(
        &remote,
        "Cargo.toml",
        b"[package]\nname='widget'\nversion='1.0.0'\nedition='2024'\n",
    );
    put(&remote, "src/lib.rs", b"");
    let checksum = "a".repeat(64);
    put_json(
        &remote,
        ".cargo-checksum.json",
        &json!({"files": {}, "package": checksum}),
    );
    let mut original = BTreeMap::new();
    for (name, table) in [("server", "patch.crates-io"), ("worker", "dependencies")] {
        let app = root.join("apps").join(name);
        put(&app, "Cargo.toml", format!(
            "[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n[{table}]\nwidget={{registry='fixture',version='=1.0.0'}}\n"
        ).as_bytes());
        cargo(&app, &["generate-lockfile", "--offline"])?;
        original.extend(selected(&app)?);
    }
    let server = root.join("apps/server");
    let lock = cargo_file(&server, "Cargo.lock");
    assert!(lock["patch"]["unused"][0].get("checksum").is_none());
    let checked = MigrationValidator::new(fetch_locked);
    let before = author_port_payload(&root);
    let planned = update(&next, &root, true, &checked)?;
    assert_eq!(author_port_payload(&root), before);
    let applied = update(&next, &root, false, &checked)?;
    assert_eq!(
        planned.state.cargo_resolution,
        applied.state.cargo_resolution
    );
    let lock = cargo_file(&root, "Cargo.lock");
    assert!(lock.get("patch").is_none());
    let entry = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some("widget"))
        .unwrap();
    assert_eq!(entry["source"].as_str(), Some(index));
    assert_eq!(entry["checksum"].as_str(), Some(checksum.as_str()));
    fetch_locked(&root)?;
    assert_eq!(selected(&root)?, original);
    Ok(())
}
