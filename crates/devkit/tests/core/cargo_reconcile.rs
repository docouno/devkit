use super::*;

struct ResolvedLock<'a>(&'a [u8]);
impl CargoLockResolver for ResolvedLock<'_> {
    fn resolve_cargo(&self, stage: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
        assert!(offline);
        put(stage, "Cargo.lock", self.0);
        Ok(())
    }
}

fn manifest(root: &Path, path: &str, name: &str, dependencies: &str) {
    let cargo =
        format!("[package]\nname='{name}'\nversion='1.0.0'\nedition='2024'\n{dependencies}");
    put(root, &format!("{path}/Cargo.toml"), cargo.as_bytes());
    put(root, &format!("{path}/src/lib.rs"), b"");
}

fn vendor_workspace(root: &Path, extra: &str) {
    put(
        root,
        "Cargo.toml",
        format!(
            "[workspace]\nresolver='3'\nmembers=['packages/core']\nexclude=['vendor']\n{extra}"
        )
        .as_bytes(),
    );
}

#[test]
fn sync_keeps_inactive_optional_edges_out_of_external_crates_and_repairs_required_paths() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    vendor_workspace(&root, "");
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\nhelper={path='../../vendor/helper'}\n",
    );
    manifest(
        &root,
        "vendor/helper",
        "helper",
        "[dependencies]\nleaf={path='../leaf',optional=true}\n",
    );
    manifest(&root, "vendor/leaf", "leaf", "");
    let sync = || sync_project(&release, &root, &offline(), &NoResolver);
    let dormant = b"version=4\n[[package]]\nname='lib-core'\nversion='1.0.0'\ndependencies=['helper']\n[[package]]\nname='helper'\nversion='1.0.0'\n";
    assert!(sync().is_err());
    assert!(
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&ResolvedLock(dormant)),
                ..offline()
            },
            &NoResolver
        )
        .unwrap()
        .applied
    );
    let before = fs::read(root.join("Cargo.lock")).unwrap();
    assert!(!std::str::from_utf8(&before).unwrap().contains("leaf"));
    assert!(sync().unwrap().no_op);
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), before);
    manifest(
        &root,
        "vendor/helper",
        "helper",
        "[dependencies]\nleaf={path='../leaf'}\n",
    );
    assert!(sync().unwrap().applied);
    let graph = cargo_file(&root, "Cargo.lock");
    let helper = graph["package"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some("helper"))
        .unwrap();
    assert_eq!(helper["dependencies"][0].as_str(), Some("leaf"));
    assert!(sync().unwrap().no_op);
}

#[test]
fn sync_reconciles_unused_path_patches_and_reuses_selected_config_patches_offline() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    vendor_workspace(&root, "[patch.crates-io]\nhelper={path='vendor/helper'}\n");
    manifest(&root, "vendor/helper", "helper", "");
    let sync = || sync_project(&release, &root, &offline(), &NoResolver);
    sync().unwrap();
    assert_eq!(
        cargo_file(&root, "Cargo.lock")["patch"]["unused"][0]["name"].as_str(),
        Some("helper")
    );
    vendor_workspace(
        &root,
        "[patch.crates-io]\nhelper={path='vendor/helper'}\n[patch.'https://example.org/index']\nhelper={path='vendor/helper'}\n",
    );
    assert!(sync().unwrap().applied);
    assert_eq!(
        cargo_file(&root, "Cargo.lock")["patch"]["unused"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(sync().unwrap().no_op);
    vendor_workspace(&root, "");
    assert!(sync().unwrap().applied);
    assert!(cargo_file(&root, "Cargo.lock").get("patch").is_none());

    put(
        &root,
        ".cargo/config.toml",
        b"[patch.crates-io]\nhelper={path='vendor/helper'}\n",
    );
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\nhelper='1'\n",
    );
    let accepted=b"version=4\n[[package]]\nname='lib-core'\nversion='1.0.0'\ndependencies=['helper']\n[[package]]\nname='helper'\nversion='1.0.0'\n";
    put(&root, "Cargo.lock", accepted);
    assert!(
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&ResolvedLock(accepted)),
                ..offline()
            },
            &NoResolver
        )
        .unwrap()
        .applied
    );
    assert!(sync().unwrap().no_op);
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), accepted);
}

