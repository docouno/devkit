use super::*;

const DESKTOP_FILES: [&str; 4] = [
    ".devkit/runtime/desktop.Dockerfile",
    ".devkit/runtime/desktop-seccomp.json",
    ".devkit/runtime/licenses/Apache-2.0.txt",
    ".devkit/runtime/licenses/moby-profiles.NOTICE",
];

pub(super) fn add_electron(
    release: &Path,
    root: &Path,
    name: &str,
    hooks: Option<&dyn MutationHooks>,
) -> Result<MutationOutcome> {
    let inventory = capture_compose_inputs(root)?;
    add_project_component(
        release,
        root,
        Role::App,
        ComponentSpec {
            name: name.into(),
            template: "electron-react".into(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            hooks,
            ..Default::default()
        },
        &FixtureResolver,
    )
}

fn assert_scoped_checkout(root: &Path, desktop: bool) {
    let state = read_project_state(root).unwrap();
    let checksums = fs::read_to_string(root.join(CHECKSUM_PATH)).unwrap();
    for path in DESKTOP_FILES {
        assert_eq!(root.join(path).is_file(), desktop, "{path}");
        assert_eq!(state.managed.contains_key(path), desktop, "{path}");
        assert_eq!(checksums.contains(path), desktop, "{path}");
    }
    assert!(root.join(".devkit/runtime/LICENSE").is_file());
    assert_eq!(root.join(".devkit/runtime/licenses").exists(), desktop);
    let pins = fs::read_to_string(root.join(".devkit/runtime/toolchains.env")).unwrap();
    let desktop_pin = pins
        .lines()
        .find_map(|s| s.strip_prefix("DEVKIT_DESKTOP_RECIPE_HASH="));
    assert_eq!(
        desktop_pin,
        state
            .managed
            .get(DESKTOP_FILES[0])
            .map(|id| id.sha256.as_str())
    );
    let tools = fs::read(root.join(".devkit/runtime/compose.tools.yaml")).unwrap();
    let workspace: Value = read_json(&root.join(".devkit/compose.workspace.json")).unwrap();
    for projection in [parse_compose(&tools).unwrap(), workspace] {
        assert_eq!(projection["services"].get("desktop").is_some(), desktop);
        assert!(projection["services"].get("node").is_some());
    }
    read_runtime_project(root, &state.version).unwrap();
}

#[test]
fn scoped_runtime_follows_registered_profiles_independently_of_app_names() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for (name, template, desktop) in [
        ("desktop", "node-api", false),
        ("server", "rust-api", false),
        ("shell", "electron-react", true),
    ] {
        let root = temp.path().join(name);
        init_project(
            &release,
            &root,
            &InitOptions {
                apps: Some(vec![ComponentSpec {
                    name: name.into(),
                    template: template.into(),
                    uses: vec![],
                }]),
                ..Default::default()
            },
            &FixtureResolver,
        )
        .unwrap();
        assert_scoped_checkout(&root, desktop);
        let apps =
            parse_compose(&fs::read(root.join(".devkit/compose.apps.yaml")).unwrap()).unwrap();
        assert!(apps["services"].get(name).is_some());
        if desktop {
            let second = add_electron(&release, &root, "viewer", None).unwrap();
            assert!(
                second
                    .files
                    .iter()
                    .all(|f| !DESKTOP_FILES.contains(&f.path.as_str()))
            );
            assert_scoped_checkout(&root, true);
        }
    }
    let lib = temp.path().join("library");
    init(&release, &lib, "ts-library", PackageMode::Registry);
    assert_scoped_checkout(&lib, false);
}

