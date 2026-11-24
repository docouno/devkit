use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    assembler::{
        AssemblyOptions, TargetInput, assemble, verify_migration_index, verify_npm_inputs,
        verify_npm_pack, verify_runtime_notices, verify_rust_template, verify_source_checkout,
    },
    files::{digest, file_map, read_json, write_atomic, write_json},
    processes::ProcessRunner,
};
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn sri(bytes: &[u8]) -> String {
    format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes)))
}

fn prepared_fixture(base: &Path) -> (PathBuf, PathBuf) {
    let root = base.join("source");
    let prepared = base.join("prepared");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&prepared).unwrap();
    write_atomic(&root.join("VERSION"), b"0.4.1\n", 0o644).unwrap();
    write_atomic(&root.join("LICENSE"), b"Original MIT license\n", 0o644).unwrap();
    write_json(
        &root.join("packages/env/package.json"),
        &json!({"name":"@docouno/env","version":"0.4.1"}),
    )
    .unwrap();
    write_atomic(
        &root.join("packages/env/README.md"),
        b"Environment API\n",
        0o644,
    )
    .unwrap();
    let env_bytes = b"opaque frozen native npm archive bytes";
    let env_integrity = sri(env_bytes);
    write_atomic(
        &prepared.join("npm/docouno-env-0.5.0.tgz"),
        env_bytes,
        0o644,
    )
    .unwrap();
    let external = json!({"resolution":{"integrity":sri(b"selected external artifact")}});
    let mut templates = BTreeMap::new();
    for name in [
        "node-api",
        "react-web",
        "electron-react",
        "react-client",
        "ts-library",
        "react-ui",
    ] {
        let manifest = json!({"name":format!("fixture-{name}"),"version":"1.0.0","devDependencies":{"@docouno/env":"0.4.1","external":"1.2.3"}});
        let lock = json!({"lockfileVersion":"9.0","importers":{".":{"devDependencies":{"@docouno/env":{"specifier":"0.4.1","version":"0.4.1"},"external":{"specifier":"1.2.3","version":"1.2.3"}}}},"packages":{"@docouno/env@0.4.1":{"resolution":{"integrity":sri(b"old environment")}},"external@1.2.3":external},"snapshots":{"@docouno/env@0.4.1":{},"external@1.2.3":{}}});
        let manifest_name = format!("templates/{name}/package.json");
        let lock_name = format!("templates/{name}/pnpm-lock.yaml");
        write_json(&root.join(&manifest_name), &manifest).unwrap();
        write_json(&root.join(&lock_name), &lock).unwrap();
        let mut new_manifest = manifest;
        new_manifest["devDependencies"]["@docouno/env"] = json!("0.5.0");
        let mut new_lock = lock;
        new_lock["importers"]["."]["devDependencies"]["@docouno/env"] =
            json!({"specifier":"0.5.0","version":"0.5.0"});
        new_lock["packages"]
            .as_object_mut()
            .unwrap()
            .remove("@docouno/env@0.4.1");
        new_lock["packages"]["@docouno/env@0.5.0"] =
            json!({"resolution":{"integrity":env_integrity}});
        new_lock["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove("@docouno/env@0.4.1");
        new_lock["snapshots"]["@docouno/env@0.5.0"] = json!({});
        write_json(&prepared.join(&manifest_name), &new_manifest).unwrap();
        write_json(&prepared.join(&lock_name), &new_lock).unwrap();
        templates.insert(name, json!({"manifest":manifest_name,"lock":lock_name}));
    }
    write_json(&prepared.join("inputs.json"), &json!({
        "schemaVersion":1,"version":"0.5.0","sourceVersion":"0.4.1","sourceEnvironmentVersion":"0.4.1",
        "environment":{"name":"@docouno/env","version":"0.5.0","archive":"npm/docouno-env-0.5.0.tgz","integrity":env_integrity},
        "templates":templates,"sources":file_map(&root).unwrap(),"files":file_map(&prepared).unwrap()
    })).unwrap();
    (root, prepared)
}