#[test]
fn a_single_surviving_alias_can_resolve_to_only_one_previously_locked_version() {
    struct Resolve(String);
    impl CargoLockResolver for Resolve {
        fn resolve_cargo(&self, stage: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
            put(stage, "Cargo.lock", self.0.as_bytes());
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    let registry = "registry+https://github.com/rust-lang/crates.io-index";
    let base =
        |v: &str| format!("[[package]]\nname='base64'\nversion='{v}'\nsource='{registry}'\n");
    let core = "version=4\n[[package]]\nname='lib-core'\nversion='1.0.0'\n";
    let previous = format!(
        "{core}dependencies=['base64 0.23.1','base64 0.22.1']\n{}{}",
        base("0.22.1"),
        base("0.23.1")
    );
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\ncurrent={package='base64',version='*'}\n",
    );
    put(&root, "Cargo.lock", previous.as_bytes());
    let sync = |resolver: &dyn CargoLockResolver| {
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(resolver),
                ..Default::default()
            },
            &NoResolver,
        )
    };
    let err = sync(&Resolve(previous.clone())).unwrap_err();
    assert!(format!("{err:#}").contains("lock inconsistent"), "{err:#}");
    assert_eq!(
        fs::read_to_string(root.join("Cargo.lock")).unwrap(),
        previous
    );
    let current = format!("{core}dependencies=['base64']\n{}", base("0.23.1"));
    assert!(sync(&Resolve(current.clone())).unwrap().applied);
    assert_eq!(
        fs::read_to_string(root.join("Cargo.lock")).unwrap(),
        current
    );
    assert!(sync(&NoCargoResolver).unwrap().no_op);
}

#[test]
fn sync_allows_cargo_to_drop_disabled_transitive_edges_and_preserves_unrelated_versions() {
    struct Resolved(String);
    impl CargoLockResolver for Resolved {
        fn resolve_cargo(&self, stage: &Path, _: &AdmittedRelease, _: bool) -> Result<()> {
            put(stage, "Cargo.lock", self.0.as_bytes());
            Ok(())
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init_project(
        &release,
        &root,
        &InitOptions {
            packages: Some(
                ["core", "extra"]
                    .map(|name| ComponentSpec {
                        name: name.into(),
                        template: "rust-library".into(),
                        uses: vec![],
                    })
                    .into(),
            ),
            ..Default::default()
        },
        &NoResolver,
    )
    .unwrap();
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\nbytes='1'\n",
    );
    // Extra previously depended on bytes with features = ["serde"]; its
    // removal also removes bytes' optional serde edge in a real Cargo lock.
    manifest(&root, "packages/extra", "lib-extra", "");
    let make_lock = |version: &str, checksum: &str, previous: bool| {
        let registry = "registry+https://github.com/rust-lang/crates.io-index";
        let mut lock = String::from(
            "version=4\n[[package]]\nname='lib-core'\nversion='1.0.0'\ndependencies=['bytes']\n[[package]]\nname='lib-extra'\nversion='1.0.0'\n",
        );
        if previous {
            lock.push_str("dependencies=['bytes']\n");
        }
        lock.push_str(&format!("[[package]]\nname='bytes'\nversion='{version}'\nsource='{registry}'\nchecksum='{checksum}'\n"));
        if previous {
            lock.push_str(&format!("dependencies=['serde']\n[[package]]\nname='serde'\nversion='1.0.229'\nsource='{registry}'\n"));
        }
        lock
    };
    let checksum = "a".repeat(64);
    let original = make_lock("1.12.1", &checksum, true);
    put(&root, "Cargo.lock", original.as_bytes());
    let sync = |lock: String| {
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&Resolved(lock)),
                ..Default::default()
            },
            &NoResolver,
        )
    };
    for invalid in [
        make_lock("1.12.2", &checksum, false),
        make_lock("1.12.1", &"b".repeat(64), false),
    ] {
        assert!(sync(invalid).is_err());
        assert_eq!(
            fs::read_to_string(root.join("Cargo.lock")).unwrap(),
            original
        );
    }
    let updated = make_lock("1.12.1", &checksum, false);
    assert!(sync(updated.clone()).unwrap().applied);
    assert_eq!(
        fs::read_to_string(root.join("Cargo.lock")).unwrap(),
        updated
    );
}

