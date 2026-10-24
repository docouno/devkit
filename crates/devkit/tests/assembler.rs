use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    assembler::{
        AssemblyOptions, assemble, verify_npm_inputs, verify_npm_pack, verify_runtime_notices,
        verify_rust_template, verify_source_checkout,
    },
    files::{digest, file_map, write_atomic, write_json},
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
    write_atomic(&root.join("VERSION"), b"0.2.1\n", 0o644).unwrap();
    write_atomic(&root.join("LICENSE"), b"Original MIT license\n", 0o644).unwrap();
    write_json(
        &root.join("packages/env/package.json"),
        &json!({"name":"@docouno/env","version":"0.2.1"}),
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
        &prepared.join("npm/docouno-env-0.3.0.tgz"),
        env_bytes,
        0o644,
    )
    .unwrap();
    let external = json!({"version":"1.2.3","resolved":"https://registry.npmjs.org/external/-/external-1.2.3.tgz","integrity":sri(b"selected external artifact"),"dev":true});
    let mut templates = BTreeMap::new();
    for name in ["node-api", "react-web", "electron-react", "react-client"] {
        let manifest = json!({"name":format!("fixture-{name}"),"version":"1.0.0","devDependencies":{"@docouno/env":"0.2.1","external":"1.2.3"}});
        let lock = json!({"lockfileVersion":3,"packages":{"":manifest,"node_modules/@docouno/env":{"version":"0.2.1","resolved":"https://registry.npmjs.org/@docouno/env/-/env-0.2.1.tgz","integrity":sri(b"old environment")},"node_modules/external":external}});
        let manifest_name = format!("templates/{name}/package.json");
        let lock_name = format!("templates/{name}/package-lock.json");
        write_json(&root.join(&manifest_name), &manifest).unwrap();
        write_json(&root.join(&lock_name), &lock).unwrap();
        let mut new_manifest = manifest;
        new_manifest["devDependencies"]["@docouno/env"] = json!("0.3.0");
        let mut new_lock = lock;
        new_lock["packages"][""] = new_manifest.clone();
        new_lock["packages"]["node_modules/@docouno/env"] = json!({"version":"0.3.0","resolved":"https://registry.npmjs.org/@docouno/env/-/env-0.3.0.tgz","integrity":env_integrity});
        write_json(&prepared.join(&manifest_name), &new_manifest).unwrap();
        write_json(&prepared.join(&lock_name), &new_lock).unwrap();
        templates.insert(name, json!({"manifest":manifest_name,"lock":lock_name}));
    }
    write_json(&prepared.join("inputs.json"), &json!({
        "schemaVersion":1,"version":"0.3.0","sourceVersion":"0.2.1","sourceEnvironmentVersion":"0.2.1",
        "environment":{"name":"@docouno/env","version":"0.3.0","archive":"npm/docouno-env-0.3.0.tgz","integrity":env_integrity},
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
    let name = "templates/node-api/package-lock.json";
    let mut lock: Value = serde_json::from_slice(&fs::read(prepared.join(name)).unwrap()).unwrap();
    lock["packages"]["node_modules/external"]["version"] = json!("1.2.4");
    lock["packages"]["node_modules/external"]["resolved"] =
        json!("https://registry.npmjs.org/external/-/external-1.2.4.tgz");
    write_json(&prepared.join(name), &lock).unwrap();
    let mut receipt: Value =
        serde_json::from_slice(&fs::read(prepared.join("inputs.json")).unwrap()).unwrap();
    refresh_file(&mut receipt, &prepared, name);
    assert!(
        verify_npm_inputs(&root, &prepared)
            .unwrap_err()
            .to_string()
            .contains("Selected external template identity changed")
    );
}

#[test]
fn missing_sri_or_root_and_fixture_urls_are_rejected_without_panicking() {
    for corruption in ["integrity", "root", "fixture-url"] {
        let temp = TempDir::new().unwrap();
        let (root, prepared) = prepared_fixture(temp.path());
        let name = "templates/react-web/package-lock.json";
        let mut lock: Value =
            serde_json::from_slice(&fs::read(prepared.join(name)).unwrap()).unwrap();
        match corruption {
            "integrity" => {
                lock["packages"]["node_modules/external"]
                    .as_object_mut()
                    .unwrap()
                    .remove("integrity");
            }
            "root" => {
                lock["packages"].as_object_mut().unwrap().remove("");
            }
            _ => {
                lock["packages"]["node_modules/external"]["resolved"] =
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
        engine: temp.path().join("missing-engine"),
        npm_inputs: temp.path().join("missing-inputs"),
        output: output.clone(),
        notices: temp.path().join("missing-notices"),
        notice_set_sha256: "a".repeat(64),
        source_commit: String::new(),
        source_tree: String::new(),
        build_run: String::new(),
        local: true,
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
    ] {
        write_atomic(&root.join(name), value.as_bytes(), 0o644).unwrap();
    }
    let outside = temp.path().join("outside-notice");
    fs::write(&outside, "Outside sentinel\n").unwrap();
    let options = AssemblyOptions {
        root: root.clone(),
        engine: root.join("missing-engine"),
        npm_inputs: root.join("missing-prepared-inputs"),
        output: temp.path().join("unpublished-set"),
        notices: root.join("missing-cargo-notices"),
        notice_set_sha256: "a".repeat(64),
        source_commit: String::new(),
        source_tree: String::new(),
        build_run: String::new(),
        local: true,
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
fn assembler_requires_the_external_notice_digest_before_downstream_inputs_or_staging() {
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
    fs::write(root.join("toolchains.env"), format!("DEVKIT_NODE_IMAGE=node:24@sha256:{}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\n", "a".repeat(64))).unwrap();
    let notices = temp.path().join("notices");
    fs::create_dir(&notices).unwrap();
    let receipt_path = notices.join("notice-set.json");
    fs::write(&receipt_path, b"{\"producer\":\"original\"}\n").unwrap();
    let producer_digest = digest(fs::read(&receipt_path).unwrap());
    fs::write(&receipt_path, b"changed and no longer valid JSON\n").unwrap();
    let mut options = AssemblyOptions {
        root,
        engine: temp.path().join("missing-engine"),
        npm_inputs: temp.path().join("missing-inputs"),
        notices,
        notice_set_sha256: producer_digest,
        output: temp.path().join("unpublished"),
        source_commit: String::new(),
        source_tree: String::new(),
        build_run: String::new(),
        local: true,
    };
    let runner = ProcessRunner::new().unwrap();
    assert!(
        assemble(&options, &runner)
            .unwrap_err()
            .to_string()
            .contains("producer notice-set digest")
    );
    options.notice_set_sha256.clear();
    assert!(
        assemble(&options, &runner)
            .unwrap_err()
            .to_string()
            .contains("Invalid producer notice-set digest")
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