#[test]
fn occupied_and_modified_desktop_paths_refuse_mutation_and_preserve_owner_bytes() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let new = fixture_release(temp.path(), "0.6.0");
    let project = temp.path().join("legacy");
    init(&old, &project, "rust", PackageMode::Registry);
    put(&project, DESKTOP_FILES[1], b"owner changes\n");
    let before = author_port_payload(&project);
    let err = update_project(&new, &project, &UpdateOptions::default(), &NoResolver).unwrap_err();
    assert!(format!("{err:#}").contains(DESKTOP_FILES[1]), "{err:#}");
    assert_eq!(author_port_payload(&project), before);

    let current = temp.path().join("current");
    init(&new, &current, "rust", PackageMode::Registry);
    put(&current, DESKTOP_FILES[1], b"project-owned profile\n");
    let before = author_port_payload(&current);
    let err = add_electron(&new, &current, "shell", None).unwrap_err();
    assert!(format!("{err:#}").contains(DESKTOP_FILES[1]), "{err:#}");
    assert_eq!(author_port_payload(&current), before);
    fs::remove_file(current.join(DESKTOP_FILES[1])).unwrap();
    fs::create_dir(current.join(".devkit/runtime/licenses")).unwrap();
    let before = author_port_payload(&current);
    let err = update_project(&new, &current, &UpdateOptions::default(), &NoResolver).unwrap_err();
    assert!(
        format!("{err:#}").contains(".devkit/runtime/licenses"),
        "{err:#}"
    );
    assert_eq!(author_port_payload(&current), before);
    assert!(current.join(".devkit/runtime/licenses").is_dir());
}

#[test]
fn interrupted_profile_transition_recovers_offline_and_can_be_retried() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let new = fixture_release(temp.path(), "0.6.0");
    let mut engines = vec![PathBuf::from(env!("CARGO_BIN_EXE_devkit"))];
    let genuine = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts/baseline/v0.5.1/payload/devkit");
    if genuine.is_file() {
        engines.push(genuine);
    }
    for (i, engine) in engines.iter().enumerate() {
        for add in [true, false] {
            let project = temp.path().join(format!("checkout-{i}-{add}"));
            init(
                if add { &new } else { &old },
                &project,
                "rust",
                PackageMode::Registry,
            );
            let before = author_port_payload(&project);
            assert_eq!(before[".devkit/runtime/devkit.sh"].mode, 0o755);
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "interruption_worker", "--ignored"])
                .env("DEVKIT_TEST_RELEASE", &new)
                .env("DEVKIT_TEST_TARGET", &project)
                .env("DEVKIT_TEST_WRITE_PATH", DESKTOP_FILES[3]);
            if add {
                child.env("DEVKIT_TEST_ADD_ELECTRON", "1");
            }
            let interruption = child.output().unwrap();
            assert_eq!(interruption.status.code(), Some(86), "{interruption:?}");
            assert_eq!(project.join(".devkit/runtime/licenses").is_dir(), add);
            assert_eq!(read_journal(&project).unwrap().schema_version, 2);
            assert_eq!(
                identity(&project.join(MANIFEST_PATH)).unwrap(),
                Some(before[MANIFEST_PATH].clone())
            );
            let result = Command::new(engine)
                .args(["--offline", "--json", "recover"])
                .arg(&project)
                .env_clear()
                .env("CI", "1")
                .output()
                .unwrap();
            assert!(result.status.success(), "{engine:?}: {result:?}");
            let report: Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(report["result"]["recovered"], true);
            assert_eq!(author_port_payload(&project), before);
            assert_eq!(project.join(".devkit/runtime/licenses").is_dir(), !add);
            if add {
                add_electron(&new, &project, "shell", None).unwrap();
            } else {
                update_project(&new, &project, &UpdateOptions::default(), &NoResolver).unwrap();
            }
            assert_scoped_checkout(&project, add);
        }
    }
}

// Keep the accepted hashes coherent so each reader must enforce the format itself.
fn record_managed_change(project: &Path, state: &mut ProjectState, path: &str, tracked: bool) {
    if tracked {
        state
            .managed
            .insert(path.into(), identity(&project.join(path)).unwrap().unwrap());
    } else {
        state.managed.remove(path);
    }
    state.managed.remove(CHECKSUM_PATH);
    let checksums = state
        .managed
        .iter()
        .map(|(name, id)| format!("{}  {name}\n", id.sha256))
        .collect::<String>();
    put(project, CHECKSUM_PATH, checksums.as_bytes());
    state.managed.insert(
        CHECKSUM_PATH.into(),
        identity(&project.join(CHECKSUM_PATH)).unwrap().unwrap(),
    );
    write_json(&project.join(MANIFEST_PATH), state).unwrap();
}

