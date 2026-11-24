use devkit::{
    delivery::{EnginePin, file_digest, verify_cached_engine, verify_engine},
    distribution::Target,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;

#[test]
fn fetch_resolves_relative_redirects_and_limits_bearer_to_the_original_origin() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        process::Command,
        thread,
    };
    let temporary = TempDir::new().unwrap();
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let first_address = first.local_addr().unwrap();
    let second_address = second.local_addr().unwrap();
    let first_thread = thread::spawn(move || {
        for path in ["/dir/start", "/next"] {
            let (mut connection, _) = first.accept().unwrap();
            connection
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            let count = connection.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..count]).to_lowercase();
            assert!(request.starts_with(&format!("get {path} ")), "{request}");
            assert!(
                request.contains("authorization: bearer fixture-token\r\n"),
                "{request}"
            );
            let location = if path == "/dir/start" {
                "../next".to_owned()
            } else {
                format!("http://{second_address}/final")
            };
            write!(connection, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let second_thread = thread::spawn(move || {
        let (mut connection, _) = second.accept().unwrap();
        connection
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0; 4096];
        let count = connection.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..count]).to_lowercase();
        assert!(!request.contains("authorization:"), "{request}");
        connection
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });
    let output = temporary.path().join("download");
    let result = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "internal",
            "fetch",
            "--allow-loopback-http",
            "--url",
            &format!("http://{first_address}/dir/start"),
            "--output",
        ])
        .arg(&output)
        .args(["--max-bytes", "10"])
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env("DEVKIT_INTERNAL_FETCH_BEARER", "fixture-token")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    first_thread.join().unwrap();
    second_thread.join().unwrap();
    assert_eq!(fs::read(output).unwrap(), b"ok");
}

#[test]
fn fetch_rejects_plain_http_without_fixture_permission() {
    let temporary = TempDir::new().unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "internal",
            "fetch",
            "--url",
            "http://127.0.0.1:1/file",
            "--max-bytes",
            "1",
            "--output",
        ])
        .arg(temporary.path().join("output"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("HTTPS"));
}

