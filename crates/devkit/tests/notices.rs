use devkit::{
    distribution::{TARGET, build_info},
    files::{digest, file_map, write_atomic, write_json},
    notices::{
        NoticeSet, NoticeSource, PackageNotices, read_crate_notices, resolved_packages,
        verify_notice_set,
    },
};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

#[test]
fn cargo_resolution_walk_keeps_normal_and_build_edges_without_dev_or_disconnected_targets() {
    let package = |id| json!({"id":id});
    let edge = |id, kind: Value| json!({"pkg":id,"dep_kinds":[{"kind":kind,"target":null}]});
    let node = |id, deps| json!({"id":id,"deps":deps});
    let metadata = json!({
        "workspace_members":["root"],
        "packages":[package("root"),package("normal"),package("build"),package("dev"),package("other-target")],
        "resolve":{"nodes":[node("root",vec![edge("normal",Value::Null),edge("build",json!("build")),edge("dev",json!("dev"))]),node("normal",vec![]),node("build",vec![]),node("dev",vec![]),node("other-target",vec![])]}
    });
    let (roots, packages) = resolved_packages(&metadata).unwrap();
    assert_eq!(roots, ["root"]);
    assert_eq!(
        packages
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["build", "normal", "root"]
    );
    let mut incomplete = metadata;
    incomplete["resolve"]["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    assert!(resolved_packages(&incomplete).is_err());
}

fn crate_archive(path: &Path, files: &[(&str, &str)]) {
    let gzip = GzEncoder::new(fs::File::create(path).unwrap(), Compression::default());
    let mut archive = tar::Builder::new(gzip);
    for (name, value) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(value.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("fixture-1.0.0/{name}"),
                value.as_bytes(),
            )
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap();
}

#[test]
fn notices_come_from_exact_archive_bytes_including_a_nonstandard_declared_license_file() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("fixture.crate");
    crate_archive(
        &archive,
        &[
            (
                "legal/terms.txt",
                "Copyright Fixture Authors\nActual license text\n",
            ),
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='1.0.0'\nlicense-file='legal/terms.txt'\n",
            ),
        ],
    );
    let hash = digest(fs::read(&archive).unwrap());
    let (spdx, notices) = read_crate_notices(&archive, &hash, "fixture", "1.0.0").unwrap();
    assert!(spdx.is_none());
    assert_eq!(
        notices["legal/terms.txt"],
        b"Copyright Fixture Authors\nActual license text\n"
    );
    assert!(read_crate_notices(&archive, &"a".repeat(64), "fixture", "1.0.0").is_err());
    assert!(read_crate_notices(&archive, &hash, "other", "1.0.0").is_err());
}

#[test]
fn an_spdx_label_without_actual_notice_text_does_not_produce_a_fake_bundle() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("fixture.crate");
    crate_archive(
        &archive,
        &[(
            "Cargo.toml",
            "[package]\nname='fixture'\nversion='1.0.0'\nlicense='MIT'\n",
        )],
    );
    let hash = digest(fs::read(&archive).unwrap());
    assert!(
        read_crate_notices(&archive, &hash, "fixture", "1.0.0")
            .unwrap_err()
            .to_string()
            .contains("No actual license")
    );
}

