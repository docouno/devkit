//! Process semantics are exercised in child test processes, so signals and fake
//! Docker PATH never alter the test harness or another concurrent test.
use devkit::processes::{OutputMode, OwnedContainer, ProcessRunner, exit_code};
use std::{
    fs,
    io::{self, Read},
    os::fd::AsRawFd,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "Readiness marker missing: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn nonblocking(pipe: &impl AsRawFd) {
    let fd = pipe.as_raw_fd();
    // SAFETY: this descriptor belongs to the owned test child output pipe.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
}
fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> bool {
    let mut block = [0; 4096];
    for _ in 0..64 {
        match pipe.read(&mut block) {
            Ok(0) => return true,
            Ok(count) => {
                assert!(bytes.len() + count < 1024 * 1024);
                bytes.extend_from_slice(&block[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("Cannot read test child: {error}"),
        }
    }
    false
}
fn wait(mut child: Child) -> std::process::Output {
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    nonblocking(&stdout);
    nonblocking(&stderr);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut status = None;
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let out_done = drain(&mut stdout, &mut out);
        let err_done = drain(&mut stderr, &mut err);
        if status.is_none() {
            status = child.try_wait().unwrap();
        }
        if let Some(status) = status
            && out_done
            && err_done
        {
            return std::process::Output {
                status,
                stdout: out,
                stderr: err,
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "Process/output did not complete: {}",
                String::from_utf8_lossy(&err)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn script(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
struct Fixture {
    temp: TempDir,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        script(
            &root.join("docker"),
            r#"#!/bin/sh
set -eu
root=$DEVKIT_PROCESS_FIXTURE
if test -f "$root/daemon-failed"; then echo 'daemon verification unavailable' >&2; exit 97; fi
case "$1 $2" in
  'container ls')
    if test -f "$root/container"; then cat "$root/id"; fi
    ;;
  'container inspect')
    token=$(cat "$root/container")
    printf '{"Id":"%s","Name":"/devkit-process-test","Config":{"Labels":{"io.docouno.devkit.invocation":"%s"}}}\n' "$(cat "$root/id")" "$token"
    ;;
  'stop --time')
    printf 'stop %s\n' "$4" >> "$root/mutations"
    ;;
  'rm --force')
    printf 'rm %s\n' "$3" >> "$root/mutations"
    rm -f "$root/container"
    ;;
  *) echo "unexpected fake Docker command: $*" >&2; exit 98 ;;
esac
"#,
        );
        fs::write(root.join("id"), format!("{ID}\n")).unwrap();
        Self { temp }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn start(&self, scenario: &str) -> Child {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root().to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native_process_probe", "--nocapture"])
            .env("DEVKIT_PROCESS_PROBE", scenario)
            .env("DEVKIT_PROCESS_FIXTURE", self.root())
            .env("PATH", std::env::join_paths(paths).unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

// This helper is executed by a separate test process. Its exit is the exact
// ProcessRunner result, rather than the Rust harness's generic test-failure 101.
#[test]
fn native_process_probe() {
    let Ok(scenario) = std::env::var("DEVKIT_PROCESS_PROBE") else {
        return;
    };
    let runner = ProcessRunner::new().unwrap();
    let owned = OwnedContainer::new("devkit-process-test".into(), "accepted-token".into());
    let mut command = Command::new("sh");
    let source = match scenario.as_str() {
        "exit42" => "exit 42",
        "creator-not-run" => "touch \"$DEVKIT_PROCESS_FIXTURE/creator-ran\"",
        "owned-cleanup" => "printf accepted-token > \"$DEVKIT_PROCESS_FIXTURE/container\"",
        "foreign-cleanup" => "printf foreign-token > \"$DEVKIT_PROCESS_FIXTURE/container\"",
        "cleanup-failed" => "touch \"$DEVKIT_PROCESS_FIXTURE/daemon-failed\"",
        "primary-failed" => "touch \"$DEVKIT_PROCESS_FIXTURE/daemon-failed\"; exit 42",
        "wait-cancel" => {
            "sleep 25 & printf '%s' \"$!\" > \"$DEVKIT_PROCESS_FIXTURE/child-pid\"; touch \"$DEVKIT_PROCESS_FIXTURE/ready\"; wait"
        }
        "delayed-create" => {
            "sleep 1.5 & printf '%s' \"$!\" > \"$DEVKIT_PROCESS_FIXTURE/child-pid\"; touch \"$DEVKIT_PROCESS_FIXTURE/ready\"; wait; printf accepted-token > \"$DEVKIT_PROCESS_FIXTURE/container\""
        }
        "inflight-create" => {
            "trap 'printf accepted-token > \"$DEVKIT_PROCESS_FIXTURE/container\"; touch \"$DEVKIT_PROCESS_FIXTURE/created\"; exit 143' TERM; sleep 25 & touch \"$DEVKIT_PROCESS_FIXTURE/ready\"; wait"
        }
        _ => panic!("Unknown process probe"),
    };
    command.args(["-c", source]);
    let result = runner.run_owned(&mut command, OutputMode::Inherit, &owned);
    let code = match result {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("{error:#}");
            exit_code(&error)
        }
    };
    std::process::exit(code as i32);
}

#[test]
fn argv_is_exact_without_host_shell_expansion() {
    let runner = ProcessRunner::new().unwrap();
    let arguments = [
        "has spaces",
        "$(touch should-not-exist)",
        "one;two",
        "'quoted'",
        "",
        "--flag=value",
    ];
    let output = runner
        .capture(Command::new("printf").arg("%s\n").args(arguments))
        .unwrap();
    assert_eq!(
        output.stdout,
        arguments
            .join("\n")
            .as_bytes()
            .iter()
            .copied()
            .chain(*b"\n")
            .collect::<Vec<_>>()
    );
}
#[test]
fn stdout_and_stderr_are_drained_concurrently_and_bounded() {
    let runner = ProcessRunner::new().unwrap();
    let output = runner.capture(Command::new("sh").args(["-c", "i=0; while test $i -lt 12000; do printf 'stdout-record\n'; printf 'stderr-record\n' >&2; i=$((i+1)); done"])).unwrap();
    assert_eq!(output.stdout.len(), 14 * 12000);
    assert_eq!(output.stderr.len(), 14 * 12000);
    let before = Instant::now();
    let error = runner.run(Command::new("sh").args(["-c", "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n'; done"]), OutputMode::Capture(4096)).err().unwrap();
    assert!(format!("{error:#}").contains("byte budget"));
    assert!(before.elapsed() < Duration::from_secs(5));
}
#[test]
fn normal_creator_exit_reaps_children_holding_output_pipe() {
    let fixture = Fixture::new();
    let runner = ProcessRunner::new().unwrap();
    let before = Instant::now();
    runner
        .capture(
            Command::new("sh")
                .args(["-c", "sleep 25 & printf '%s' \"$!\" > \"$1\"", "sh"])
                .arg(fixture.root().join("child-pid")),
        )
        .unwrap();
    assert!(before.elapsed() < Duration::from_secs(5));
    let pid = fs::read_to_string(fixture.root().join("child-pid")).unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "Creator child was not reaped"
    );
}
#[test]
fn native_failure_and_cleanup_failure_preserve_primary_exit() {
    for (scenario, code, diagnostic) in [
        ("exit42", 42, "exit 42"),
        ("cleanup-failed", 1, "Cannot verify cleanup"),
        ("primary-failed", 42, "daemon verification unavailable"),
    ] {
        let fixture = Fixture::new();
        let output = wait(fixture.start(scenario));
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic));
    }
}
#[test]
fn cleanup_uses_immutable_id_and_refuses_unowned_objects() {
    let fixture = Fixture::new();
    let output = wait(fixture.start("owned-cleanup"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.root().join("container").exists());
    assert_eq!(
        fs::read_to_string(fixture.root().join("mutations")).unwrap(),
        format!("stop {ID}\nrm {ID}\n")
    );
    let fixture = Fixture::new();
    let output = wait(fixture.start("foreign-cleanup"));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Refusing unowned container"));
    assert_eq!(
        fs::read_to_string(fixture.root().join("container")).unwrap(),
        "foreign-token"
    );
    assert!(!fixture.root().join("mutations").exists());
    let fixture = Fixture::new();
    fs::write(fixture.root().join("container"), "accepted-token").unwrap();
    let output = wait(fixture.start("creator-not-run"));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert!(!fixture.root().join("creator-ran").exists());
    assert!(!fixture.root().join("mutations").exists());
}
#[test]
fn cancellation_reaps_before_cleanup_and_never_reports_success() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        for scenario in ["wait-cancel", "delayed-create", "inflight-create"] {
            let fixture = Fixture::new();
            let child = fixture.start(scenario);
            wait_for(&fixture.root().join("ready"));
            // SAFETY: this PID belongs to the live child spawned by this test.
            assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
            let output = wait(child);
            assert_eq!(
                output.status.code(),
                Some(128 + signal),
                "{scenario}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!fixture.root().join("container").exists());
            if scenario == "inflight-create" {
                assert!(fixture.root().join("created").exists());
            }
            if let Ok(pid) = fs::read_to_string(fixture.root().join("child-pid")) {
                assert!(
                    !Path::new(&format!("/proc/{pid}")).exists(),
                    "Creator child survived {scenario}"
                );
            }
        }
    }
}
