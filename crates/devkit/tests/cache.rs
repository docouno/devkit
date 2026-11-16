use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    cache::ReleaseCache, files::*, model::*, processes::ProcessRunner, release::admit_release,
};
use sha2::{Digest, Sha512};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;

fn minimal_static_elf() -> Vec<u8> {
    let total_len: u64 = 120;
    let mut elf = Vec::with_capacity(total_len as usize);
    elf.extend_from_slice(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&62u16.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&0x400078u64.to_le_bytes());
    elf.extend_from_slice(&64u64.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes());
    elf.extend_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&56u16.to_le_bytes());
    elf.extend_from_slice(&1u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&5u32.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&0x400000u64.to_le_bytes());
    elf.extend_from_slice(&0x400000u64.to_le_bytes());
    elf.extend_from_slice(&total_len.to_le_bytes());
    elf.extend_from_slice(&total_len.to_le_bytes());
    elf.extend_from_slice(&0x1000u64.to_le_bytes());
    elf
}

fn fixture(root: &Path, marker: &[u8]) -> String {
    fs::create_dir_all(root).unwrap();
    let archive = b"opaque known environment archive";
    write_atomic(&root.join("npm/env.tgz"), archive, 0o644).unwrap();
    let elf = minimal_static_elf();
    let elf_sha = digest(&elf);
    write_atomic(&root.join("devkit"), &elf, 0o755).unwrap();
    let recipe = "b".repeat(64);
    let toolchains = format!(
        "DEVKIT_VERSION=0.4.1\nDEVKIT_ENGINE_SHA256={elf_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24@sha256:{}\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe}\n",
        "a".repeat(64)
    );
    write_atomic(
        &root.join("runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    write_atomic(&root.join("marker"), marker, 0o644).unwrap();
    write_json(
        &root.join("release.json"),
        &Release {
            schema_version: 1,
            version: "0.4.1".into(),
            min_supported_from_version: None,
            rust_version: "1.99.1".into(),
            environment: Environment {
                name: "@docouno/env".into(),
                version: "0.4.1".into(),
                archive: "npm/env.tgz".into(),
                integrity: format!("sha512-{}", STANDARD.encode(Sha512::digest(archive))),
            },
            files: file_map(root).unwrap(),
            migrations: Vec::new(),
        },
    )
    .unwrap();
    admit_release(root).unwrap().identity
}

#[test]
fn restrictive_npm_modes_are_normalized_only_in_the_private_cache() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source with spaces");
    let id = fixture(&source, b"source");
    for (name, value) in file_map(&source).unwrap() {
        fs::set_permissions(
            source.join(name),
            fs::Permissions::from_mode(if value.mode & 0o111 == 0 {
                0o600
            } else {
                0o700
            }),
        )
        .unwrap();
    }
    let before = file_map(&source).unwrap();
    let runner = ProcessRunner::new().unwrap();
    let root = temp.path().join("cache with spaces");
    let cache = ReleaseCache::new(Some(&root), &runner).unwrap();
    let release = cache.import(&source, "0.4.1", Some(&id)).unwrap();
    assert_eq!(release.identity, id);
    assert_eq!(file_map(&source).unwrap(), before);
    assert_eq!(
        identity(&release.root.join("marker"))
            .unwrap()
            .unwrap()
            .mode,
        0o644
    );
    assert_eq!(
        identity(&release.root.join("devkit"))
            .unwrap()
            .unwrap()
            .mode,
        0o755
    );
    let cached = cache.acquire("0.4.1", Some(&id), None, true).unwrap();
    assert_eq!(cached.root, release.root);
    assert!(!cache.root.read_dir().unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".import-")
    }));
}

#[test]
fn changed_sources_and_corrupt_cache_never_succeed_or_install_a_second_identity() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let id = fixture(&source, b"source");
    let runner = ProcessRunner::new().unwrap();
    let cache = ReleaseCache::new(Some(&temp.path().join("cache")), &runner).unwrap();
    let release = cache.import(&source, "0.4.1", Some(&id)).unwrap();
    fs::write(source.join("marker"), "different source bytes").unwrap();
    assert!(cache.import(&source, "0.4.1", Some(&id)).is_err());
    admit_release(&release.root).unwrap();
    fs::write(release.root.join("marker"), "corrupt cached bytes").unwrap();
    assert!(
        cache
            .acquire("0.4.1", Some(&id), None, true)
            .unwrap_err()
            .to_string()
            .contains("Cached release integrity")
    );
    assert_eq!(
        fs::read_dir(cache.root.join("releases/0.4.1"))
            .unwrap()
            .count(),
        1
    );
    assert!(
        cache
            .acquire("0.4.0", Some(&"a".repeat(64)), None, true)
            .unwrap_err()
            .to_string()
            .contains("not cached")
    );
}

#[test]
fn fifo_or_symlink_cache_locks_refuse_without_waiting_or_mutating_the_source() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let id = fixture(&source, b"source");
    let before = file_map(&source).unwrap();
    let runner = ProcessRunner::new().unwrap();
    let root = temp.path().join("cache");
    fs::create_dir_all(root.join(".locks")).unwrap();
    let lock = root
        .join(".locks")
        .join(format!("{}.lock", digest(format!("release:0.4.1:{id}"))));
    use std::os::unix::ffi::OsStrExt;
    let encoded = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
    let cache = ReleaseCache::new(Some(&root), &runner).unwrap();
    let started = std::time::Instant::now();
    assert!(cache.import(&source, "0.4.1", Some(&id)).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    fs::remove_file(&lock).unwrap();
    fs::write(temp.path().join("sentinel"), "owner data").unwrap();
    std::os::unix::fs::symlink(temp.path().join("sentinel"), &lock).unwrap();
    assert!(cache.import(&source, "0.4.1", Some(&id)).is_err());
    assert_eq!(
        fs::read_to_string(temp.path().join("sentinel")).unwrap(),
        "owner data"
    );
    assert_eq!(file_map(&source).unwrap(), before);
    assert!(!root.join("releases").exists());
}