fn notice_fixture(root: &Path) -> std::path::PathBuf {
    fs::write(root.join("Cargo.lock"), format!("version=4\n[[package]]\nname='fixture'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\n", "a".repeat(64))).unwrap();
    let output = root.join("notices");
    fs::create_dir(&output).unwrap();
    for (path, text) in [
        ("NOTICE.txt", "Third-party notices\n"),
        ("cargo/fixture/notice-001.txt", "Original crate license\n"),
        (
            "rust/COPYRIGHT-library.html",
            "Original Rust std/musl copyright\n",
        ),
        ("rust/licenses/MIT.txt", "Original Rust license\n"),
    ] {
        write_atomic(&output.join(path), text.as_bytes(), 0o644).unwrap();
    }
    let source = |path: &str| NoticeSource {
        path: path.to_owned(),
        sha256: digest(fs::read(output.join(path)).unwrap()),
    };
    let id = "registry+https://github.com/rust-lang/crates.io-index#fixture@1.0.0".to_owned();
    let receipt = NoticeSet {
        schema_version: 1,
        target: TARGET.to_owned(),
        rust_version: "1.99.1".to_owned(),
        source_cargo_lock_sha256: digest(fs::read(root.join("Cargo.lock")).unwrap()),
        generator: build_info(),
        cargo_metadata_sha256: "b".repeat(64),
        roots: vec!["workspace+fixture".to_owned()],
        resolved_ids: vec![id.clone(), "workspace+fixture".to_owned()],
        packages: vec![PackageNotices {
            id,
            name: "fixture".to_owned(),
            version: "1.0.0".to_owned(),
            source: "registry+https://github.com/rust-lang/crates.io-index".to_owned(),
            license: Some("MIT".to_owned()),
            archive_sha256: "a".repeat(64),
            notices: BTreeMap::from([(
                "cargo/fixture/notice-001.txt".to_owned(),
                source("cargo/fixture/notice-001.txt"),
            )]),
        }],
        standard_library: BTreeMap::from([
            (
                "rust/COPYRIGHT-library.html".to_owned(),
                source("rust/COPYRIGHT-library.html"),
            ),
            (
                "rust/licenses/MIT.txt".to_owned(),
                source("rust/licenses/MIT.txt"),
            ),
        ]),
        files: file_map(&output).unwrap(),
    };
    write_json(&output.join("notice-set.json"), &receipt).unwrap();
    output
}

#[test]
fn bundle_verification_rejects_missing_resolved_package_even_when_remaining_files_are_self_consistent()
 {
    let temp = TempDir::new().unwrap();
    let output = notice_fixture(temp.path());
    verify_notice_set(&output, temp.path(), TARGET, "1.99.1").unwrap();
    let path = output.join("notice-set.json");
    let mut receipt: NoticeSet = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    receipt.packages.clear();
    fs::remove_file(output.join("cargo/fixture/notice-001.txt")).unwrap();
    receipt.files.remove("cargo/fixture/notice-001.txt");
    write_json(&path, &receipt).unwrap();
    assert!(
        verify_notice_set(&output, temp.path(), TARGET, "1.99.1")
            .unwrap_err()
            .to_string()
            .contains("omits a resolved")
    );
}

#[test]
fn bundle_verification_binds_bytes_lock_target_and_standard_library_files() {
    let temp = TempDir::new().unwrap();
    let output = notice_fixture(temp.path());
    assert!(
        verify_notice_set(&output, temp.path(), "aarch64-unknown-linux-musl", "1.99.1").is_err()
    );
    fs::write(output.join("cargo/fixture/notice-001.txt"), "changed").unwrap();
    assert!(verify_notice_set(&output, temp.path(), TARGET, "1.99.1").is_err());
    fs::write(
        output.join("cargo/fixture/notice-001.txt"),
        "Original crate license\n",
    )
    .unwrap();
    fs::write(temp.path().join("Cargo.lock"), "changed lock").unwrap();
    assert!(verify_notice_set(&output, temp.path(), TARGET, "1.99.1").is_err());
}

#[test]
#[ignore = "requires the pinned Rust builder, exact standard-library docs and original registry archives"]
fn actual_pinned_cargo_and_standard_library_notice_collection() {
    use devkit::{
        notices::{NoticeOptions, collect_notices},
        processes::ProcessRunner,
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let runner = ProcessRunner::new().unwrap();
    let output = runner
        .capture(std::process::Command::new("rustc").args(["--print", "sysroot"]))
        .unwrap();
    let docs = Path::new(output.text().unwrap().trim()).join("share/doc/rust");
    let temp = TempDir::new().unwrap();
    let options = NoticeOptions {
        root: root.clone(),
        cargo_home: std::env::var_os("CARGO_HOME").unwrap().into(),
        rust_docs: docs,
        output: temp.path().join("collected"),
        target: TARGET.to_owned(),
        rust_version: "1.99.1".to_owned(),
    };
    let receipt = collect_notices(&options, &runner).unwrap();
    assert!(receipt["packages"].as_array().unwrap().len() > 20);
    verify_notice_set(&options.output, &root, TARGET, "1.99.1").unwrap();
}