#[test]
fn sync_allows_shared_transitive_upgrade_when_feature_activates_new_edge() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    let registry = "registry+https://github.com/rust-lang/crates.io-index";
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\nwrapper={version='1',features=['extra']}\n",
    );
    // Before sync: wrapper 1.0.0 depended only on shared 0.5.1.
    // Enabling feature `extra` activates a brand-new `extra-dep 1.0.0` edge on
    // wrapper 1.0.0 that requires `shared 0.5.2`.
    let before_lock = format!(
        "version = 4\n\
         [[package]]\nname = 'lib-core'\nversion = '1.0.0'\ndependencies = ['wrapper']\n\
         [[package]]\nname = 'wrapper'\nversion = '1.0.0'\nsource = '{registry}'\nchecksum = '{}'\ndependencies = ['shared']\n\
         [[package]]\nname = 'shared'\nversion = '0.5.1'\nsource = '{registry}'\nchecksum = '{}'\n",
        "1".repeat(64),
        "2".repeat(64),
    );
    let after_lock = format!(
        "version = 4\n\
         [[package]]\nname = 'lib-core'\nversion = '1.0.0'\ndependencies = ['wrapper']\n\
         [[package]]\nname = 'wrapper'\nversion = '1.0.0'\nsource = '{registry}'\nchecksum = '{}'\ndependencies = ['extra-dep', 'shared']\n\
         [[package]]\nname = 'extra-dep'\nversion = '1.0.0'\nsource = '{registry}'\nchecksum = '{}'\ndependencies = ['shared']\n\
         [[package]]\nname = 'shared'\nversion = '0.5.2'\nsource = '{registry}'\nchecksum = '{}'\n",
        "1".repeat(64),
        "3".repeat(64),
        "4".repeat(64),
    );
    put(&root, "Cargo.lock", before_lock.as_bytes());
    let outcome = sync_project(
        &release,
        &root,
        &UpdateOptions {
            cargo_resolver: Some(&ResolvedLock(after_lock.as_bytes())),
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    assert!(outcome.applied);
    assert_eq!(
        fs::read_to_string(root.join("Cargo.lock")).unwrap(),
        after_lock
    );
}

#[test]
fn add_and_register_preserve_old_local_selections_before_writing() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    vendor_workspace(&root, "");
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\nhelper={path='../../vendor/helper'}\n",
    );
    manifest(&root, "vendor/helper", "helper", "");
    sync_project(&release, &root, &offline(), &NoResolver).unwrap();
    manifest(&root, "packages/core", "lib-core", "");
    manifest(&root, "packages/adopt", "adopt", "");
    let before = author_port_payload(&root);
    let result = add_project_component(
        &release,
        &root,
        Role::Library,
        ComponentSpec {
            name: "new".into(),
            template: "rust-library".into(),
            uses: vec![],
        },
        &offline(),
        &NoResolver,
    );
    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("existing Cargo selection")
            && message.contains("run make sync before adding")
    );
    assert_eq!(before, author_port_payload(&root));
    let group = "packages/adopt";
    let nested = "packages/adopt/inner";
    manifest(&root, nested, "inner", "");
    let root_manifest = root.join("Cargo.toml");
    let original_workspace = fs::read_to_string(&root_manifest).unwrap();
    set_cargo_members(&root, &["packages/core", group, nested]);
    let listed_workspace = format!(
        "{}default-members=['packages/core','{nested}']\n",
        fs::read_to_string(&root_manifest).unwrap()
    );
    put(&root, "Cargo.toml", listed_workspace.as_bytes());
    let before = author_port_payload(&root);
    let register = || {
        register_project_component(
            &release,
            &root,
            Role::Library,
            "adopt".into(),
            ToolingProfile::Rust,
            &offline(),
            &NoResolver,
        )
    };
    let message = format!("{:#}", register().unwrap_err());
    assert!(
        message.contains("existing Cargo selection")
            && message.contains("set aside dependency edits that require packages/adopt")
            && message.contains("restore the accepted dependencies")
            && message.contains("reapply the deferred edits and run make sync")
            && message.contains("move packages/adopt outside apps/ and packages/")
            && message.contains("workspace.members")
            && message.contains("default-members")
            && message.contains("restore the directory and the saved entries"),
        "{message}"
    );
    assert_eq!(before, author_port_payload(&root));
    let parked = temp.path().join("parked");
    fs::rename(root.join(group), &parked).unwrap();
    put(
        &root,
        "Cargo.toml",
        format!("{original_workspace}default-members=['packages/core']\n").as_bytes(),
    );
    let sync = || sync_project(&release, &root, &offline(), &NoResolver);
    assert!(sync().unwrap().applied);
    fs::rename(parked, root.join(group)).unwrap();
    put(&root, "Cargo.toml", listed_workspace.as_bytes());
    let registered = register().unwrap();
    assert_eq!(
        registered
            .state
            .cargo_resolution
            .unwrap()
            .inputs
            .members
            .into_keys()
            .collect::<Vec<_>>(),
        [group, nested, "packages/core"]
    );
    assert_eq!(
        fs::read_to_string(&root_manifest).unwrap(),
        listed_workspace
    );
    assert!(sync().unwrap().no_op);
}

