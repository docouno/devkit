#[path = "support/runtime.rs"]
mod runtime_fixture;

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

#[test]
fn pnpm_cache_is_verified_and_offline_misses_never_acquire() {
    let temporary = TempDir::new().unwrap();
    let runner = ProcessRunner::new().unwrap();
    let cache = ReleaseCache::new(Some(temporary.path()), &runner).unwrap();
    let elf = minimal_static_elf();
    let pin = devkit::runtime::PnpmPin {
        package: "@pnpm/exe.linux-x64-musl".into(),
        version: "12.21.0".into(),
        tarball_sha256: "a".repeat(64),
        binary_sha256: digest(&elf),
    };
    let error = cache
        .ensure_pnpm(&pin, None, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("absent from local cache"), "{error}");
    let target = temporary.path().join(format!(
        "tools/pnpm-{}-x86_64-unknown-linux-musl-{}/pnpm",
        pin.version, pin.binary_sha256
    ));
    write_atomic(&target, &elf, 0o755).unwrap();
    assert_eq!(cache.ensure_pnpm(&pin, None, false).unwrap(), target);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(cache.ensure_pnpm(&pin, None, false).is_err());
    write_atomic(&target, b"tampered", 0o755).unwrap();
    assert!(cache.ensure_pnpm(&pin, None, false).is_err());
}

#[test]
fn static_elf_verifier_rejects_interpreters_needed_libraries_and_truncation() {
    let temporary = TempDir::new().unwrap();
    let target = temporary.path().join("pnpm");
    let elf = minimal_static_elf();
    for case in [
        "valid",
        "interpreter",
        "needed",
        "truncated",
        "architecture",
    ] {
        let mut bytes = elf.clone();
        match case {
            "interpreter" => bytes[64..68].copy_from_slice(&3u32.to_le_bytes()),
            "needed" => {
                bytes[64..68].copy_from_slice(&2u32.to_le_bytes());
                bytes[72..80].copy_from_slice(&120u64.to_le_bytes());
                bytes[96..104].copy_from_slice(&16u64.to_le_bytes());
                bytes.extend_from_slice(&1u64.to_le_bytes());
                bytes.extend_from_slice(&0u64.to_le_bytes());
            }
            "truncated" => bytes.truncate(70),
            "architecture" => bytes[18..20].copy_from_slice(&183u16.to_le_bytes()),
            _ => {}
        }
        write_atomic(&target, &bytes, 0o755).unwrap();
        assert_eq!(
            devkit::delivery::verify_static_elf_file(&target, &digest(&bytes), true).is_ok(),
            case == "valid",
            "{case}"
        );
    }
}

#[test]
fn pnpm_pins_validate_source_release_and_future_extensions() {
    use devkit::runtime::PnpmPin;
    let pin = PnpmPin::from_source_pins(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(PnpmPin::parse(pin.encode().as_bytes()).unwrap(), pin);
    assert!(PnpmPin::parse(format!("{}DEVKIT_PNPM_FUTURE=1\n", pin.encode()).as_bytes()).is_ok());
    for source in [
        pin.encode().replace("12.21.0", "latest"),
        pin.encode().replace("linux-x64-musl", "linux-arm64"),
        format!("{}DEVKIT_PNPM_VERSION=1.0.0\n", pin.encode()),
        pin.encode().replace(&pin.binary_sha256, "bad"),
    ] {
        assert!(PnpmPin::parse(source.as_bytes()).is_err());
    }
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
fn rejected_scoped_runtime_preserves_the_working_offline_release() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fixture(&source, b"working");
    let version = "0.6.0-rc.1";
    let mut manifest: Release = read_json(&source.join("release.json")).unwrap();
    manifest.version = version.into();
    manifest.environment.version = version.into();
    runtime_fixture::write_runtime(&source, version, &manifest.files["devkit"].sha256);
    write_atomic(
        &source.join("runtime/extra-guide.md"),
        b"Shared runtime guide\n",
        0o644,
    )
    .unwrap();
    let seal = |root: &Path| {
        let mut release = manifest.clone();
        release.files = file_map(root).unwrap();
        release.files.remove("release.json");
        write_json(&root.join("release.json"), &release).unwrap();
    };
    seal(&source);
    let runner = ProcessRunner::new().unwrap();
    let cache = ReleaseCache::new(Some(&temp.path().join("cache")), &runner).unwrap();
    let good = cache.import(&source, version, None).unwrap();
    let reference = cache.root.join(format!("refs/{version}.json"));
    let saved = fs::read(&reference).unwrap();

    for (path, replacement) in [
        (
            "runtime/licenses/extra.NOTICE",
            Some(b"Additional notice\n".as_slice()),
        ),
        (
            "runtime/licenses/vendor/NOTICE",
            Some(b"Nested notice\n".as_slice()),
        ),
        ("runtime/LICENSE", None),
        ("runtime/LICENSE", Some(b"".as_slice())),
        ("runtime/licenses/Apache-2.0.txt", Some(b"".as_slice())),
        (
            "runtime/licenses/moby-profiles.NOTICE",
            Some(b"".as_slice()),
        ),
        ("runtime/devkit.mk", None),
        ("devkit", Some(b"invalid engine".as_slice())),
        (
            "runtime/compose.tools.yaml",
            Some(b"services: {}\n".as_slice()),
        ),
        (
            "runtime/desktop.Dockerfile",
            Some(b"FROM changed\n".as_slice()),
        ),
    ] {
        let bad = tempfile::tempdir_in(temp.path()).unwrap();
        copy_tree(&source, bad.path(), &|_| false, false).unwrap();
        match replacement {
            Some(bytes) => write_atomic(&bad.path().join(path), bytes, 0o644).unwrap(),
            None => fs::remove_file(bad.path().join(path)).unwrap(),
        }
        seal(bad.path());
        assert!(
            cache
                .acquire(version, None, Some(bad.path()), true)
                .is_err(),
            "{path}"
        );
        assert_eq!(fs::read(&reference).unwrap(), saved, "{path}");
        assert_eq!(
            cache.acquire(version, None, None, true).unwrap().identity,
            good.identity,
            "{path}"
        );
    }
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