fn refresh_file(receipt: &mut Value, prepared: &Path, name: &str) {
    receipt["files"][name]["sha256"] = json!(digest(fs::read(prepared.join(name)).unwrap()));
    write_json(&prepared.join("inputs.json"), receipt).unwrap();
}

#[test]
fn npm_preparation_is_bound_to_the_complete_actual_source_and_output_file_sets() {
    let temp = TempDir::new().unwrap();
    let (root, prepared) = prepared_fixture(temp.path());
    verify_npm_inputs(&root, &prepared).unwrap();
    fs::write(root.join("packages/env/README.md"), "changed API").unwrap();
    assert!(
        verify_npm_inputs(&root, &prepared)
            .unwrap_err()
            .to_string()
            .contains("Npm source file set")
    );
    fs::write(root.join("packages/env/README.md"), "Environment API\n").unwrap();
    fs::write(prepared.join("extra"), "unlisted").unwrap();
    assert!(
        verify_npm_inputs(&root, &prepared)
            .unwrap_err()
            .to_string()
            .contains("prepared file set")
    );
}

#[test]
fn a_self_consistent_receipt_cannot_reselect_external_template_artifacts() {
    let temp = TempDir::new().unwrap();
    let (root, prepared) = prepared_fixture(temp.path());
    let name = "templates/node-api/pnpm-lock.yaml";
    let mut lock: Value = serde_json::from_slice(&fs::read(prepared.join(name)).unwrap()).unwrap();
    let external = lock["packages"]
        .as_object_mut()
        .unwrap()
        .remove("external@1.2.3")
        .unwrap();
    lock["packages"]["external@1.2.4"] = external;
    write_json(&prepared.join(name), &lock).unwrap();
    let mut receipt: Value =
        serde_json::from_slice(&fs::read(prepared.join("inputs.json")).unwrap()).unwrap();
    refresh_file(&mut receipt, &prepared, name);
    assert!(
        verify_npm_inputs(&root, &prepared)
            .unwrap_err()
            .to_string()
            .contains("Selected external template identities changed")
    );
}

#[test]
fn a_self_consistent_receipt_cannot_reselect_edges_or_peer_contexts() {
    for corruption in [
        "edge",
        "context",
        "optional-field",
        "optional-flag",
        "package-metadata",
    ] {
        let temp = TempDir::new().unwrap();
        let (root, prepared) = prepared_fixture(temp.path());
        let name = "templates/node-api/pnpm-lock.yaml";
        for directory in [&root, &prepared] {
            let mut lock: Value = read_json(&directory.join(name)).unwrap();
            for version in ["1.0.0", "2.0.0"] {
                let key = format!("leaf@{version}");
                lock["packages"][&key] =
                    json!({"resolution":{"integrity":sri(version.as_bytes())}});
                lock["snapshots"][&key] = json!({});
                lock["snapshots"][format!("external@1.2.3(leaf@{version})")] =
                    json!({"dependencies":{"leaf":version}});
            }
            lock["snapshots"]
                .as_object_mut()
                .unwrap()
                .remove("external@1.2.3");
            lock["importers"]["."]["devDependencies"]["external"]["version"] =
                json!("1.2.3(leaf@1.0.0)");
            write_json(&directory.join(name), &lock).unwrap();
        }
        let mut receipt: Value = read_json(&prepared.join("inputs.json")).unwrap();
        receipt["sources"] = json!(file_map(&root).unwrap());
        refresh_file(&mut receipt, &prepared, name);
        verify_npm_inputs(&root, &prepared).unwrap();
        let mut changed: Value = read_json(&prepared.join(name)).unwrap();
        let first = "external@1.2.3(leaf@1.0.0)";
        match corruption {
            "edge" => changed["snapshots"][first]["dependencies"]["leaf"] = json!("2.0.0"),
            "context" => {
                changed["importers"]["."]["devDependencies"]["external"]["version"] =
                    json!("1.2.3(leaf@2.0.0)")
            }
            "optional-field" => {
                changed["snapshots"][first]["optionalDependencies"] =
                    changed["snapshots"][first]["dependencies"].clone();
                changed["snapshots"][first]
                    .as_object_mut()
                    .unwrap()
                    .remove("dependencies");
            }
            "package-metadata" => changed["packages"]["leaf@1.0.0"]["os"] = json!(["darwin"]),
            _ => changed["snapshots"][first]["optional"] = json!(true),
        }
        write_json(&prepared.join(name), &changed).unwrap();
        refresh_file(&mut receipt, &prepared, name);
        assert!(
            verify_npm_inputs(&root, &prepared)
                .unwrap_err()
                .to_string()
                .contains(if corruption == "package-metadata" {
                    "Selected template package metadata changed"
                } else {
                    "Selected template graph changed"
                }),
            "{corruption}"
        );
    }
}

