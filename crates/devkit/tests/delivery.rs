use devkit::{
    delivery::{EnginePin, file_digest, verify_cached_engine, verify_engine},
    distribution::TARGET,
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
