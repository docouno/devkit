use super::*;

const DESKTOP_FILES: [&str; 4] = [
    ".devkit/runtime/desktop.Dockerfile",
    ".devkit/runtime/desktop-seccomp.json",
    ".devkit/runtime/licenses/Apache-2.0.txt",
    ".devkit/runtime/licenses/moby-profiles.NOTICE",
];

const RETIRED_FILES: [&str; 2] = [
    ".devkit/compose.workspace.json",
    ".devkit/runtime/compose.tools.yaml",
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
    for path in RETIRED_FILES {
        assert!(
            !root.join(path).exists() && !root.join(path).is_symlink(),
            "{path}"
        );
        assert!(!state.managed.contains_key(path), "{path}");
        assert!(!checksums.contains(path), "{path}");
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
    // 0.6.0 recovers both Cargo-migrating Rust and Node profile transitions.
    // The 0.5.1 engine predates root Cargo journal paths (docs/migrations/0.6.0.md
    // directs Rust recovery to the cached 0.6.0 engine), while still recovering
    // Node profile transitions cleanly offline.
    let current = PathBuf::from(env!("CARGO_BIN_EXE_devkit"));
    let genuine = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts/baseline/v0.5.1/payload/devkit");
    let mut cases = vec![
        (&current, "rust", false, false),
        (&current, "node", false, false),
        (&current, "electron-react", false, true),
        (&current, "rust", true, true),
        (&current, "node", true, true),
    ];
    if devkit::distribution::Target::host().unwrap() == devkit::distribution::Target::LinuxX64
        && genuine.is_file()
    {
        cases.extend([
            (&genuine, "node", false, false),
            (&genuine, "electron-react", false, true),
            (&genuine, "node", true, true),
        ]);
    }
    for (i, (engine, template, add, desktop_after)) in cases.into_iter().enumerate() {
        let mut stop_paths = Vec::new();
        if !add {
            stop_paths.extend(RETIRED_FILES);
        }
        if add || !desktop_after {
            stop_paths.push(DESKTOP_FILES[3]);
        }
        for (j, stop_path) in stop_paths.into_iter().enumerate() {
            let project = temp.path().join(format!("checkout-{i}-{j}"));
            init(
                if add { &new } else { &old },
                &project,
                template,
                PackageMode::Registry,
            );
            let before = author_port_payload(&project);
            assert_eq!(before[".devkit/runtime/devkit.sh"].mode, 0o755);
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "interruption_worker", "--ignored"])
                .env("DEVKIT_TEST_RELEASE", &new)
                .env("DEVKIT_TEST_TARGET", &project)
                .env("DEVKIT_TEST_WRITE_PATH", stop_path);
            if add {
                child.env("DEVKIT_TEST_ADD_ELECTRON", "1");
            }
            let interruption = child.output().unwrap();
            assert_eq!(interruption.status.code(), Some(86), "{interruption:?}");
            for path in RETIRED_FILES {
                assert_eq!(
                    project.join(path).exists(),
                    !add && path > stop_path,
                    "{path}"
                );
            }
            assert_eq!(
                project.join(".devkit/runtime/licenses").is_dir(),
                add || stop_path < DESKTOP_FILES[3]
            );
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
                update_project(&new, &project, &UpdateOptions::default(), &FixtureResolver)
                    .unwrap();
            }
            assert_scoped_checkout(&project, desktop_after);
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

fn read_checkout_command(root: &Path, command: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args(["--offline", "--json", "--cache-dir"])
        .arg(root.join(".devkit/state/empty-cache"))
        .arg(command)
        .arg(root)
        .env_clear()
        .env("CI", "1")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{command}: {out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    if command == "doctor" {
        report["result"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "checkout_state")
            .unwrap()
            .clone()
    } else {
        report["error"].clone()
    }
}

#[test]
fn retired_managed_files_are_removed_only_when_their_identities_match() {
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let current = fixture_release(temp.path(), "0.6.0");
    for template in ["node", "electron-react"] {
        let legacy = temp.path().join(format!("legacy-{template}"));
        init(&old, &legacy, template, PackageMode::Registry);
        for path in RETIRED_FILES {
            let original = fs::read(legacy.join(path)).unwrap();
            put(&legacy, path, b"owner changes\n");
            let before = author_port_payload(&legacy);
            let error = update_project(&current, &legacy, &UpdateOptions::default(), &NoResolver)
                .unwrap_err();
            assert!(error.to_string().contains(path), "{error:#}");
            assert_eq!(author_port_payload(&legacy), before);
            put(&legacy, path, &original);
        }
        let before = author_port_payload(&legacy);
        let preview = update_project(
            &current,
            &legacy,
            &UpdateOptions {
                preview: true,
                ..Default::default()
            },
            &FixtureResolver,
        )
        .unwrap();
        assert_eq!(author_port_payload(&legacy), before);
        let applied = update_project(
            &current,
            &legacy,
            &UpdateOptions::default(),
            &FixtureResolver,
        )
        .unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .map(|file| &file.path)
                .collect::<Vec<_>>(),
            applied
                .files
                .iter()
                .map(|file| &file.path)
                .collect::<Vec<_>>(),
        );
        for path in RETIRED_FILES {
            let file = applied.files.iter().find(|file| file.path == path).unwrap();
            assert_eq!(file.before.as_ref(), Some(&before[path]));
            assert!(file.after.is_none(), "{path}");
        }
        assert_scoped_checkout(&legacy, template == "electron-react");

        for path in RETIRED_FILES {
            for modified in [false, true] {
                let mut state = read_project_state(&legacy).unwrap();
                put(&legacy, path, b"{}\n");
                record_managed_change(&legacy, &mut state, path, true);
                if modified {
                    put(&legacy, path, b"owner changes\n");
                }
                let before = author_port_payload(&legacy);
                for command in ["describe", "status"] {
                    let report = read_checkout_command(&legacy, command);
                    assert!(
                        report["message"].as_str().unwrap().contains(path),
                        "{report}"
                    );
                }
                let doctor = read_checkout_command(&legacy, "doctor");
                assert_eq!(doctor["status"], "fail");
                if !modified {
                    assert_eq!(doctor["remediation"], "make sync");
                }
                for options in [
                    UpdateOptions::default(),
                    UpdateOptions {
                        reconcile_lock: true,
                        ..Default::default()
                    },
                    UpdateOptions {
                        package_mode: Some(PackageMode::Local),
                        ..Default::default()
                    },
                ] {
                    let error =
                        update_project(&current, &legacy, &options, &NoResolver).unwrap_err();
                    assert!(error.to_string().contains(path), "{error:#}");
                    assert_eq!(author_port_payload(&legacy), before);
                }
                if modified {
                    let error =
                        sync_project(&current, &legacy, &UpdateOptions::default(), &NoResolver)
                            .unwrap_err();
                    assert!(error.to_string().contains(path), "{error:#}");
                    assert_eq!(author_port_payload(&legacy), before);
                    put(&legacy, path, b"{}\n");
                }
                sync_project(
                    &current,
                    &legacy,
                    &UpdateOptions::default(),
                    &FixtureResolver,
                )
                .unwrap();
                assert_scoped_checkout(&legacy, template == "electron-react");
            }
        }
    }
}

#[test]
fn legacy_status_remains_available_during_native_lock_repair() {
    use devkit::runtime::{RuntimeRequest, ToolAdmission};

    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.5.1");
    for template in ["rust", "node"] {
        let root = temp.path().join(template);
        let mut state = init(&release, &root, template, PackageMode::Registry);
        let lock = if template == "rust" {
            root.join(state.components.keys().next().unwrap())
                .join("Cargo.lock")
        } else {
            root.join("pnpm-lock.yaml")
        };
        fs::remove_file(lock).unwrap();
        let request = RuntimeRequest::new(root.clone(), "status".into());
        let admission = ToolAdmission::capture_status_compat(&request).unwrap();
        assert_eq!(admission.state.version, state.version);

        let mut stale_projections = vec![json!({"services": {}})];
        if template == "node" {
            let mut projection: Value = read_json(&root.join(RETIRED_FILES[0])).unwrap();
            let mut volumes = projection["services"]["node"]["volumes"]
                .as_array()
                .unwrap()
                .clone();
            volumes.push(json!({
                "type": "bind",
                "source": "${DEVKIT_PROJECT_ROOT:?}/vendor/local/node_modules",
                "target": "/workspace/vendor/local/node_modules",
                "read_only": false,
                "bind": {"create_host_path": false, "selinux": "z"}
            }));
            projection["services"]["node"]["volumes"] = json!(&volumes);
            projection["services"]["desktop"]["volumes"] = json!(&volumes);
            write_json(&root.join(RETIRED_FILES[0]), &projection).unwrap();
            record_managed_change(&root, &mut state, RETIRED_FILES[0], true);
            assert!(ToolAdmission::capture_status_compat(&request).is_ok());

            let mut missing_root = projection.clone();
            volumes.retain(|mount| mount["target"] != "/workspace/node_modules");
            missing_root["services"]["node"]["volumes"] = json!(&volumes);
            missing_root["services"]["desktop"]["volumes"] = json!(&volumes);
            stale_projections.push(missing_root);
            stale_projections
                .push(json!({"services": {"node": {"volumes": []}, "desktop": {"volumes": []}}}));
        }
        for projection in stale_projections {
            write_json(&root.join(RETIRED_FILES[0]), &projection).unwrap();
            record_managed_change(&root, &mut state, RETIRED_FILES[0], true);
            let error = match ToolAdmission::capture_status_compat(&request) {
                Ok(_) => panic!("admitted a stale workspace projection"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("projection is stale"),
                "{error:#}"
            );
        }
    }
}

#[test]
fn retired_runtime_directory_is_refused_with_a_managed_child() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("current");
    let mut state = init(&release, &root, "rust", PackageMode::Registry);
    let directory = RETIRED_FILES[1];
    let child = format!("{directory}/README.txt");
    put(&root, &child, b"local content\n");
    record_managed_change(&root, &mut state, &child, true);
    let before = author_port_payload(&root);
    let doctor = read_checkout_command(&root, "doctor");
    assert_eq!(doctor["status"], "fail");
    assert_eq!(doctor["remediation"], format!("rmdir {directory}"));
    let error = sync_project(&release, &root, &UpdateOptions::default(), &NoResolver).unwrap_err();
    assert!(error.to_string().contains(directory), "{error:#}");
    assert_eq!(author_port_payload(&root), before);
}

#[test]
fn stray_retired_paths_require_explicit_owner_cleanup() {
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, symlink},
    };

    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for template in ["node", "electron-react"] {
        let root = temp.path().join(template);
        init(&release, &root, template, PackageMode::Registry);
        let inventory = capture_compose_inputs(&root).unwrap();
        let options = UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        };
        let baseline = source_signature(&root, Some(&read_project_state(&root).unwrap())).unwrap();
        for path in RETIRED_FILES {
            let entry = root.join(path);
            for kind in ["file", "directory", "symlink", "fifo"] {
                match kind {
                    "file" => put(&root, path, b"owner file\n"),
                    "directory" => fs::create_dir(&entry).unwrap(),
                    "symlink" => symlink("missing-owner-target", &entry).unwrap(),
                    _ => {
                        let c_path = std::ffi::CString::new(entry.as_os_str().as_bytes()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);
                    }
                }
                let state = read_project_state(&root).unwrap();
                let before = source_signature(&root, Some(&state)).unwrap();
                let doctor = read_checkout_command(&root, "doctor");
                let utility = if kind == "directory" { "rmdir" } else { "rm" };
                assert_eq!(doctor["status"], "fail");
                assert_eq!(doctor["remediation"], format!("{utility} {path}"));
                for command in ["describe", "status"] {
                    let report = read_checkout_command(&root, command);
                    assert!(
                        report["message"].as_str().unwrap().contains(path),
                        "{report}"
                    );
                }
                let error = match admit_runtime_project(&root, "0.6.0") {
                    Ok(_) => panic!("admitted stray {path} ({kind})"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains(path), "{error:#}");
                for result in [
                    update_project(&release, &root, &options, &NoResolver),
                    sync_project(&release, &root, &options, &NoResolver),
                    add_project_component(
                        &release,
                        &root,
                        Role::App,
                        ComponentSpec {
                            name: "extra".into(),
                            template: "node-api".into(),
                            uses: vec![],
                        },
                        &options,
                        &NoResolver,
                    ),
                    register_project_component(
                        &release,
                        &root,
                        Role::App,
                        "extra".into(),
                        ToolingProfile::Node,
                        &options,
                        &NoResolver,
                    ),
                ] {
                    let error = result.unwrap_err();
                    assert!(error.to_string().contains(path), "{error:#}");
                }
                assert_eq!(source_signature(&root, Some(&state)).unwrap(), before);
                let typ = fs::symlink_metadata(&entry).unwrap().file_type();
                assert_eq!(typ.is_fifo(), kind == "fifo");
                if kind == "directory" {
                    fs::remove_dir(&entry).unwrap();
                } else {
                    fs::remove_file(&entry).unwrap();
                }
                assert_eq!(source_signature(&root, Some(&state)).unwrap(), baseline);
            }
        }
    }
}