#[test]
fn describe_rejects_a_consistent_manifest_with_the_wrong_profile_runtime() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for (case, template, path) in [
        ("extra-pin", "rust", ".devkit/runtime/toolchains.env"),
        ("extra-file", "node", DESKTOP_FILES[1]),
        ("missing-file", "electron-react", DESKTOP_FILES[3]),
        ("wrong-recipe", "electron-react", DESKTOP_FILES[0]),
        (
            "stale-compose",
            "rust",
            ".devkit/runtime/compose.tools.yaml",
        ),
    ] {
        let root = temp.path().join(case);
        let mut state = init(&release, &root, template, PackageMode::Registry);
        match case {
            "missing-file" => fs::remove_file(root.join(path)).unwrap(),
            "wrong-recipe" => put(&root, path, b"FROM another-image\n"),
            _ => put(
                &root,
                path,
                &fs::read(release.join(path.strip_prefix(".devkit/").unwrap())).unwrap(),
            ),
        }
        record_managed_change(&root, &mut state, path, case != "missing-file");
        let result = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args(["--json", "--cache-dir"])
            .arg(temp.path().join("empty-cache"))
            .arg("describe")
            .arg(&root)
            .env_clear()
            .env("CI", "1")
            .output()
            .unwrap();
        assert!(!result.status.success(), "{case}: {result:?}");
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        let error = report["error"]["message"].as_str().unwrap();
        let expected = match case {
            "extra-pin" | "wrong-recipe" => "DEVKIT_DESKTOP_RECIPE_HASH",
            "stale-compose" => "projection is stale",
            _ => path,
        };
        assert!(error.contains(expected), "{case}: {error}");
        if case == "stale-compose" {
            let restored =
                update_project(&release, &root, &UpdateOptions::default(), &NoResolver).unwrap();
            assert!(restored.applied);
            assert_scoped_checkout(&root, false);
        }
        if case == "missing-file" {
            put(&root, path, b"local attribution\n");
            let before = author_port_payload(&root);
            let err = sync_project(&release, &root, &UpdateOptions::default(), &FixtureResolver)
                .unwrap_err();
            assert!(format!("{err:#}").contains(path), "{err:#}");
            assert_eq!(author_port_payload(&root), before);
        }
    }
}

#[test]
fn scoped_checkouts_require_a_complete_release_with_a_matching_recipe() {
    for (case, path) in [
        ("missing", "runtime/licenses/moby-profiles.NOTICE"),
        ("mismatch", "runtime/desktop.Dockerfile"),
        ("no-pin", "runtime/toolchains.env"),
    ] {
        let temp = TempDir::new().unwrap();
        let release = fixture_release(temp.path(), "0.6.0");
        match case {
            "missing" => fs::remove_file(release.join(path)).unwrap(),
            "mismatch" => put(&release, path, b"FROM different\n"),
            _ => {
                let pins = fs::read_to_string(release.join(path)).unwrap();
                let remaining = pins
                    .lines()
                    .filter(|line| !line.starts_with("DEVKIT_DESKTOP_RECIPE_HASH="))
                    .collect::<Vec<_>>()
                    .join("\n");
                put(&release, path, remaining.as_bytes());
            }
        }
        seal(&release, "0.6.0");
        let expected = if case == "missing" {
            path
        } else {
            "DEVKIT_DESKTOP_RECIPE_HASH"
        };
        for template in ["rust", "electron-react"] {
            let root = temp.path().join(template);
            let error = init_project(
                &release,
                &root,
                &InitOptions {
                    template: Some(template.into()),
                    ..Default::default()
                },
                &FixtureResolver,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert!(!root.exists());
        }
    }
}

#[test]
fn preview_and_apply_preserve_excluded_entries_in_retiring_licenses() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let new = fixture_release(temp.path(), "0.6.0");
    for hidden in [".artifacts", ".git"] {
        let root = temp.path().join(&hidden[1..]);
        init(&old, &root, "rust", PackageMode::Registry);
        let child = format!(".devkit/runtime/licenses/{hidden}");
        fs::create_dir(root.join(&child)).unwrap();
        let before = author_port_payload(&root);
        for preview in [true, false] {
            let error = update_project(
                &new,
                &root,
                &UpdateOptions {
                    preview,
                    ..Default::default()
                },
                &NoResolver,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(&child), "{error:#}");
            assert_eq!(author_port_payload(&root), before);
            assert!(root.join(&child).is_dir());
            assert!(!root.join(PENDING_PATH).exists());
        }
    }
}