#[test]
fn missing_sri_or_root_and_fixture_urls_are_rejected_without_panicking() {
    for corruption in ["integrity", "root", "fixture-url"] {
        let temp = TempDir::new().unwrap();
        let (root, prepared) = prepared_fixture(temp.path());
        let name = "templates/react-web/pnpm-lock.yaml";
        let mut lock: Value =
            serde_json::from_slice(&fs::read(prepared.join(name)).unwrap()).unwrap();
        match corruption {
            "integrity" => {
                lock["packages"]["external@1.2.3"]["resolution"]
                    .as_object_mut()
                    .unwrap()
                    .remove("integrity");
            }
            "root" => {
                lock["importers"].as_object_mut().unwrap().remove(".");
            }
            _ => {
                lock["packages"]["external@1.2.3"]["resolution"]["tarball"] =
                    json!("http://127.0.0.1:1234/external.tgz");
            }
        }
        write_json(&prepared.join(name), &lock).unwrap();
        let mut receipt: Value =
            serde_json::from_slice(&fs::read(prepared.join("inputs.json")).unwrap()).unwrap();
        refresh_file(&mut receipt, &prepared, name);
        assert!(verify_npm_inputs(&root, &prepared).is_err(), "{corruption}");
    }
}

#[test]
fn npm_pack_metadata_must_describe_the_one_actual_archive() {
    let temp = TempDir::new().unwrap();
    let bytes = b"the actual native npm pack output";
    fs::write(temp.path().join("docouno-devkit-0.3.0.tgz"), bytes).unwrap();
    let report = json!([{"name":"@docouno/devkit","version":"0.3.0","filename":"docouno-devkit-0.3.0.tgz","size":bytes.len(),"integrity":sri(bytes),"shasum":format!("{:x}",Sha1::digest(bytes))}]);
    let valid = serde_json::to_vec(&report).unwrap();
    assert_eq!(
        verify_npm_pack(&valid, temp.path(), "@docouno/devkit", "0.3.0")
            .unwrap()
            .archive,
        "docouno-devkit-0.3.0.tgz"
    );
    for field in ["size", "integrity", "shasum", "filename", "name"] {
        let mut corrupt = report.clone();
        corrupt[0][field] = if field == "size" {
            json!(1)
        } else {
            json!("conflicting")
        };
        assert!(
            verify_npm_pack(
                &serde_json::to_vec(&corrupt).unwrap(),
                temp.path(),
                "@docouno/devkit",
                "0.3.0"
            )
            .is_err(),
            "{field}"
        );
    }
    fs::write(temp.path().join("extra"), "unexpected").unwrap();
    assert!(verify_npm_pack(&valid, temp.path(), "@docouno/devkit", "0.3.0").is_err());
}