#[test]
fn source_execution_permission_and_canonical_cache_mode_are_distinct() {
    let temporary = TempDir::new().unwrap();
    let path = temporary.path().join("native engine");
    fs::copy(Path::new(env!("CARGO_BIN_EXE_devkit")), &path).unwrap();
    let pin = EnginePin {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: Target::host().unwrap().triple().into(),
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
#[test]
fn current_executable_authority_detects_replacement_after_verification() {
    use devkit::delivery::current_engine_digest;
    use std::os::unix::fs::PermissionsExt;
    const WORKER: &str = "DEVKIT_TEST_CURRENT_EXECUTABLE_WORKER";
    if let Ok(order) = std::env::var(WORKER) {
        if order == "after" {
            let first = current_engine_digest().unwrap();
            assert_eq!(first, current_engine_digest().unwrap());
        }
        let current = std::env::current_exe().unwrap();
        let replacement = current.with_extension("replacement");
        // A valid copy of the very same executable defeats a digest-only check.
        std::fs::copy(&current, &replacement).unwrap();
        std::fs::rename(replacement, current).unwrap();
        assert!(current_engine_digest().is_err());
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("delivery-test");
    for order in ["before", "after"] {
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "current_executable_authority_detects_replacement_after_verification",
                "--nocapture",
            ])
            .env(WORKER, order)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{order}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Build a load-command table in isolation. Never execute fixture bytes.
fn mach_o(dylib_command: u32, dependency: &str) -> Vec<u8> {
    fn path_command(kind: u32, fixed_size: usize, path: &str) -> Vec<u8> {
        let size = (fixed_size + path.len() + 1).next_multiple_of(8);
        let mut cmd = vec![0; size];
        cmd[..4].copy_from_slice(&kind.to_le_bytes());
        cmd[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        cmd[8..12].copy_from_slice(&(fixed_size as u32).to_le_bytes());
        cmd[fixed_size..fixed_size + path.len()].copy_from_slice(path.as_bytes());
        cmd
    }
    let linker = path_command(0xe, 12, "/usr/lib/dyld");
    let dylib = path_command(dylib_command, 24, dependency);
    let mut segment = [0_u8; 72];
    let mut signature = [0_u8; 16];
    let table_size = (linker.len() + dylib.len() + segment.len() + signature.len()) as u32;
    let total_size = u64::from(32 + table_size) + 12;
    segment[..4].copy_from_slice(&0x19_u32.to_le_bytes()); // LC_SEGMENT_64
    segment[4..8].copy_from_slice(&72_u32.to_le_bytes());
    segment[8..14].copy_from_slice(b"__TEXT");
    segment[40..48].copy_from_slice(&0_u64.to_le_bytes()); // fileoff
    segment[48..56].copy_from_slice(&total_size.to_le_bytes()); // filesize
    segment[60..64].copy_from_slice(&5_u32.to_le_bytes()); // VM_PROT_READ | VM_PROT_EXECUTE
    signature[..4].copy_from_slice(&0x1d_u32.to_le_bytes());
    signature[4..8].copy_from_slice(&16_u32.to_le_bytes());
    signature[8..12].copy_from_slice(&(32 + table_size).to_le_bytes());
    signature[12..].copy_from_slice(&12_u32.to_le_bytes());
    let mut header = [0_u8; 32];
    for (at, value) in [
        (0, 0xfeedfacf_u32),
        (4, 0x0100000c),
        (12, 2),
        (16, 4),
        (20, table_size),
    ] {
        header[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let mut superblob = [0_u8; 12];
    superblob[..4].copy_from_slice(&0xfade0cc0_u32.to_be_bytes());
    superblob[4..8].copy_from_slice(&12_u32.to_be_bytes());
    [
        header.as_slice(),
        &linker,
        &dylib,
        &segment,
        &signature,
        &superblob,
    ]
    .concat()
}

#[test]
fn target_format_and_mach_o_load_commands_are_verified_before_execution() {
    use devkit::{delivery::verify_executable, files::digest};
    let temp = TempDir::new().unwrap();
    let binary = temp.path().join("native");
    let verify = |bytes: &[u8], target| {
        fs::write(&binary, bytes).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        verify_executable(&binary, &digest(bytes), target, true).is_ok()
    };
    let signed = mach_o(0xc, "/usr/lib/libSystem.B.dylib");
    assert!(verify(&signed, Target::DarwinArm64));
    let seg_offset = signed.len() - 12 - 16 - 72;
    let sig_offset = signed.len() - 12 - 16;
    let blob_offset = signed.len() - 12;
    for (offset, value) in [
        (0, 0xcafebabe_u32),         // universal executable
        (4, 0x01000007),             // thin x86-64
        (12, 6),                     // shared library
        (16, u32::MAX),              // overflowing command count
        (20, u32::MAX),              // table beyond EOF
        (36, 7),                     // undersized command
        (40, 0),                     // path inside the command header
        (seg_offset + 60, 1),        // non-executable segment
        (seg_offset + 48, u32::MAX), // segment beyond EOF
        (sig_offset, 0),             // missing LC_CODE_SIGNATURE
        (sig_offset + 8, 1),         // signature inside the header
        (sig_offset + 12, u32::MAX), // signature beyond EOF
        (blob_offset, 0xdeadbeef),   // bad CSMAGIC_EMBEDDED_SIGNATURE
    ] {
        let mut bytes = signed.clone();
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(!verify(&bytes, Target::DarwinArm64), "offset {offset}");
    }
    assert!(!verify(&signed, Target::LinuxArm64));
    for command in [0xc, 0x80000018, 0x8000001f, 0x20, 0x80000023] {
        for path in [
            "/opt/homebrew/lib/a",
            "@rpath/a",
            "/usr/lib/../local/lib/a",
            "/System/Library/Frameworks/../a",
        ] {
            assert!(!verify(&mach_o(command, path), Target::DarwinArm64));
        }
    }
    assert!(!verify(
        &mach_o(0x8000001c, "/usr/lib/a"),
        Target::DarwinArm64
    ));
    assert!(verify(
        &mach_o(
            0x80000018,
            "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation"
        ),
        Target::DarwinArm64
    ));
}