#[test]
fn newly_arrived_license_content_rolls_back_the_migration() {
    const OWNER_FILE: &str = ".devkit/runtime/licenses/.git/owner";
    struct OwnerWrite<'a> {
        root: &'a Path,
        after: &'a str,
    }
    impl MutationHooks for OwnerWrite<'_> {
        fn after_write(&self, _: usize, path: &str) -> Result<()> {
            if path == self.after {
                put(self.root, OWNER_FILE, b"owned locally");
            }
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let new = fixture_release(temp.path(), "0.6.0");
    for (i, after) in [DESKTOP_FILES[2], DESKTOP_FILES[3]].into_iter().enumerate() {
        let root = temp.path().join(format!("consumer-{i}"));
        init(&old, &root, "rust", PackageMode::Registry);
        let before = author_port_payload(&root);
        let error = update_project(
            &new,
            &root,
            &UpdateOptions {
                hooks: Some(&OwnerWrite { root: &root, after }),
                ..Default::default()
            },
            &NoResolver,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains(".devkit/runtime/licenses"),
            "{error:#}"
        );
        assert!(!root.join(PENDING_PATH).exists());
        assert_eq!(fs::read(root.join(OWNER_FILE)).unwrap(), b"owned locally");
        let mut restored = author_port_payload(&root);
        restored.remove(OWNER_FILE);
        assert_eq!(restored, before);
    }
}

#[test]
fn idempotent_add_and_register_admit_the_accepted_runtime() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for register in [true, false] {
        let root = temp.path().join(format!("consumer-{register}"));
        init(&release, &root, "rust", PackageMode::Registry);
        if register {
            copy_tree(
                &release.join("templates/rust-api"),
                &root.join("apps/extra"),
                &|_| false,
                false,
            )
            .unwrap();
        }
        let apply = || -> Result<MutationOutcome> {
            let inventory = capture_compose_inputs(&root)?;
            let options = UpdateOptions {
                idempotent: true,
                compose_inventory: Some(&inventory),
                ..Default::default()
            };
            if register {
                register_project_component(
                    &release,
                    &root,
                    Role::App,
                    "extra".into(),
                    ToolingProfile::Rust,
                    &options,
                    &NoResolver,
                )
            } else {
                add_project_component(
                    &release,
                    &root,
                    Role::App,
                    ComponentSpec {
                        name: "extra".into(),
                        template: "rust-api".into(),
                        uses: vec![],
                    },
                    &options,
                    &NoResolver,
                )
            }
        };
        assert!(apply().unwrap().applied);
        assert!(apply().unwrap().no_op);
        let expected = if register {
            fs::create_dir(root.join(".devkit/runtime/licenses")).unwrap();
            ".devkit/runtime/licenses"
        } else {
            let path = ".devkit/runtime/toolchains.env";
            fs::copy(release.join("runtime/toolchains.env"), root.join(path)).unwrap();
            let mut state = read_project_state(&root).unwrap();
            record_managed_change(&root, &mut state, path, true);
            "DEVKIT_DESKTOP_RECIPE_HASH"
        };
        let before = author_port_payload(&root);
        let error = apply().unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(author_port_payload(&root), before);
    }
}
