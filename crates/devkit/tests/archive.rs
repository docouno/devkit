use devkit::{
    archive::{create_archive, extract_archive, publish_new_directory},
    files::*,
};
use flate2::{Compression, write::GzEncoder};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
};
use tempfile::TempDir;

fn write_archive(path: &Path, entries: &[(&str, tar::EntryType, u32, &[u8])]) {
    let output = GzEncoder::new(fs::File::create(path).unwrap(), Compression::default());
    let mut tar = tar::Builder::new(output);
    for (name, kind, mode, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_mode(*mode);
        header.set_size(body.len() as u64);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, *body).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn canonical_archive_roundtrip_is_deterministic_and_preserves_modes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("source")).unwrap();
    write_atomic(&root.join("source/a.txt"), b"actual regular bytes", 0o644).unwrap();
    write_atomic(
        &root.join("source/nested/program"),
        b"executable bytes",
        0o755,
    )
    .unwrap();
    create_archive(
        &root.join("source"),
        "devkit-0.3.0",
        &root.join("first.tgz"),
    )
    .unwrap();
    create_archive(
        &root.join("source"),
        "devkit-0.3.0",
        &root.join("second.tgz"),
    )
    .unwrap();
    assert_eq!(
        fs::read(root.join("first.tgz")).unwrap(),
        fs::read(root.join("second.tgz")).unwrap()
    );
    extract_archive(&root.join("first.tgz"), &root.join("out"), "devkit-0.3.0").unwrap();
    assert_eq!(
        file_map(&root.join("source")).unwrap(),
        file_map(&root.join("out")).unwrap()
    );
}

#[test]
fn traversal_extra_roots_duplicates_links_devices_and_noncanonical_modes_never_publish() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let bad = [
        ("../outside", tar::EntryType::Regular, 0o644),
        ("/absolute", tar::EntryType::Regular, 0o644),
        ("devkit-0.3.0/../outside", tar::EntryType::Regular, 0o644),
        ("different/file", tar::EntryType::Regular, 0o644),
        ("devkit-0.3.0/link", tar::EntryType::Symlink, 0o755),
        ("devkit-0.3.0/link", tar::EntryType::Link, 0o644),
        ("devkit-0.3.0/fifo", tar::EntryType::Fifo, 0o644),
        ("devkit-0.3.0/device", tar::EntryType::Char, 0o644),
        ("devkit-0.3.0/file", tar::EntryType::Regular, 0o600),
        ("devkit-0.3.0/file", tar::EntryType::Regular, 0o4755),
    ];
    fs::write(root.join("outside"), "owner sentinel").unwrap();
    for (index, entry) in bad.iter().enumerate() {
        let archive = root.join(format!("{index}.tgz"));
        write_archive(&archive, &[(entry.0, entry.1, entry.2, b"")]);
        let output = root.join(format!("output-{index}"));
        assert!(
            extract_archive(&archive, &output, "devkit-0.3.0").is_err(),
            "{:?}",
            entry.0
        );
        assert!(!output.exists());
        assert_eq!(
            fs::read_to_string(root.join("outside")).unwrap(),
            "owner sentinel"
        );
    }
    let archive = root.join("duplicate.tgz");
    write_archive(
        &archive,
        &[
            (
                "devkit-0.3.0/file",
                tar::EntryType::Regular,
                0o644,
                b"first",
            ),
            (
                "devkit-0.3.0/file",
                tar::EntryType::Regular,
                0o644,
                b"second",
            ),
        ],
    );
    assert!(
        extract_archive(&archive, &root.join("duplicates"), "devkit-0.3.0")
            .unwrap_err()
            .to_string()
            .contains("Duplicate")
    );
}

#[test]
fn truncated_crc_and_trailing_tar_data_do_not_leave_partial_installations() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let valid = root.join("valid.tgz");
    write_archive(
        &valid,
        &[(
            "devkit-0.3.0/file",
            tar::EntryType::Regular,
            0o644,
            b"complete",
        )],
    );
    let bytes = fs::read(&valid).unwrap();
    for cut in [1, bytes.len() / 2, bytes.len() - 1] {
        let path = root.join(format!("cut-{cut}.tgz"));
        fs::write(&path, &bytes[..cut]).unwrap();
        let output = root.join(format!("cut-{cut}"));
        assert!(extract_archive(&path, &output, "devkit-0.3.0").is_err());
        assert!(!output.exists());
    }
    let mut crc = bytes.clone();
    let n = crc.len();
    crc[n - 8] ^= 1;
    fs::write(root.join("crc.tgz"), crc).unwrap();
    assert!(extract_archive(&root.join("crc.tgz"), &root.join("crc"), "devkit-0.3.0").is_err());
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_end(&mut raw)
        .unwrap();
    raw.extend_from_slice(b"unexpected second tar");
    let mut extra = GzEncoder::new(
        fs::File::create(root.join("extra.tgz")).unwrap(),
        Compression::default(),
    );
    extra.write_all(&raw).unwrap();
    extra.finish().unwrap();
    assert!(
        extract_archive(&root.join("extra.tgz"), &root.join("extra"), "devkit-0.3.0")
            .unwrap_err()
            .to_string()
            .contains("Unexpected data")
    );
}

#[test]
fn publishing_never_replaces_an_existing_empty_directory() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("stage")).unwrap();
    fs::write(root.join("stage/sentinel"), "staged").unwrap();
    fs::create_dir(root.join("existing")).unwrap();
    assert!(publish_new_directory(&root.join("stage"), &root.join("existing")).is_err());
    assert!(root.join("stage/sentinel").exists());
    assert_eq!(fs::read_dir(root.join("existing")).unwrap().count(), 0);
}

#[test]
fn source_links_and_restrictive_modes_are_rejected_before_packaging() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("source")).unwrap();
    fs::write(root.join("source/file"), "actual").unwrap();
    fs::set_permissions(root.join("source/file"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        create_archive(
            &root.join("source"),
            "devkit-0.3.0",
            &root.join("private.tgz")
        )
        .is_err()
    );
    assert!(!root.join("private.tgz").exists());
    fs::set_permissions(root.join("source/file"), fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink("file", root.join("source/link")).unwrap();
    assert!(
        create_archive(
            &root.join("source"),
            "devkit-0.3.0",
            &root.join("symlink.tgz")
        )
        .is_err()
    );
    assert!(!root.join("symlink.tgz").exists());
}