#[test]
fn describe_rejects_a_consistent_manifest_with_the_wrong_profile_runtime() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for (case, template, path) in [
        ("extra-pin", "rust", ".devkit/runtime/toolchains.env"),
        ("extra-file", "node", DESKTOP_FILES[1]),
        ("missing-file", "electron-react", DESKTOP_FILES[3]),
        ("missing-mit", "rust", ".devkit/runtime/LICENSE"),
        ("wrong-recipe", "electron-react", DESKTOP_FILES[0]),
        ("stale-make", "rust", ".devkit/project.mk"),
    ] {
        let root = temp.path().join(case);
        let mut state = init(&release, &root, template, PackageMode::Registry);
        match case {
            "missing-file" | "missing-mit" => fs::remove_file(root.join(path)).unwrap(),
            "wrong-recipe" => put(&root, path, b"FROM another-image\n"),
            "stale-make" => put(&root, path, b"# stale projection\n"),
            _ => put(
                &root,
                path,
                &fs::read(release.join(path.strip_prefix(".devkit/").unwrap())).unwrap(),
            ),
        }
        record_managed_change(
            &root,
            &mut state,
            path,
            !matches!(case, "missing-file" | "missing-mit"),
        );
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
            "stale-make" => "projection is stale",
            _ => path,
        };
        assert!(error.contains(expected), "{case}: {error}");
        if case == "stale-make" {
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
        if case == "missing-mit" {
            sync_project(&release, &root, &UpdateOptions::default(), &NoResolver).unwrap();
            assert_scoped_checkout(&root, false);
        }
    }
}