#[test]
fn register_reuses_an_accepted_feature_recipe_with_a_matching_standalone_lock() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    let deps = "[dependencies]\nbytes={version='1',features=['serde']}\n";
    manifest(&root, "packages/core", "lib-core", deps);
    let raw = "version=4\n[[package]]\nname='lib-core'\nversion='1.0.0'\ndependencies=['bytes']\n[[package]]\nname='bytes'\nversion='1.12.1'\nsource='registry+https://github.com/rust-lang/crates.io-index'\ndependencies=['serde']\n[[package]]\nname='serde'\nversion='1.0.229'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n";
    sync_project(
        &release,
        &root,
        &UpdateOptions {
            cargo_resolver: Some(&ResolvedLock(raw.as_bytes())),
            ..offline()
        },
        &NoResolver,
    )
    .unwrap();
    manifest(&root, "packages/duplicate", "duplicate", deps);
    put(
        &root,
        "packages/duplicate/Cargo.lock",
        raw.replace("lib-core", "duplicate").as_bytes(),
    );
    let outcome = register_project_component(
        &release,
        &root,
        Role::Library,
        "duplicate".into(),
        ToolingProfile::Rust,
        &offline(),
        &NoResolver,
    )
    .unwrap();
    assert!(outcome.applied);
    assert!(!root.join("packages/duplicate/Cargo.lock").exists());
    assert!(
        sync_project(&release, &root, &offline(), &NoResolver)
            .unwrap()
            .no_op
    );
}

struct PinnedCargo;
fn cargo(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "cargo {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
impl CargoLockResolver for PinnedCargo {
    fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
        assert!(offline);
        cargo(root, &["update", "--workspace", "--offline"])
    }
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_checks_optional_activation_through_added_and_removed_paths() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    vendor_workspace(&root, "");
    manifest(&root, "vendor/leaf", "leaf", "");
    manifest(
        &root,
        "vendor/shared",
        "shared",
        "[dependencies]\nleaf={path='../leaf',optional=true}\n",
    );
    manifest(
        &root,
        "vendor/bridge",
        "bridge",
        "[dependencies]\nshared={path='../shared', features=['leaf']}\n",
    );
    let helper = "[dependencies]\nshared={path='../../vendor/shared'}\n";
    for with_bridge in [true, false] {
        let mut deps = helper.to_owned();
        if with_bridge {
            deps.push_str("bridge={path='../../vendor/bridge'}\n");
        }
        manifest(&root, "packages/core", "lib-core", &deps);
        assert!(sync_project(&release, &root, &offline(), &NoResolver).is_err());
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&PinnedCargo),
                ..offline()
            },
            &NoResolver,
        )
        .unwrap();
        cargo(
            &root,
            &["metadata", "--locked", "--offline", "--format-version=1"],
        )
        .unwrap();
        let lock = cargo_file(&root, "Cargo.lock");
        assert_eq!(
            lock["package"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"].as_str() == Some("leaf")),
            with_bridge
        );
        assert!(
            sync_project(&release, &root, &offline(), &NoResolver)
                .unwrap()
                .no_op
        );
    }
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_preserves_compatible_versions_from_distinct_paths() -> Result<()> {
    let temp = TempDir::new()?;
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    vendor_workspace(&root, "");
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\na={package='helper',path='../../vendor/a'}\nb={package='helper',path='../../vendor/b'}\n",
    );
    for (dir, version) in [("a", "1.0.0"), ("b", "1.0.1")] {
        let path = format!("vendor/{dir}");
        put(
            &root,
            &format!("{path}/Cargo.toml"),
            format!("[package]\nname='helper'\nversion='{version}'\nedition='2024'\n").as_bytes(),
        );
        put(&root, &format!("{path}/src/lib.rs"), b"");
    }
    cargo(&root, &["update", "--workspace", "--offline"])?;
    let expected = fs::read(root.join("Cargo.lock"))?;
    let sync = || {
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&PinnedCargo),
                ..offline()
            },
            &NoResolver,
        )
    };
    sync()?;
    assert_eq!(fs::read(root.join("Cargo.lock"))?, expected);
    cargo(
        &root,
        &["metadata", "--locked", "--offline", "--format-version=1"],
    )?;
    assert!(sync_project(&release, &root, &offline(), &NoResolver)?.no_op);

    // Two paths with identical name and version cannot be encoded in Cargo.lock.
    manifest(&root, "vendor/b", "helper", "");
    assert!(cargo(&root, &["update", "--workspace", "--offline"]).is_err());
    assert!(sync().is_err());
    assert_eq!(fs::read(root.join("Cargo.lock"))?, expected);
    Ok(())
}

