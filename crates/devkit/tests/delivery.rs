use devkit::{
    delivery::{EnginePin, file_digest, verify_cached_engine, verify_engine},
    distribution::TARGET,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;

#[test]
fn source_execution_permission_and_canonical_cache_mode_are_distinct() {
    let temporary = TempDir::new().unwrap();
    let path = temporary.path().join("native engine");
    fs::copy(Path::new(env!("CARGO_BIN_EXE_devkit")), &path).unwrap();
    let pin = EnginePin {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: TARGET.to_owned(),
        sha256: file_digest(&path).unwrap(),
    };
    for mode in [0o700, 0o755, 0o644, 0o645] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        // The file is owned by this process. Root's actual X_OK semantics allow
        // any set execute bit; cache policy still requires exactly 0755.
        let root = unsafe { libc::geteuid() } == 0;
        let executable = mode & 0o111 != 0 && (root || mode & 0o100 != 0);
        assert_eq!(
            verify_engine(&path, &pin).is_ok(),
            executable,
            "source mode {mode:o}"
        );
        assert_eq!(
            verify_cached_engine(&path, &pin).is_ok(),
            mode == 0o755,
            "cache mode {mode:o}"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
    assert_eq!(file_digest(&path).unwrap(), pin.sha256);
}