#[test]
fn an_existing_output_is_never_replaced_even_when_it_is_empty() {
    let temp = TempDir::new().unwrap();
    let output = temp.path().join("existing");
    fs::create_dir(&output).unwrap();
    let options = AssemblyOptions {
        root: temp.path().to_owned(),
        target_inputs: Vec::new(),
        npm_inputs: temp.path().join("missing-inputs"),
        output: output.clone(),
        source_commit: String::new(),
        source_tree: String::new(),
        build_run: String::new(),
        mode: devkit::assembler::AssemblyMode::Local,
    };
    let runner = ProcessRunner::new().unwrap();
    assert!(
        assemble(&options, &runner)
            .unwrap_err()
            .to_string()
            .contains("must be a new directory")
    );
    assert!(output.is_dir());
    assert_eq!(fs::read_dir(output).unwrap().count(), 0);
}

#[test]
fn rust_template_channel_must_match_the_release_pin_in_the_actual_toml_value() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("templates/rust-api/rust-toolchain.toml");
    write_atomic(&path, b"[toolchain]\nchannel = '1.99.1'\n", 0o644).unwrap();
    verify_rust_template(temp.path(), "1.99.1").unwrap();
    write_atomic(
        &path,
        b"# channel = '1.99.1'\n[toolchain]\nchannel = '0.0.0'\n",
        0o644,
    )
    .unwrap();
    assert!(
        verify_rust_template(temp.path(), "1.99.1")
            .unwrap_err()
            .to_string()
            .contains("Rust template channel differs")
    );
}

#[test]
fn runtime_notices_refuse_empty_missing_and_symlinked_sources_before_staging() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().unwrap();
    let (root, _) = prepared_fixture(temp.path());
    for (name, value) in [
        ("README.md", "Fixture source\n"),
        ("Cargo.toml", "[workspace]\nmembers=[]\n"),
        ("Cargo.lock", "version=4\n"),
        ("package.json", "{}\n"),
        ("toolchains.env", "DEVKIT_RUST_VERSION=1.99.1\n"),
        (
            "docs/migrations/index.json",
            "{\"schemaVersion\":1,\"migrations\":[]}\n",
        ),
    ] {
        write_atomic(&root.join(name), value.as_bytes(), 0o644).unwrap();
    }
    let outside = temp.path().join("outside-notice");
    fs::write(&outside, "Outside sentinel\n").unwrap();
    let options = AssemblyOptions {
        root: root.clone(),
        target_inputs: Vec::new(),
        npm_inputs: root.join("missing-prepared-inputs"),
        output: temp.path().join("unpublished-set"),
        source_commit: String::new(),
        source_tree: String::new(),
        build_run: String::new(),
        mode: devkit::assembler::AssemblyMode::Local,
    };
    let runner = ProcessRunner::new().unwrap();
    let names = ["Apache-2.0.txt", "moby-profiles.NOTICE"];
    for name in names {
        write_atomic(
            &root.join(format!("runtime/licenses/{name}")),
            b"Actual runtime notice\n",
            0o644,
        )
        .unwrap();
    }
    verify_runtime_notices(&root).unwrap();
    for name in names {
        let path = root.join(format!("runtime/licenses/{name}"));
        fs::write(&path, b"").unwrap();
        assert!(
            assemble(&options, &runner)
                .unwrap_err()
                .to_string()
                .contains("Runtime notice is empty"),
            "the public assembler must check source notices before downstream inputs or staging"
        );
        assert!(!options.output.exists());
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".devkit-build-set-")
        }));
        fs::remove_file(&path).unwrap();
        assert!(verify_runtime_notices(&root).is_err());
        assert!(assemble(&options, &runner).is_err());
        symlink(&outside, &path).unwrap();
        assert!(verify_runtime_notices(&root).is_err());
        assert!(assemble(&options, &runner).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "Outside sentinel\n");
        fs::remove_file(&path).unwrap();
        write_atomic(&path, b"Actual runtime notice\n", 0o644).unwrap();
    }
    verify_runtime_notices(&root).unwrap();
}

