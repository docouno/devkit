use devkit::{
    files::*,
    state::{MANIFEST_PATH, PENDING_PATH},
    transaction::PreparedTransaction,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("CI", "true")
        .env("DEVKIT_CACHE_DIR", root.join("invalid-cache-file"))
        .output()
        .unwrap()
}
fn decoded(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON stdout: {e}; {:?}", output))
}

#[test]
fn help_version_and_catalog_do_not_require_home_sdk_cache_or_docker() {
    let temp = TempDir::new().unwrap();
    fs::write(
        temp.path().join("invalid-cache-file"),
        "owner cache sentinel",
    )
    .unwrap();
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["catalog"],
        vec!["version", "--json"],
        vec!["catalog", "--json"],
    ] {
        let output = invoke(temp.path(), &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args.contains(&"--json") {
            assert_eq!(decoded(&output)["schemaVersion"], 1);
            assert_eq!(decoded(&output)["ok"], true);
        }
    }
    assert_eq!(
        fs::read_to_string(temp.path().join("invalid-cache-file")).unwrap(),
        "owner cache sentinel"
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn incomplete_ci_and_conflicting_prompt_modes_fail_with_one_json_result_before_effects() {
    let temp = TempDir::new().unwrap();
    for args in [
        vec!["--json"],
        vec!["init", "--json"],
        vec!["init", "target", "--json"],
        vec!["add-app", "--json"],
        vec!["update", "--json"],
        vec!["init", "--interactive", "--json"],
        vec!["unknown", "--json"],
    ] {
        let output = invoke(temp.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        let value = decoded(&output);
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["ok"], false);
        assert!(value["error"]["message"].is_string());
        assert!(
            output.stderr.is_empty(),
            "JSON protocol error unexpectedly mixed into stderr"
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0, "{args:?}");
    }
}

#[test]
fn json_streaming_refusal_happens_before_runtime_or_process_execution() {
    let temp = TempDir::new().unwrap();
    for args in [
        vec!["run", "setup", "--json"],
        vec!["exec", "--json", "--", "would-run"],
        vec!["task", "doctor", "--json"],
        vec!["internal", "fetch", "--json"],
    ] {
        let output = invoke(temp.path(), &args);
        assert!(!output.status.success());
        assert_eq!(decoded(&output)["error"]["code"], "UNSUPPORTED_OUTPUT_MODE");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    let output = invoke(
        temp.path(),
        &[
            "exec",
            "--component",
            "apps/api",
            "--",
            "--json",
            "--cache-dir",
            "literal-child-value",
        ],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("UNSUPPORTED_OUTPUT_MODE"));
}

fn pending(root: &Path, missing: bool) -> (Vec<u8>, String) {
    let before = b"{\"known-before\":true}\n".to_vec();
    write_atomic(&root.join(MANIFEST_PATH), &before, 0o644).unwrap();
    let staged = root.join("next");
    fs::create_dir(&staged).unwrap();
    if !missing {
        write_atomic(
            &staged.join(MANIFEST_PATH),
            b"{known unparseable after",
            0o644,
        )
        .unwrap();
    }
    let stage = format!(".devkit/state/update-{}", uuid::Uuid::new_v4());
    let tx = PreparedTransaction::prepare(
        root,
        &stage,
        &staged,
        &BTreeSet::from([MANIFEST_PATH.to_owned()]),
        Vec::new(),
    )
    .unwrap();
    write_json(&root.join(PENDING_PATH), &tx.journal).unwrap();
    if missing {
        fs::remove_file(root.join(MANIFEST_PATH)).unwrap();
    } else {
        write_atomic(
            &root.join(MANIFEST_PATH),
            b"{known unparseable after",
            0o644,
        )
        .unwrap();
    }
    fs::remove_dir_all(staged).unwrap();
    (before, stage)
}

#[test]
fn recover_uses_known_journal_bytes_before_manifest_cache_or_sdk_admission() {
    for missing in [false, true] {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let (before, stage) = pending(root, missing);
        fs::write(root.join("invalid-cache-file"), "owner cache sentinel").unwrap();
        let output = invoke(root, &["recover", ".", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(decoded(&output)["result"]["recovered"], true);
        assert_eq!(fs::read(root.join(MANIFEST_PATH)).unwrap(), before);
        assert!(!root.join(PENDING_PATH).exists());
        assert!(!root.join(stage).exists());
        assert_eq!(
            fs::read_to_string(root.join("invalid-cache-file")).unwrap(),
            "owner cache sentinel"
        );
    }
}

#[test]
fn unknown_recovery_conflict_does_not_roll_back_anything() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (before, stage) = pending(root, false);
    write_atomic(&root.join(MANIFEST_PATH), b"unknown owner edit", 0o644).unwrap();
    let pending_before = fs::read(root.join(PENDING_PATH)).unwrap();
    let output = invoke(root, &["recover", ".", "--json"]);
    assert!(!output.status.success());
    assert_eq!(decoded(&output)["ok"], false);
    assert_eq!(
        fs::read(root.join(MANIFEST_PATH)).unwrap(),
        b"unknown owner edit"
    );
    assert_eq!(fs::read(root.join(PENDING_PATH)).unwrap(), pending_before);
    assert_eq!(fs::read(root.join(stage).join("backup/0")).unwrap(), before);
}