#[test]
fn scoped_checkouts_require_a_complete_release_with_a_matching_recipe() {
    for (case, path) in [
        ("missing", "runtime/licenses/moby-profiles.NOTICE"),
        ("mismatch", "runtime/desktop.Dockerfile"),
        ("no-pin", "runtime/toolchains.env"),
        ("retired-compose", "runtime/compose.tools.yaml"),
        ("retired-compose-dir", "runtime/compose.tools.yaml"),
    ] {
        let temp = TempDir::new().unwrap();
        let release = fixture_release(temp.path(), "0.6.0");
        match case {
            "missing" => fs::remove_file(release.join(path)).unwrap(),
            "mismatch" => put(&release, path, b"FROM different\n"),
            "retired-compose" => put(&release, path, b"services: {}\n"),
            "retired-compose-dir" => fs::create_dir(release.join(path)).unwrap(),
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
        let expected = if matches!(case, "missing" | "retired-compose" | "retired-compose-dir") {
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
fn first_electron_preserves_hidden_content_in_the_reserved_runtime() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    init(&release, &root, "rust", PackageMode::Registry);
    put(
        &root,
        ".devkit/runtime/licenses/.artifacts/owner",
        b"owned locally",
    );
    let before = author_port_payload(&root);
    let error = add_electron(&release, &root, "shell", None).unwrap_err();
    assert!(
        format!("{error:#}").contains(".devkit/runtime/licenses"),
        "{error:#}"
    );
    assert_eq!(author_port_payload(&root), before);
    assert!(!root.join(PENDING_PATH).exists());
    assert!(!root.join("apps/shell").exists());
}

#[test]
fn profile_transitions_preserve_runtime_content_created_during_commit() {
    struct OwnerWrite<'a> {
        root: &'a Path,
        file: &'a str,
        after: &'a str,
    }
    impl MutationHooks for OwnerWrite<'_> {
        fn after_write(&self, _: usize, path: &str) -> Result<()> {
            if path == self.after {
                put(self.root, self.file, b"owned locally");
            }
            Ok(())
        }
    }
    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let new = fixture_release(temp.path(), "0.6.0");
    for (i, (add, after, owner)) in [
        (
            false,
            DESKTOP_FILES[2],
            ".devkit/runtime/licenses/.git/owner",
        ),
        (
            false,
            DESKTOP_FILES[3],
            ".devkit/runtime/licenses/.git/owner",
        ),
        (false, DESKTOP_FILES[3], ".devkit/runtime/owner.txt"),
        (false, RETIRED_FILES[0], RETIRED_FILES[0]),
        (false, RETIRED_FILES[1], RETIRED_FILES[1]),
        (true, DESKTOP_FILES[3], RETIRED_FILES[0]),
        (true, DESKTOP_FILES[3], RETIRED_FILES[1]),
        (
            true,
            DESKTOP_FILES[3],
            ".devkit/runtime/licenses/.artifacts/owner",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let root = temp.path().join(format!("consumer-{i}"));
        init(
            if add { &new } else { &old },
            &root,
            "rust",
            PackageMode::Registry,
        );
        let before = author_port_payload(&root);
        let hooks = OwnerWrite {
            root: &root,
            file: owner,
            after,
        };
        let result = if add {
            add_electron(&new, &root, "shell", Some(&hooks))
        } else {
            update_project(
                &new,
                &root,
                &UpdateOptions {
                    hooks: Some(&hooks),
                    ..Default::default()
                },
                &NoResolver,
            )
        };
        let error = result.unwrap_err();
        let expected = if RETIRED_FILES.contains(&owner) {
            owner
        } else {
            ".devkit/runtime/"
        };
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(fs::read(root.join(owner)).unwrap(), b"owned locally");
        if !add && RETIRED_FILES.contains(&owner) {
            assert!(
                format!("{error:#}").contains("automatic rollback was blocked"),
                "{error:#}"
            );
            assert!(root.join(PENDING_PATH).is_file());
            fs::remove_file(root.join(owner)).unwrap();
            recover_project(&root).unwrap();
            assert_eq!(author_port_payload(&root), before);
        } else {
            let mut restored = author_port_payload(&root);
            restored.remove(owner);
            assert_eq!(restored, before);
        }
        assert!(!root.join(PENDING_PATH).exists());
        assert!(!root.join("apps/shell").exists());
    }
}

#[test]
fn idempotent_add_and_register_admit_the_accepted_runtime() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    for (case, register) in [
        ("licenses-dir", true),
        ("extra-pin", false),
        ("stale-make", false),
        ("stray-workspace", false),
        ("forged-workspace", true),
    ] {
        let root = temp.path().join(format!("consumer-{case}"));
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
        let expected = match case {
            "licenses-dir" => {
                fs::create_dir(root.join(".devkit/runtime/licenses")).unwrap();
                ".devkit/runtime/licenses"
            }
            "extra-pin" => {
                let path = ".devkit/runtime/toolchains.env";
                fs::copy(release.join("runtime/toolchains.env"), root.join(path)).unwrap();
                let mut state = read_project_state(&root).unwrap();
                record_managed_change(&root, &mut state, path, true);
                "DEVKIT_DESKTOP_RECIPE_HASH"
            }
            "stale-make" => {
                let path = ".devkit/project.mk";
                put(&root, path, b"# stale projection\n");
                let mut state = read_project_state(&root).unwrap();
                record_managed_change(&root, &mut state, path, true);
                "projection is stale"
            }
            _ => {
                let path = RETIRED_FILES[0];
                put(&root, path, b"{}\n");
                if case == "forged-workspace" {
                    let mut state = read_project_state(&root).unwrap();
                    record_managed_change(&root, &mut state, path, true);
                }
                path
            }
        };
        let before = author_port_payload(&root);
        let error = apply().unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{case}: {error:#}");
        assert_eq!(author_port_payload(&root), before);
    }
}

#[test]
fn runtime_inventory_rejects_unlisted_dirs_and_accepts_non_normalized_root() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("consumer");
    let state = init(&release, &root, "rust", PackageMode::Registry);

    let non_normalized = root.join("apps/..");
    verify_managed_set(&non_normalized, &state).unwrap();
    let manifest_id = identity(&root.join(MANIFEST_PATH)).unwrap().unwrap();
    admit_runtime_snapshot(&non_normalized, state.clone(), manifest_id).unwrap();

    for dir in [
        ".devkit/runtime/.git",
        ".devkit/runtime/extra",
        ".devkit/runtime/desktop.Dockerfile",
    ] {
        fs::create_dir(root.join(dir)).unwrap();
        let err = verify_managed_set(&root, &state).unwrap_err();
        assert!(
            format!("{err:#}").contains("Unlisted runtime directory"),
            "{dir}: {err:#}"
        );
        fs::remove_dir(root.join(dir)).unwrap();
    }

    // Unclassified attribution keys in manifest.json are rejected even if on disk.
    let extra_notice = ".devkit/runtime/licenses/extra.NOTICE";
    put(&root, extra_notice, b"unclassified\n");
    let mut forged = state;
    record_managed_change(&root, &mut forged, extra_notice, true);
    let err = verify_managed_set(&root, &forged).unwrap_err();
    assert!(
        format!("{err:#}").contains("Unclassified managed runtime attribution"),
        "{err:#}"
    );
}

#[test]
fn sync_preserves_pnpm_lock_comments_and_empty_packages_map() {
    let temp = TempDir::new().unwrap();
    let release = fixture_release(temp.path(), "0.6.0");
    let root = temp.path().join("node-consumer");
    let initial = init(&release, &root, "node", PackageMode::Registry);

    let lock_path = root.join("pnpm-lock.yaml");
    let original = fs::read_to_string(&lock_path).unwrap();
    assert!(original.contains("apps/main:\n"));
    let commented = format!(
        "# preserved header\n{}",
        original.replacen("apps/main:\n", "apps/main:\n    dependencies: {}\n", 1)
    );
    fs::write(&lock_path, &commented).unwrap();

    let synced = sync_project(&release, &root, &UpdateOptions::default(), &NoResolver).unwrap();
    assert!(synced.no_op);
    assert_eq!(synced.state.generation, initial.generation);
    assert_eq!(fs::read_to_string(&lock_path).unwrap(), commented);
}

#[test]
fn dirty_runtime_is_rejected_before_node_lock_resolver_runs() {
    struct FailIfCalledResolver;
    impl LockResolver for FailIfCalledResolver {
        fn regenerate(&self, _: &Path, _: &AdmittedRelease, _: Option<&str>) -> Result<()> {
            panic!("resolver must not run on an invalid runtime directory");
        }
    }

    let temp = TempDir::new().unwrap();
    let old = fixture_release(temp.path(), "0.5.1");
    let new = fixture_release(temp.path(), "0.6.0");

    let legacy = temp.path().join("legacy-node");
    init(&old, &legacy, "node", PackageMode::Registry);
    fs::create_dir(legacy.join(".devkit/runtime/licenses/.git")).unwrap();
    for preview in [true, false] {
        let err = update_project(
            &new,
            &legacy,
            &UpdateOptions {
                preview,
                ..Default::default()
            },
            &FailIfCalledResolver,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains(".devkit/runtime/licenses/.git"),
            "{err:#}"
        );
    }

    let headless = temp.path().join("headless-node");
    init(&new, &headless, "node", PackageMode::Registry);
    put(
        &headless,
        ".devkit/runtime/licenses/.artifacts/owner",
        b"owned",
    );
    let inventory = capture_compose_inputs(&headless).unwrap();
    let err = add_project_component(
        &new,
        &headless,
        Role::App,
        ComponentSpec {
            name: "shell".into(),
            template: "electron-react".into(),
            uses: vec![],
        },
        &UpdateOptions {
            compose_inventory: Some(&inventory),
            ..Default::default()
        },
        &FailIfCalledResolver,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains(".devkit/runtime/licenses"),
        "{err:#}"
    );

    let register = temp.path().join("register-node");
    init(&new, &register, "node", PackageMode::Registry);
    copy_tree(
        &new.join("templates/node-api"),
        &register.join("apps/extra"),
        &|_| false,
        false,
    )
    .unwrap();
    let mut manifest: Value = read_json(&register.join("apps/extra/package.json")).unwrap();
    manifest["name"] = json!("registered-extra");
    put_json(&register, "apps/extra/package.json", &manifest);
    let hook = "apps/extra/.pnpmfile.cjs";
    put(&register, hook, b"module.exports = {};\n");
    let err = register_project_component(
        &new,
        &register,
        Role::App,
        "extra".into(),
        ToolingProfile::Node,
        &UpdateOptions::default(),
        &FailIfCalledResolver,
    )
    .unwrap_err();
    assert!(err.to_string().contains(hook), "{err:#}");
}