#[test]
#[ignore = "requires Git; run the compiled static test executable on the Linux author host"]
fn strict_source_authority_detects_edits_hidden_by_git_assume_unchanged() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let git = |args: &[&str]| fixture_git(root, args);
    git(&["init", "--quiet", "--initial-branch=main"]);
    source_checkout_fixture(root);
    fs::write(root.join("source.rs"), "original source\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "fixture"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let tree = git(&["rev-parse", "HEAD^{tree}"]);
    let runner = ProcessRunner::new().unwrap();
    verify_source_checkout(root, &commit, &tree, &runner).unwrap();
    git(&["update-index", "--assume-unchanged", "source.rs"]);
    fs::write(root.join("source.rs"), "different source\n").unwrap();
    assert!(git(&["status", "--porcelain", "--untracked-files=all"]).is_empty());
    assert!(
        verify_source_checkout(root, &commit, &tree, &runner)
            .unwrap_err()
            .to_string()
            .contains("differ from HEAD")
    );
}

fn source_checkout_fixture(root: &Path) {
    for name in [
        "VERSION",
        "LICENSE",
        "README.md",
        "toolchains.env",
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "runtime/fixture",
        "templates/rust-api/fixture",
        "packages/env/fixture",
        "docs/migrations/index.json",
    ] {
        write_atomic(&root.join(name), b"tracked source\n", 0o644).unwrap();
    }
}