#[test]
fn registering_different_features_carries_unrelated_standalone_pins_through_cargo() -> Result<()> {
    struct Staged {
        upgrade: bool,
    }
    impl CargoLockResolver for Staged {
        fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(offline);
            let lock = cargo_file(root, "Cargo.lock");
            let packages = lock["package"].as_array().unwrap();
            let record = |name: &str| {
                packages
                    .iter()
                    .find(|p| p["name"].as_str() == Some(name))
                    .unwrap()
            };
            assert_eq!(record("bytes")["dependencies"][0].as_str(), Some("serde"));
            assert_eq!(record("itoa")["version"].as_str(), Some("1.0.18"));
            if self.upgrade {
                let changed =
                    fs::read_to_string(root.join("Cargo.lock"))?.replace("1.0.18", "1.0.19");
                put(root, "Cargo.lock", changed.as_bytes());
            }
            Ok(())
        }
    }
    let temp = TempDir::new()?;
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("product");
    init(&release, &root, "rust-library", PackageMode::Local);
    manifest(
        &root,
        "packages/core",
        "lib-core",
        "[dependencies]\nbytes='1'\n",
    );
    let source = "registry+https://github.com/rust-lang/crates.io-index";
    let bytes = format!("[[package]]\nname='bytes'\nversion='1.12.1'\nsource='{source}'\n");
    let old = format!(
        "version=4\n[[package]]\nname='lib-core'\nversion='1.0.0'\ndependencies=['bytes']\n{bytes}"
    );
    sync_project(
        &release,
        &root,
        &UpdateOptions {
            cargo_resolver: Some(&ResolvedLock(old.as_bytes())),
            ..offline()
        },
        &NoResolver,
    )?;
    manifest(
        &root,
        "packages/addon",
        "addon",
        "[dependencies]\nbytes={version='1',features=['serde']}\nitoa='1'\n",
    );
    let incoming = format!(
        "version=4\n[[package]]\nname='addon'\nversion='1.0.0'\ndependencies=['bytes','itoa']\n{bytes}dependencies=['serde']\n[[package]]\nname='serde'\nversion='1.0.229'\nsource='{source}'\n[[package]]\nname='itoa'\nversion='1.0.18'\nsource='{source}'\n"
    );
    put(&root, "packages/addon/Cargo.lock", incoming.as_bytes());
    let register = |upgrade| {
        register_project_component(
            &release,
            &root,
            Role::Library,
            "addon".into(),
            ToolingProfile::Rust,
            &UpdateOptions {
                cargo_resolver: Some(&Staged { upgrade }),
                ..offline()
            },
            &NoResolver,
        )
    };
    let before = author_port_payload(&root);
    assert!(
        format!("{:#}", register(true).unwrap_err())
            .contains("staged pin or checksum for itoa 1.0.18")
    );
    assert_eq!(author_port_payload(&root), before);
    assert!(register(false)?.applied);
    assert!(sync_project(&release, &root, &offline(), &NoResolver)?.no_op);
    Ok(())
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_resolution_path_witnesses_for_excluded_dependencies() {
    use std::cell::Cell;
    use std::collections::BTreeSet;
    struct Resolver(Cell<usize>);
    fn cargo(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("cargo")
            .current_dir(root)
            .args(args)
            .output()?;
        if !output.status.success() {
            bail!(
                "cargo {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output.stdout)
    }
    impl CargoLockResolver for Resolver {
        fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(offline);
            cargo(root, &["update", "--workspace", "--offline"]).map(drop)
        }
        fn selected_local_paths(
            &self,
            root: &Path,
            _: &AdmittedRelease,
            offline: bool,
        ) -> Result<BTreeSet<String>> {
            assert!(offline);
            self.0.set(self.0.get() + 1);
            let meta: Value = serde_json::from_slice(&cargo(
                root,
                &[
                    "metadata",
                    "--locked",
                    "--offline",
                    "--all-features",
                    "--format-version=1",
                ],
            )?)?;
            Ok(meta["packages"]
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
    }
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    vendor_workspace(&root, "");
    let resolver = Resolver(Cell::new(0));
    let sync = || {
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&resolver),
                ..offline()
            },
            &NoResolver,
        )
        .unwrap()
    };
    let worker = |extra: &str| {
        manifest(
            &root,
            "packages/core",
            "lib-core",
            &format!("[dependencies]\nhelper={{path='../../vendor/helper'{extra}}}\n"),
        )
    };
    worker("");
    for (table, optional) in [("dependencies", ",optional=true"), ("dev-dependencies", "")] {
        let helper = format!("[{table}]\nleaf={{path='../dormant'{optional}}}\n");
        manifest(&root, "vendor/helper", "helper", &helper);
        sync();
        assert!(sync().no_op);
        put(&root, "vendor/dormant/Cargo.toml", b"invalid TOML");
        sync();
        fs::remove_file(root.join("vendor/dormant/Cargo.toml")).unwrap();
        manifest(&root, "vendor/required", "leaf", "");
        manifest(&root, "vendor/dormant", "leaf", "");
        manifest(
            &root,
            "packages/core",
            "lib-core",
            "[dependencies]\nhelper={path='../../vendor/helper'}\nleaf={path='../../vendor/required'}\n",
        );
        sync();
        cargo(
            &root,
            &["metadata", "--locked", "--offline", "--format-version=1"],
        )
        .unwrap();
        assert!(sync().no_op);
        fs::remove_file(root.join("vendor/dormant/Cargo.toml")).unwrap();
        worker("");
    }
    assert_eq!(resolver.0.get(), 0);
    manifest(
        &root,
        "vendor/helper",
        "helper",
        "[dependencies]\none={package='leaf',path='../one',optional=true}\ntwo={package='leaf',path='../two',optional=true}\n",
    );
    manifest(&root, "vendor/one", "leaf", "");
    manifest(&root, "vendor/two", "leaf", "");
    let mut previous = None;
    for feature in ["one", "two"] {
        if feature == "two" {
            put(&root, "vendor/one/Cargo.toml", b"now dormant, invalid TOML");
        }
        worker(&format!(",features=['{feature}']"));
        sync();
        let lock = fs::read(root.join("Cargo.lock")).unwrap();
        if let Some(before) = previous {
            assert_eq!(before, lock);
        }
        previous = Some(lock);
        let state: ProjectState = read_json(&root.join(".devkit/manifest.json")).unwrap();
        let paths = state
            .cargo_resolution
            .unwrap()
            .inputs
            .local_selections
            .unwrap();
        assert_eq!(paths, BTreeSet::from([format!("vendor/{feature}")]));
        assert!(sync().no_op);
    }
    // Older receipts did not store the path, and older projects may have no
    // Cargo receipt at all. Both acquire the witness once and then replay it.
    let path = root.join(".devkit/manifest.json");
    for whole_receipt in [false, true] {
        let mut state: Value = read_json(&path).unwrap();
        if whole_receipt {
            state.as_object_mut().unwrap().remove("cargoResolution");
        } else {
            state["cargoResolution"]["inputs"]
                .as_object_mut()
                .unwrap()
                .remove("localSelections");
        }
        write_json(&path, &state).unwrap();
        assert!(sync().applied);
        assert!(sync().no_op);
    }
    assert_eq!(resolver.0.get(), 4);
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_workspace_defaults_and_policy() {
    use std::cell::Cell;

    struct PinnedCargo(Cell<usize>);
    impl CargoLockResolver for PinnedCargo {
        fn resolve_cargo(&self, root: &Path, _: &AdmittedRelease, offline: bool) -> Result<()> {
            assert!(offline);
            self.0.set(self.0.get() + 1);
            let result = Command::new("cargo")
                .current_dir(root)
                .args(["update", "--workspace", "--offline"])
                .output()?;
            anyhow::ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);
    let cargo = PinnedCargo(Cell::new(0));
    let sync = || {
        sync_project(
            &release,
            &root,
            &UpdateOptions {
                cargo_resolver: Some(&cargo),
                ..offline()
            },
            &NoResolver,
        )
    };
    let locked = || {
        Command::new("cargo")
            .current_dir(&root)
            .args(["fetch", "--locked", "--offline"])
            .output()
            .unwrap()
            .status
            .success()
    };

    vendor_workspace(&root, "default-members=['packages/core']\n");
    sync().unwrap();
    assert!(locked());
    assert!(sync().unwrap().no_op);
    assert_eq!(cargo.0.get(), 0);
    let before = fs::read(root.join(MANIFEST_PATH)).unwrap();
    for invalid_glob in ["packages/[c]or?", "packages/*", ".?/packages/*"] {
        vendor_workspace(&root, &format!("default-members=['{invalid_glob}']\n"));
        let err = format!("{:#}", sync().unwrap_err());
        assert!(
            err.contains("Use explicit paths in workspace.default-members")
                || err.contains("escapes its root"),
            "{invalid_glob}: {err}"
        );
        assert_eq!(fs::read(root.join(MANIFEST_PATH)).unwrap(), before);
    }
    vendor_workspace(&root, "default-members=['packages/gone']\n");
    assert!(!locked());
    assert!(sync().is_err());
    assert_eq!(fs::read(root.join(MANIFEST_PATH)).unwrap(), before);
    assert_eq!(cargo.0.get(), 0);

    vendor_workspace(&root, "[profile.release]\nopt-level=1\n");
    sync().unwrap();
    assert!(locked());
    assert!(sync().unwrap().no_op);
    assert_eq!(cargo.0.get(), 1);
    let before = fs::read(root.join(MANIFEST_PATH)).unwrap();
    for invalid in [
        "[workspace]\nresolver='9'\nmembers=['packages/core']\n",
        "[workspace]\nresolver='3'\nmembers=['packages/core']\n[profile.release]\nopt-level=[]\n",
    ] {
        put(&root, "Cargo.toml", invalid.as_bytes());
        assert!(!locked());
        assert!(sync().is_err());
        assert_eq!(fs::read(root.join(MANIFEST_PATH)).unwrap(), before);
    }
    assert_eq!(cargo.0.get(), 3);
}

#[test]
#[ignore = "requires the pinned author Cargo"]
fn pinned_cargo_metadata_prefers_modern_dependency_tables_at_each_scope() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust-library", PackageMode::Local);

    for edition in ["2021", "2024"] {
        vendor_workspace(
            &root,
            &format!("[workspace.package]\nedition='{edition}'\n"),
        );
        for scope in ["", "target.'cfg(unix)'."] {
            for kind in ["dev", "build"] {
                let cargo = format!(
                    "[package]\nname='lib-core'\nversion='1.0.0'\nedition.workspace=true\n[{scope}{kind}-dependencies]\n[{scope}{kind}_dependencies]\nghost={{path='../../vendor/missing'}}\n"
                );
                put(&root, "packages/core/Cargo.toml", cargo.as_bytes());
                let real = Command::new("cargo")
                    .current_dir(&root)
                    .args(["fetch", "--locked", "--offline"])
                    .output()
                    .unwrap();
                let admitted = sync_project(&release, &root, &offline(), &NoResolver);
                if edition == "2021" {
                    assert!(
                        real.status.success(),
                        "{}",
                        String::from_utf8_lossy(&real.stderr)
                    );
                    admitted.unwrap();
                    admit_runtime_project(&root, "0.6.0").unwrap();
                } else {
                    assert!(!real.status.success());
                    assert!(format!("{:#}", admitted.unwrap_err()).contains("2024"));
                }
            }
        }
    }
}