fn fixture_git(root: &Path, args: &[&str]) -> String {
    let mut command = std::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
    let output = command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
#[ignore = "requires Git; run in the pinned author builder"]
fn source_authority_ignores_ambient_git_controls_binds_membership_and_accepts_real_worktrees() {
    const CHILD: &str = "DEVKIT_SOURCE_AUTHORITY_TEST";
    if let Some(raw) = std::env::var_os(CHILD) {
        let case: Value = serde_json::from_str(raw.to_str().unwrap()).unwrap();
        let root = Path::new(case["root"].as_str().unwrap());
        let runner = ProcessRunner::new().unwrap();
        let result = verify_source_checkout(
            root,
            case["commit"].as_str().unwrap(),
            case["tree"].as_str().unwrap(),
            &runner,
        );
        assert_eq!(
            result.is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{result:?}"
        );
        return;
    }
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("actual");
    fs::create_dir(&root).unwrap();
    fixture_git(&root, &["init", "--quiet", "--initial-branch=main"]);
    source_checkout_fixture(&root);
    fs::write(
        root.join(".gitignore"),
        "templates/rust-api/ignored-extra.txt\n",
    )
    .unwrap();
    fixture_git(&root, &["add", "."]);
    fixture_git(&root, &["commit", "--quiet", "-m", "fixture"]);
    let commit = fixture_git(&root, &["rev-parse", "HEAD"]);
    let tree = fixture_git(&root, &["rev-parse", "HEAD^{tree}"]);
    let runner = ProcessRunner::new().unwrap();
    verify_source_checkout(&root, &commit, &tree, &runner).unwrap();

    let shadow = temp.path().join("shadow");
    source_checkout_fixture(&shadow);
    fs::write(
        shadow.join(".gitignore"),
        "templates/rust-api/ignored-extra.txt\n",
    )
    .unwrap();
    fs::write(
        shadow.join("templates/rust-api/untracked-extra.txt"),
        "unattributed payload\n",
    )
    .unwrap();
    assert!(!shadow.join(".git").exists());
    for (directory, accepted) in [(&root, true), (&shadow, false)] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "source_authority_ignores_ambient_git_controls_binds_membership_and_accepts_real_worktrees", "--ignored", "--nocapture"])
            .env(CHILD, json!({"root":directory,"commit":commit,"tree":tree,"accepted":accepted}).to_string())
            .env("GIT_DIR", root.join(".git"))
            .env("GIT_WORK_TREE", &root)
            .env("GIT_INDEX_FILE", temp.path().join("nonexistent-index"))
            .env("GIT_OBJECT_DIRECTORY", temp.path().join("nonexistent-objects"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.worktree")
            .env("GIT_CONFIG_VALUE_0", &root)
            .status().unwrap();
        assert!(
            status.success(),
            "inherited controls must not select the source checkout"
        );
    }
    assert!(!temp.path().join("nonexistent-index").exists());
    let worktree = temp.path().join("linked");
    fixture_git(
        &root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().unwrap(),
            &commit,
        ],
    );
    assert!(worktree.join(".git").is_file());
    verify_source_checkout(&worktree, &commit, &tree, &runner).unwrap();
    assert!(
        verify_source_checkout(&root.join("templates"), &commit, &tree, &runner)
            .unwrap_err()
            .to_string()
            .contains("checkout root differs")
    );
    fs::write(
        root.join("templates/rust-api/ignored-extra.txt"),
        "ignored payload\n",
    )
    .unwrap();
    assert!(fixture_git(&root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
    assert!(
        verify_source_checkout(&root, &commit, &tree, &runner)
            .unwrap_err()
            .to_string()
            .contains("absent from HEAD")
    );
}

#[test]
fn assembler_requires_the_external_contribution_digest_before_downstream_inputs_or_staging() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    source_checkout_fixture(&root);
    for name in ["Apache-2.0.txt", "moby-profiles.NOTICE"] {
        write_atomic(
            &root.join(format!("runtime/licenses/{name}")),
            b"Runtime license\n",
            0o644,
        )
        .unwrap();
    }
    fs::write(
        root.join("toolchains.env"),
        include_str!("../../../toolchains.env"),
    )
    .unwrap();
    let notices = temp.path().join("notices");
    fs::create_dir(&notices).unwrap();
    let receipt_path = notices.join("producer.json");
    fs::write(&receipt_path, b"{\"producer\":\"original\"}\n").unwrap();
    let producer_digest = digest(fs::read(&receipt_path).unwrap());
    fs::write(&receipt_path, b"changed and no longer valid JSON\n").unwrap();
    let mut options = AssemblyOptions {
        root,
        target_inputs: vec![TargetInput {
            target: devkit::distribution::Target::LinuxX64,
            directory: notices,
            sha256: producer_digest,
        }],
        npm_inputs: temp.path().join("missing-inputs"),
        output: temp.path().join("unpublished"),
        source_commit: String::new(),
        source_tree: String::new(),
        build_run: String::new(),
        mode: devkit::assembler::AssemblyMode::Local,
    };
    let runner = ProcessRunner::new().unwrap();
    assert!(
        assemble(&options, &runner)
            .unwrap_err()
            .to_string()
            .contains("producer digest")
    );
    options.target_inputs[0].sha256.clear();
    assert!(
        assemble(&options, &runner)
            .unwrap_err()
            .to_string()
            .contains("Invalid producer contribution digest")
    );
    assert!(!options.output.exists());
    assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".devkit-build-set-")
    }));
}

#[test]
fn migration_index_validates_unique_ids_semver_bounds_profiles_and_markdown_anchors() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write_atomic(
        &root.join("docs/migrations/0.4.0.md"),
        b"# Migration to 0.4.0\n\n## Glob Scripts and Single-Pass Checks\n\nDetails here.\n\n```md\n## Fake Heading Inside Fence\n```\n",
        0o644,
    )
    .unwrap();
    let valid_entry = json!({
        "id": "glob-scripts-040",
        "version": "0.4.0",
        "severity": "recommended",
        "title": "Adopt glob lint/format scripts and single-pass check",
        "summary": "Replace directory scripts and duplicate npm test in check for projects initialized on 0.3.x.",
        "profiles": ["node", "web", "electron", "react-library"],
        "docPath": "docs/migrations/0.4.0.md",
        "anchor": "glob-scripts-and-single-pass-checks"
    });
    write_json(
        &root.join("docs/migrations/index.json"),
        &json!({ "schemaVersion": 1, "migrations": [valid_entry.clone()] }),
    )
    .unwrap();
    let parsed = verify_migration_index(root, "0.5.0").unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].id, "glob-scripts-040");

    // Anchor inside fenced code block must NOT satisfy heading validation.
    let mut fenced_anchor = valid_entry.clone();
    fenced_anchor["anchor"] = json!("fake-heading-inside-fence");
    write_json(
        &root.join("docs/migrations/index.json"),
        &json!({ "schemaVersion": 1, "migrations": [fenced_anchor] }),
    )
    .unwrap();
    assert!(
        verify_migration_index(root, "0.5.0")
            .unwrap_err()
            .to_string()
            .contains("anchor #fake-heading-inside-fence not found")
    );

    // Version newer than release is rejected.
    let mut future_ver = valid_entry.clone();
    future_ver["version"] = json!("0.6.0");
    write_json(
        &root.join("docs/migrations/index.json"),
        &json!({ "schemaVersion": 1, "migrations": [future_ver] }),
    )
    .unwrap();
    assert!(
        verify_migration_index(root, "0.5.0")
            .unwrap_err()
            .to_string()
            .contains("newer than release")
    );

    // Duplicate IDs are rejected.
    write_json(
        &root.join("docs/migrations/index.json"),
        &json!({ "schemaVersion": 1, "migrations": [valid_entry.clone(), valid_entry] }),
    )
    .unwrap();
    assert!(
        verify_migration_index(root, "0.5.0")
            .unwrap_err()
            .to_string()
            .contains("Duplicate migration id")
    );

    // Check all published migration entries against the live release and documentation.
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo_entries = verify_migration_index(&repo_root, env!("CARGO_PKG_VERSION")).unwrap();
    let ids: Vec<_> = repo_entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "pnpm-workspaces",
            "development-stands",
            "make-targets-and-worktrunk",
            "portable-linux-checkouts"
        ]
    );
    assert!(repo_entries[2].profiles.is_empty());
}

#[test]
fn npm_launcher_follows_relative_links_and_executes_only_the_pinned_host_file() {
    use devkit::{delivery::render_npm_launcher, distribution::Target};
    use std::{
        os::unix::fs::symlink,
        process::{Command, Stdio},
    };
    let temp = TempDir::new().unwrap();
    let package = temp.path().join("modules with spaces/@docouno/devkit");
    let target = Target::host().unwrap();
    let contents = b"#!/bin/sh\nprintf '%s|%s|%s\\n' \"$$\" \"$1\" \"$2\"\nexit 42\n";
    let pins = BTreeMap::from([(target.triple().to_string(), digest(contents))]);
    let launcher = package.join("bin/devkit");
    write_atomic(
        &launcher,
        render_npm_launcher(&pins).unwrap().as_bytes(),
        0o755,
    )
    .unwrap();
    let engine = package.join(target.payload_name());
    write_atomic(&engine, contents, 0o700).unwrap();
    let shim = temp.path().join("modules with spaces/.bin/devkit");
    fs::create_dir_all(shim.parent().unwrap()).unwrap();
    symlink("../@docouno/devkit/bin/devkit", &shim).unwrap();
    let entry = temp.path().join("npm");
    symlink("modules with spaces/.bin/devkit", &entry).unwrap();
    let p = Command::new(&entry)
        .args(["one arg", ""])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = p.id();
    let result = p.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(42));
    assert_eq!(result.stdout, format!("{pid}|one arg|\n").as_bytes());
    write_atomic(&engine, b"#!/bin/sh\nprintf 'changed'\n", 0o700).unwrap();
    let result = Command::new(entry).output().unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("checksum differs"));
}
