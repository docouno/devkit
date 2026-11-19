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
        if args == vec!["--help"] {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.contains("Commands: help, info, setup, doctor,"),
                "{text}"
            );
            assert!(
                text.contains("Reconcile transitive lockfile resolutions when needed"),
                "{text}"
            );
            assert!(!text.contains("npm lockfile"), "{text}");
        }
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
    write_atomic(&root.join(".devkit/state/task.lock"), b"", 0o644).unwrap();
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

#[test]
fn catalog_exposes_library_templates_and_design_system_preset() {
    let temp = TempDir::new().unwrap();
    let json_out = invoke(temp.path(), &["catalog", "--json"]);
    assert!(json_out.status.success());
    let payload = decoded(&json_out);
    let templates = payload["result"]["templates"].as_array().unwrap();
    let find_template = |id: &str| {
        templates
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap_or_else(|| panic!("missing template {id}"))
    };
    let ts_lib = find_template("ts-library");
    assert_eq!(ts_lib["role"], "library");
    assert_eq!(ts_lib["toolchain"], "node");
    assert_eq!(ts_lib["profile"], "ts-library");

    let react_ui = find_template("react-ui");
    assert_eq!(react_ui["role"], "library");
    assert_eq!(react_ui["toolchain"], "node");
    assert_eq!(react_ui["profile"], "react-ui");

    assert_eq!(
        payload["result"]["presets"],
        serde_json::json!(["fullstack-web", "fullstack-desktop", "design-system"])
    );

    let text_out = invoke(temp.path(), &["catalog"]);
    assert!(text_out.status.success());
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    for needle in ["ts-library", "react-ui", "design-system"] {
        assert!(
            stdout.contains(needle),
            "catalog text missing {needle}: {stdout}"
        );
    }
}

fn minimal_static_x86_64_elf(stdout_payload: &[u8]) -> Vec<u8> {
    assert!(stdout_payload.len() < 4096);
    let mut code = Vec::new();
    code.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1 (SYS_write)
    code.extend_from_slice(&[0xbf, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1 (stdout)
    code.extend_from_slice(&[0x48, 0x8d, 0x35, 0x10, 0x00, 0x00, 0x00]); // lea rsi, [rip + 16]
    code.push(0xba); // mov edx, imm32
    code.extend_from_slice(&(stdout_payload.len() as u32).to_le_bytes());
    code.extend_from_slice(&[0x0f, 0x05]); // syscall
    code.extend_from_slice(&[0xb8, 0x3c, 0x00, 0x00, 0x00]); // mov eax, 60 (SYS_exit)
    code.extend_from_slice(&[0x31, 0xff]); // xor edi, edi
    code.extend_from_slice(&[0x0f, 0x05]); // syscall
    assert_eq!(code.len(), 33);

    let total_len = 64 + 56 + code.len() + stdout_payload.len();
    let mut elf = Vec::with_capacity(total_len);
    // ELF64 header (64 bytes)
    elf.extend_from_slice(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    elf.extend_from_slice(&62u16.to_le_bytes()); // e_machine = EM_X86_64
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version = 1
    elf.extend_from_slice(&0x400078u64.to_le_bytes()); // e_entry = 0x400000 + 120
    elf.extend_from_slice(&64u64.to_le_bytes()); // e_phoff = 64
    elf.extend_from_slice(&0u64.to_le_bytes()); // e_shoff = 0
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags = 0
    elf.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize = 64
    elf.extend_from_slice(&56u16.to_le_bytes()); // e_phentsize = 56
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum = 1
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    assert_eq!(elf.len(), 64);

    // Program header PT_LOAD (56 bytes)
    elf.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    elf.extend_from_slice(&5u32.to_le_bytes()); // p_flags = PF_R | PF_X
    elf.extend_from_slice(&0u64.to_le_bytes()); // p_offset = 0
    elf.extend_from_slice(&0x400000u64.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&0x400000u64.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&(total_len as u64).to_le_bytes()); // p_filesz
    elf.extend_from_slice(&(total_len as u64).to_le_bytes()); // p_memsz
    elf.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align
    assert_eq!(elf.len(), 120);

    elf.extend_from_slice(&code);
    elf.extend_from_slice(stdout_payload);
    elf
}

#[test]
fn update_and_sync_forward_dispatch_to_synthetic_99_0_0_release_with_unknown_profiles() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha512};

    let temp = TempDir::new().unwrap();
    let cache_dir = temp.path().join("cache");
    let rel_future = temp.path().join("release-99.0.0");
    fs::create_dir_all(&rel_future).unwrap();

    let delegated_stdout =
        b"{\"schemaVersion\":1,\"ok\":true,\"command\":\"delegated\",\"result\":{\"engine\":\"99.0.0\"}}\n";
    let elf_bytes = minimal_static_x86_64_elf(delegated_stdout);
    let elf_sha = digest(&elf_bytes);
    write_atomic(&rel_future.join("devkit"), &elf_bytes, 0o755).unwrap();

    let env_bytes = b"future env archive bytes";
    let env_sri = format!("sha512-{}", STANDARD.encode(Sha512::digest(env_bytes)));
    write_atomic(&rel_future.join("npm/env-99.0.0.tgz"), env_bytes, 0o644).unwrap();
    write_atomic(
        &rel_future.join("docs/migrations/99.0.0.md"),
        b"# 99.0.0\n\n## Future Wasm Profile\n\nDetails.\n",
        0o644,
    )
    .unwrap();
    let recipe_hash = "b".repeat(64);
    let toolchains = format!(
        "DEVKIT_VERSION=99.0.0\nDEVKIT_ENGINE_SHA256={elf_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(
        &rel_future.join("runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    write_json(
        &rel_future.join("release.json"),
        &devkit::model::Release {
            schema_version: 1,
            version: "99.0.0".into(),
            min_supported_from_version: Some("0.4.0".into()),
            rust_version: "1.99.1".into(),
            environment: devkit::model::Environment {
                name: "@docouno/env".into(),
                version: "99.0.0".into(),
                archive: "npm/env-99.0.0.tgz".into(),
                integrity: env_sri,
            },
            files: file_map(&rel_future).unwrap(),
            migrations: vec![devkit::model::MigrationEntry {
                id: "future-wasm-990".into(),
                version: "99.0.0".into(),
                severity: devkit::model::MigrationSeverity::Info,
                title: "Future wasm profile".into(),
                summary: "Tests unknown profile tolerance during forward-dispatch.".into(),
                profiles: vec!["future-wasm".into()],
                doc_path: "docs/migrations/99.0.0.md".into(),
                anchor: "future-wasm-profile".into(),
            }],
        },
    )
    .unwrap();
    let rel_identity = devkit::release::admit_release(&rel_future)
        .unwrap()
        .identity;

    // Create a project directory whose manifest already uses an unknown future ToolingProfile ("future-wasm")!
    let proj = temp.path().join("proj");
    fs::create_dir_all(proj.join(".devkit/runtime")).unwrap();
    write_atomic(
        &proj.join(".devkit/runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    let tc_id = identity(&proj.join(".devkit/runtime/toolchains.env"))
        .unwrap()
        .unwrap();
    write_json(
        &proj.join(MANIFEST_PATH),
        &serde_json::json!({
            "schemaVersion": 2,
            "version": "99.0.0",
            "releaseIdentity": rel_identity,
            "generation": uuid::Uuid::new_v4().to_string(),
            "packageMode": "local",
            "managed": {
                ".devkit/runtime/toolchains.env": tc_id
            },
            "components": {
                "packages/wasm": {
                    "role": "library",
                    "toolchain": "rust",
                    "toolingProfile": "future-wasm",
                    "origin": { "template": "wasm-lib", "version": "99.0.0" },
                    "toolDependencies": {},
                    "dependencies": []
                }
            }
        }),
    )
    .unwrap();

    // 1. `devkit update` forward-dispatches to 99.0.0's engine without failing on "future-wasm".
    let update_out = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "update",
            proj.to_str().unwrap(),
            "--to",
            "99.0.0",
            "--release-root",
            rel_future.to_str().unwrap(),
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(
        update_out.status.success(),
        "update stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&update_out.stdout),
        String::from_utf8_lossy(&update_out.stderr)
    );
    assert_eq!(decoded(&update_out)["result"]["engine"], "99.0.0");

    // 2. `devkit sync` dispatches to the accepted 99.0.0 engine without choking on "future-wasm".
    let sync_out = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "sync",
            proj.to_str().unwrap(),
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(
        sync_out.status.success(),
        "sync stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync_out.stdout),
        String::from_utf8_lossy(&sync_out.stderr)
    );
    assert_eq!(decoded(&sync_out)["result"]["engine"], "99.0.0");

    // 3. `devkit add-package` also forward-dispatches to the accepted 99.0.0 engine.
    let add_out = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "add-package",
            proj.to_str().unwrap(),
            "--name",
            "extra",
            "--template",
            "future-wasm",
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(
        add_out.status.success(),
        "add-package stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_out.stdout),
        String::from_utf8_lossy(&add_out.stderr)
    );
    assert_eq!(decoded(&add_out)["result"]["engine"], "99.0.0");
}

#[test]
fn bootstrap_interactive_with_dev_null_stdin_fails_immediately_listing_missing_flags() {
    use std::os::{fd::FromRawFd, unix::process::CommandExt};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let temp = TempDir::new().unwrap();
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty initializes two valid file descriptors when returning 0.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let _master_guard = unsafe { fs::File::from_raw_fd(master) };
    let stderr_tty = unsafe { fs::File::from_raw_fd(slave) };

    let started = Instant::now();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_devkit"));
    cmd.arg("init")
        .current_dir(temp.path())
        .env_clear()
        .env("DEVKIT_BOOTSTRAP_INTERACTIVE", "1")
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_tty));
    // SAFETY: setsid and TIOCSCTTY make stderr's PTY the foreground controlling terminal
    // of the child, reproducing an agent subshell with a live TTY on stderr and /dev/null on stdin.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(2, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "DEVKIT_BOOTSTRAP_INTERACTIVE=1 with /dev/null stdin hung on /dev/tty"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    assert!(started.elapsed() < Duration::from_secs(3));

    let json_out = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args(["init", "--json"])
        .current_dir(temp.path())
        .env_clear()
        .env("DEVKIT_BOOTSTRAP_INTERACTIVE", "1")
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!json_out.status.success());
    let payload = decoded(&json_out);
    assert_eq!(payload["error"]["code"], "MISSING_ARGUMENT");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(message.contains("TARGET"), "missing TARGET in: {message}");
    assert!(
        message.contains("--app/--package/--preset/--template"),
        "missing template flags in: {message}"
    );
}

#[test]
fn doctor_json_runs_lockfree_outside_checkout_and_emits_single_json_on_failure() {
    let temp = TempDir::new().unwrap();
    let sentinel = temp.path().join("invalid-cache-file");
    fs::write(&sentinel, "owner cache sentinel").unwrap();

    // Outside any initialized project (with empty PATH so docker check fails cleanly -> single JSON object, exit 1).
    let outside_dir = temp.path().join("uninitialized");
    fs::create_dir_all(&outside_dir).unwrap();
    let out = invoke(&outside_dir, &["doctor", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.is_empty(),
        "doctor --json leaked to stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload = decoded(&out);
    assert_eq!(payload["schemaVersion"], 1);
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["command"], "doctor");
    let checks = payload["result"]["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 6);
    let checkout_check = checks
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .expect("missing checkout_state check");
    assert_eq!(checkout_check["status"], "ok");
    assert_eq!(checkout_check["details"]["present"], false);
    assert!(!outside_dir.join(".devkit").exists());
    assert_eq!(
        fs::read_to_string(&sentinel).unwrap(),
        "owner cache sentinel"
    );

    // Inside a checkout with incomplete update-pending.json and held LOCK_EX on task.lock.
    let broken = temp.path().join("broken-checkout");
    fs::create_dir_all(broken.join(".devkit/state")).unwrap();
    fs::write(broken.join(PENDING_PATH), "{}").unwrap();
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(broken.join(".devkit/state/task.lock"))
        .unwrap();
    lock_file.try_lock().unwrap();

    let broken_out = invoke(temp.path(), &["doctor", broken.to_str().unwrap(), "--json"]);
    assert_eq!(broken_out.status.code(), Some(1));
    assert!(broken_out.stderr.is_empty());
    let broken_json = decoded(&broken_out);
    let broken_checkout = broken_json["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .unwrap();
    assert_eq!(broken_checkout["status"], "fail");
    assert_eq!(broken_checkout["remediation"], "devkit recover");
    drop(lock_file);
}

#[test]
fn checkout_lock_supports_concurrent_shared_readers_and_detects_self_recursion() {
    use devkit::state::CheckoutLock;
    use std::collections::BTreeMap;

    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Multiple shared locks coexist simultaneously.
    let s1 = CheckoutLock::acquire_shared(root, "describe", None).unwrap();
    let s2 = CheckoutLock::acquire_shared(root, "status", None).unwrap();
    let s3 = CheckoutLock::acquire_shared(root, "task doctor", None).unwrap();
    drop((s1, s2, s3));

    // Exclusive lock records live owner and immediately rejects recursive self-acquisition in 0ms.
    let ex = CheckoutLock::acquire_exclusive(root, "task check", None).unwrap();
    assert!(devkit::state::read_live_owner(root).is_some());
    let started = std::time::Instant::now();
    let err = match CheckoutLock::acquire_shared(root, "describe", None) {
        Ok(_) => panic!("expected recursive lock acquisition to fail"),
        Err(err) => err,
    };
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
    assert!(
        format!("{err:#}").contains("Recursive devkit invocation detected inside active operation"),
        "unexpected error: {err:#}"
    );
    drop(ex);
    assert!(devkit::state::read_live_owner(root).is_none());

    // Compose ps JSON parser handles both NDJSON and JSON arrays.
    let mut map = BTreeMap::new();
    map.insert("web".to_owned(), "apps/web".to_owned());
    let ndjson = br#"{"Service":"web","Name":"proj-web-1","State":"running","Status":"Up 2s (healthy)","Health":"healthy","Publishers":[{"URL":"0.0.0.0","TargetPort":5173,"PublishedPort":49152,"Protocol":"tcp"},{"URL":"","TargetPort":5173,"PublishedPort":0,"Protocol":"tcp"}]}
{"Service":"db","Name":"proj-db-1","State":"running","Status":"Up 2s","Health":"","Ports":"127.0.0.1:54320->5432/tcp"}"#;
    let parsed = devkit::doctor::parse_compose_ps_json(ndjson, &map).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].service, "db");
    assert_eq!(parsed[0].component, None);
    assert_eq!(parsed[0].urls, vec!["http://127.0.0.1:54320"]);
    assert_eq!(parsed[1].service, "web");
    assert_eq!(parsed[1].component.as_deref(), Some("apps/web"));
    assert_eq!(parsed[1].health.as_deref(), Some("healthy"));
    assert_eq!(parsed[1].urls, vec!["http://127.0.0.1:49152"]);
}

#[test]
fn umask_modes_pass_cli_forward_dispatch_and_doctor_remediation_uses_make_sync() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha512};
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let cache_dir = temp.path().join("cache");
    let rel_future = temp.path().join("release-99.0.1");
    fs::create_dir_all(&rel_future).unwrap();

    let delegated_stdout =
        b"{\"schemaVersion\":1,\"ok\":true,\"command\":\"delegated\",\"result\":{\"engine\":\"99.0.1\"}}\n";
    let elf_bytes = minimal_static_x86_64_elf(delegated_stdout);
    let elf_sha = digest(&elf_bytes);
    write_atomic(&rel_future.join("devkit"), &elf_bytes, 0o755).unwrap();

    let env_bytes = b"future env archive bytes";
    let env_sri = format!("sha512-{}", STANDARD.encode(Sha512::digest(env_bytes)));
    write_atomic(&rel_future.join("npm/env-99.0.1.tgz"), env_bytes, 0o644).unwrap();
    let recipe_hash = "b".repeat(64);
    let toolchains = format!(
        "DEVKIT_VERSION=99.0.1\nDEVKIT_ENGINE_SHA256={elf_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(
        &rel_future.join("runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    write_json(
        &rel_future.join("release.json"),
        &devkit::model::Release {
            schema_version: 1,
            version: "99.0.1".into(),
            min_supported_from_version: Some("0.4.0".into()),
            rust_version: "1.99.1".into(),
            environment: devkit::model::Environment {
                name: "@docouno/env".into(),
                version: "99.0.1".into(),
                archive: "npm/env-99.0.1.tgz".into(),
                integrity: env_sri,
            },
            files: file_map(&rel_future).unwrap(),
            migrations: vec![],
        },
    )
    .unwrap();
    let runner = devkit::processes::ProcessRunner::new().unwrap();
    let cache = devkit::cache::ReleaseCache::new(Some(&cache_dir), &runner).unwrap();
    let admitted = cache.import(&rel_future, "99.0.1", None).unwrap();
    cache.prime_engine(&admitted).unwrap();

    let proj = temp.path().join("proj");
    fs::create_dir_all(proj.join(".devkit/runtime")).unwrap();
    for (rel, bytes) in [
        (".devkit/runtime.sha256", b"checksum\n".as_slice()),
        (".devkit/runtime/toolchains.env", toolchains.as_bytes()),
        (".devkit/project.mk", b"# mk\n".as_slice()),
        (".devkit/compose.apps.yaml", b"{}\n".as_slice()),
        (".devkit/tooling-profiles.json", b"{}\n".as_slice()),
    ] {
        write_atomic(&proj.join(rel), bytes, 0o644).unwrap();
    }
    let mut managed_map = serde_json::Map::new();
    for rel in [
        ".devkit/runtime.sha256",
        ".devkit/runtime/toolchains.env",
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/tooling-profiles.json",
    ] {
        let id = identity(&proj.join(rel)).unwrap().unwrap();
        assert_eq!(id.mode, 0o644);
        managed_map.insert(rel.to_owned(), serde_json::to_value(id).unwrap());
    }
    write_json(
        &proj.join(MANIFEST_PATH),
        &serde_json::json!({
            "schemaVersion": 2,
            "version": "99.0.1",
            "releaseIdentity": admitted.identity,
            "generation": uuid::Uuid::new_v4().to_string(),
            "packageMode": "local",
            "managed": managed_map,
            "components": {
                "apps/api": {
                    "role": "app",
                    "toolchain": "rust",
                    "toolingProfile": "rust",
                    "origin": { "template": "rust-api", "version": "99.0.1" },
                    "toolDependencies": {},
                    "dependencies": []
                }
            }
        }),
    )
    .unwrap();

    // Simulate umask 0002 (0o664) on .devkit/runtime/toolchains.env while manifest records 0o644.
    fs::set_permissions(
        proj.join(".devkit/runtime/toolchains.env"),
        fs::Permissions::from_mode(0o664),
    )
    .unwrap();
    let sync_out = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "sync",
            proj.to_str().unwrap(),
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(
        sync_out.status.success(),
        "sync stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync_out.stdout),
        String::from_utf8_lossy(&sync_out.stderr)
    );
    assert_eq!(decoded(&sync_out)["result"]["engine"], "99.0.1");

    // Flipping executable bit (0o755) on toolchains.env fails both forward_if_needed and doctor.
    fs::set_permissions(
        proj.join(".devkit/runtime/toolchains.env"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let bad_sync = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "sync",
            proj.to_str().unwrap(),
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(!bad_sync.status.success());
    assert!(
        decoded(&bad_sync)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Managed file conflict")
    );

    let doc_out = invoke(temp.path(), &["doctor", proj.to_str().unwrap(), "--json"]);
    assert_eq!(doc_out.status.code(), Some(1));
    let doc_json = decoded(&doc_out);
    let checkout = doc_json["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .unwrap();
    assert_eq!(checkout["status"], "fail");
    assert_eq!(
        checkout["remediation"],
        "git checkout -- .devkit && make sync"
    );
}

#[test]
fn pre_0_4_checkouts_fail_before_lock_prompt_network_or_make_across_all_commands() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let make_marker = temp.path().join("make-invoked");
    let fake_make = fake_bin.join("make");
    fs::write(
        &fake_make,
        format!(
            "#!/bin/sh\nprintf 'invoked' > '{}'\nexit 99\n",
            make_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();

    let run_cli = |cwd: &Path, args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("CI", "true")
            .env("PATH", &fake_bin)
            .env("DEVKIT_CACHE_DIR", temp.path().join("unused-cache"))
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap()
    };

    for (label, manifest_val) in [
        (
            "schema1",
            serde_json::json!({"schemaVersion": 1, "version": "0.1.0"}),
        ),
        (
            "v021",
            serde_json::json!({"schemaVersion": 2, "version": "0.2.1"}),
        ),
        (
            "v032",
            serde_json::json!({"schemaVersion": 2, "version": "0.3.2"}),
        ),
    ] {
        let proj = temp.path().join(label);
        fs::create_dir_all(proj.join(".devkit")).unwrap();
        write_json(&proj.join(MANIFEST_PATH), &manifest_val).unwrap();
        write_atomic(&proj.join("Makefile"), b"check:\n\t@true\n", 0o644).unwrap();
        let snapshot = file_map(&proj).unwrap();

        let json_cases: &[(&[&str], &str)] = &[
            (
                &["update", ".", "--to", "0.5.0"],
                "UNSUPPORTED_UPGRADE_PATH",
            ),
            (&["update", "."], "UNSUPPORTED_UPGRADE_PATH"),
            (&["sync", "."], "UNSUPPORTED_PROJECT_VERSION"),
            (&["add-app", "."], "UNSUPPORTED_PROJECT_VERSION"),
            (&["add-package", "."], "UNSUPPORTED_PROJECT_VERSION"),
            (&["register", "."], "UNSUPPORTED_PROJECT_VERSION"),
            (&["describe", "."], "UNSUPPORTED_PROJECT_VERSION"),
            (&["status", "."], "UNSUPPORTED_PROJECT_VERSION"),
            (&["doctor", "."], "UNSUPPORTED_PROJECT_VERSION"),
        ];

        for (base_args, expected_code) in json_cases {
            let mut with_json = base_args.to_vec();
            with_json.push("--json");
            let j_out = run_cli(&proj, &with_json);
            assert!(
                !j_out.status.success(),
                "{label} {with_json:?} unexpectedly succeeded"
            );
            assert!(j_out.stderr.is_empty(), "{label} {with_json:?}");
            let payload = decoded(&j_out);
            assert_eq!(
                payload["error"]["code"], *expected_code,
                "{label} {with_json:?}"
            );
            let msg = payload["error"]["message"].as_str().unwrap();
            assert!(msg.contains("0.4.1"), "{msg}");
            assert!(
                msg.contains("npx @docouno/devkit@0.4.1 update . --to 0.4.1"),
                "{msg}"
            );
            assert!(!make_marker.exists(), "{label} {with_json:?}");
            assert!(
                !proj.join(".devkit/state").exists(),
                "{label} {with_json:?}"
            );
            assert_eq!(file_map(&proj).unwrap(), snapshot, "{label} {with_json:?}");

            let h_out = run_cli(&proj, base_args);
            assert!(!h_out.status.success(), "{label} {base_args:?}");
            let stderr = String::from_utf8_lossy(&h_out.stderr);
            assert!(
                stderr.contains("npx @docouno/devkit@0.4.1 update . --to 0.4.1"),
                "{label} {base_args:?}: {stderr}"
            );
            assert!(!make_marker.exists(), "{label} {base_args:?}");
            assert!(
                !proj.join(".devkit/state").exists(),
                "{label} {base_args:?}"
            );
            assert_eq!(file_map(&proj).unwrap(), snapshot, "{label} {base_args:?}");
        }

        for stream_args in [
            vec!["run", "check"],
            vec!["exec", "--component", "apps/api", "--", "true"],
            vec!["task", "check"],
        ] {
            let s_out = run_cli(&proj, &stream_args);
            assert!(!s_out.status.success(), "{label} {stream_args:?}");
            let stderr = String::from_utf8_lossy(&s_out.stderr);
            assert!(
                stderr.contains("npx @docouno/devkit@0.4.1 update . --to 0.4.1"),
                "{label} {stream_args:?}: {stderr}"
            );
            assert!(!make_marker.exists(), "{label} {stream_args:?}");
            assert!(
                !proj.join(".devkit/state").exists(),
                "{label} {stream_args:?}"
            );
            assert_eq!(
                file_map(&proj).unwrap(),
                snapshot,
                "{label} {stream_args:?}"
            );
        }
    }

    // Verify PENDING_PATH priority over both live old/current manifests (at_manifest == false)
    // and missing manifests (at_manifest == true).
    for (label, missing_manifest, version) in [
        ("pending-live-021", false, "0.2.1"),
        ("pending-live-041", false, "0.4.1"),
        ("pending-missing", true, "0.4.1"),
    ] {
        let proj = temp.path().join(label);
        fs::create_dir_all(proj.join(".devkit/runtime")).unwrap();
        write_atomic(&proj.join(PENDING_PATH), b"{}\n", 0o644).unwrap();
        write_atomic(
            &proj.join(".devkit/runtime/toolchains.env"),
            b"DEVKIT_VERSION=0.5.0\n",
            0o644,
        )
        .unwrap();
        write_atomic(&proj.join("Makefile"), b"check:\n\t@true\n", 0o644).unwrap();
        if !missing_manifest {
            write_json(
                &proj.join(MANIFEST_PATH),
                &serde_json::json!({
                    "schemaVersion": 2,
                    "version": version,
                    "releaseIdentity": "a".repeat(64),
                    "managed": {
                        ".devkit/runtime/toolchains.env": {
                            "sha256": "b".repeat(64),
                            "mode": 0o644
                        }
                    }
                }),
            )
            .unwrap();
        }
        for args in [
            vec!["update", ".", "--to", "0.5.0", "--json"],
            vec!["sync", ".", "--json"],
            vec!["add-app", ".", "--json"],
            vec!["describe", ".", "--json"],
            vec!["status", ".", "--json"],
        ] {
            let out = run_cli(&proj, &args);
            assert!(!out.status.success(), "{label} {args:?}");
            let msg = decoded(&out)["error"]["message"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                msg.contains("An update is incomplete; run devkit recover first"),
                "{label} {args:?}: {msg}"
            );
        }
        for args in [
            vec!["run", "check"],
            vec!["task", "check"],
            vec!["exec", "--component", "apps/api", "--", "true"],
        ] {
            let out = run_cli(&proj, &args);
            assert!(!out.status.success(), "{label} {args:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("An update is incomplete; run devkit recover first"),
                "{label} {args:?}: {stderr}"
            );
            assert!(!make_marker.exists(), "{label} {args:?}");
        }
        let doc = run_cli(&proj, &["doctor", ".", "--json"]);
        assert_eq!(doc.status.code(), Some(1), "{label}");
        let doc_check = decoded(&doc)["result"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "checkout_state")
            .unwrap()
            .clone();
        assert_eq!(doc_check["remediation"], "devkit recover", "{label}");
    }

    // Verify > 4 MiB valid JSON manifest bound before fs::read/identity.
    let huge = temp.path().join("huge-manifest");
    fs::create_dir_all(huge.join(".devkit")).unwrap();
    let mut big = Vec::with_capacity(4 * 1024 * 1024 + 256);
    big.extend_from_slice(br#"{"schemaVersion":2,"version":"0.4.1","pad":""#);
    big.resize(4 * 1024 * 1024 + 128, b'x');
    big.extend_from_slice(b"\"}\n");
    write_atomic(&huge.join(MANIFEST_PATH), &big, 0o644).unwrap();
    let huge_out = run_cli(&huge, &["describe", ".", "--json"]);
    assert!(!huge_out.status.success());
    assert!(
        decoded(&huge_out)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Project manifest exceeds 4 MiB byte budget")
    );
    assert!(!huge.join(".devkit/state").exists());
    let huge_doc = run_cli(&huge, &["doctor", ".", "--json"]);
    assert_eq!(huge_doc.status.code(), Some(1));
    let huge_check = decoded(&huge_doc)["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .unwrap()
        .clone();
    assert_eq!(huge_check["status"], "fail");
    assert!(
        huge_check["summary"]
            .as_str()
            .unwrap()
            .contains("Project manifest exceeds 4 MiB byte budget")
    );
    assert!(!huge.join(".devkit/state").exists());
}

#[test]
fn doctor_preserves_structured_checks_for_symlinked_and_corrupt_targets_and_inspect_status_enforces_floor()
 {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();

    // 1. Symlinked empty directory outside any checkout still runs structured host diagnostics.
    let real_empty = temp.path().join("real-empty");
    fs::create_dir_all(&real_empty).unwrap();
    let link_empty = temp.path().join("link-empty");
    symlink(&real_empty, &link_empty).unwrap();
    let empty_out = invoke(
        temp.path(),
        &["doctor", link_empty.to_str().unwrap(), "--json"],
    );
    let empty_json = decoded(&empty_out);
    assert!(
        empty_json.get("error").is_none(),
        "doctor on symlinked empty dir fell out of structured checks: {empty_json}"
    );
    let empty_check = empty_json["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .unwrap();
    assert_eq!(empty_check["status"], "ok");
    assert_eq!(empty_check["details"]["present"], false);

    // 2. Symlinked checkout hits inspect_checkout_lockfree symlink remediation instead of CLI abort.
    let real_proj = temp.path().join("real-proj");
    fs::create_dir_all(real_proj.join(".devkit")).unwrap();
    write_atomic(
        &real_proj.join(MANIFEST_PATH),
        br#"{"schemaVersion":2,"version":"0.5.0"}"#,
        0o644,
    )
    .unwrap();
    let link_proj = temp.path().join("link-proj");
    symlink(&real_proj, &link_proj).unwrap();
    let link_out = invoke(
        temp.path(),
        &["doctor", link_proj.to_str().unwrap(), "--json"],
    );
    assert_eq!(link_out.status.code(), Some(1));
    let link_json = decoded(&link_out);
    assert!(link_json.get("error").is_none(), "{link_json}");
    let link_check = link_json["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .unwrap();
    assert_eq!(link_check["status"], "fail");
    assert!(
        link_check["summary"]
            .as_str()
            .unwrap()
            .contains("Refusing a symlink in path")
    );
    assert_eq!(
        link_check["remediation"],
        "Verify directory permissions and symlinks on the checkout path"
    );

    // 3. Supported checkout with corrupt/conflicted manifest reports structured checkout_state fail.
    let corrupt = temp.path().join("corrupt-proj");
    fs::create_dir_all(corrupt.join(".devkit")).unwrap();
    write_atomic(&corrupt.join(MANIFEST_PATH), b"<<<<<<< HEAD\n", 0o644).unwrap();
    let corrupt_out = invoke(
        temp.path(),
        &["doctor", corrupt.to_str().unwrap(), "--json"],
    );
    assert_eq!(corrupt_out.status.code(), Some(1));
    let corrupt_json = decoded(&corrupt_out);
    assert!(corrupt_json.get("error").is_none(), "{corrupt_json}");
    let corrupt_check = corrupt_json["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "checkout_state")
        .unwrap();
    assert_eq!(corrupt_check["status"], "fail");
    assert_eq!(corrupt_check["remediation"], "make sync");
    assert!(!corrupt.join(".devkit/state").exists());

    // 4. Direct library call runtime::inspect_status enforces >= 0.4.0 floor before CheckoutLock.
    let pre_040 = temp.path().join("pre-040-lib");
    fs::create_dir_all(pre_040.join(".devkit")).unwrap();
    write_atomic(
        &pre_040.join(MANIFEST_PATH),
        br#"{"schemaVersion":2,"version":"0.3.2","releaseIdentity":"0000000000000000000000000000000000000000000000000000000000000000","generation":"gen-0","packageMode":"local","managed":{},"components":{}}"#,
        0o644,
    )
    .unwrap();
    let runner = devkit::processes::ProcessRunner::new().unwrap();
    let err = devkit::runtime::inspect_status(&pre_040, &runner).unwrap_err();
    assert_eq!(devkit::cli::error_code(&err), "UNSUPPORTED_PROJECT_VERSION");
    assert!(!pre_040.join(".devkit/state").exists());
}

#[test]
fn init_and_update_reject_target_versions_below_0_4_0_before_prompts_or_network() {
    let temp = TempDir::new().unwrap();
    let new_dir = temp.path().join("never-created");

    let init_json = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args(["init", new_dir.to_str().unwrap(), "--to", "0.3.2", "--json"])
        .current_dir(temp.path())
        .env_clear()
        .env("CI", "true")
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(!init_json.status.success());
    let init_payload = decoded(&init_json);
    assert_eq!(init_payload["error"]["code"], "UNSUPPORTED_PROJECT_VERSION");
    assert!(
        init_payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("@docouno/devkit@0.4.1")
    );
    assert!(!new_dir.exists());

    let proj_041 = temp.path().join("proj-041");
    fs::create_dir_all(proj_041.join(".devkit")).unwrap();
    write_json(
        &proj_041.join(MANIFEST_PATH),
        &serde_json::json!({"schemaVersion": 2, "version": "0.4.1"}),
    )
    .unwrap();
    let upd_json = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "update",
            proj_041.to_str().unwrap(),
            "--to",
            "0.2.1",
            "--json",
        ])
        .current_dir(temp.path())
        .env_clear()
        .env("CI", "true")
        .env("DEVKIT_CACHE_DIR", temp.path().join("cache"))
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(!upd_json.status.success());
    let upd_payload = decoded(&upd_json);
    assert_eq!(upd_payload["error"]["code"], "UNSUPPORTED_PROJECT_VERSION");
    assert!(
        upd_payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("@docouno/devkit@0.4.1")
    );
    assert!(!proj_041.join(".devkit/state").exists());
}

#[test]
fn symmetric_delegation_between_0_4_1_0_5_0_and_99_0_0_preserves_offline_metadata_contract() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use devkit::project::{InitOptions, LockResolver, init_project};
    use sha2::{Digest, Sha512};
    use std::os::unix::fs::PermissionsExt;

    struct NoResolver;
    impl LockResolver for NoResolver {
        fn regenerate(
            &self,
            _: &Path,
            _: &devkit::release::AdmittedRelease,
            _: Option<&str>,
        ) -> devkit::Result<()> {
            anyhow::bail!("unexpected npm lock resolution")
        }
    }

    let temp = TempDir::new().unwrap();
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let fake_docker = fake_bin.join("docker");
    fs::write(
        &fake_docker,
        "#!/bin/sh\ncase \"$*\" in\n  *'compose version --short'*) printf '5.0.0\\n' ;;\n  *'info --format'*) printf 'linux\\n' ;;\n  *'ps --all --format json'*) printf '[]\\n' ;;\n  *) exit 1 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&fake_docker, fs::Permissions::from_mode(0o755)).unwrap();

    let build_release = |version: &str, use_real_current_engine: bool| -> std::path::PathBuf {
        let rel = temp.path().join(format!("release-{version}"));
        fs::create_dir_all(&rel).unwrap();
        let elf_bytes = if use_real_current_engine {
            fs::read(env!("CARGO_BIN_EXE_devkit")).unwrap()
        } else {
            let marker = format!(
                "{{\"schemaVersion\":1,\"ok\":true,\"command\":\"delegated\",\"result\":{{\"engine\":\"{version}\"}}}}\n"
            );
            minimal_static_x86_64_elf(marker.as_bytes())
        };
        let elf_sha = digest(&elf_bytes);
        write_atomic(&rel.join("devkit"), &elf_bytes, 0o755).unwrap();

        let rust_df = b"FROM scratch\n";
        let desktop_df = b"FROM scratch\n";
        write_atomic(&rel.join("runtime/rust.Dockerfile"), rust_df, 0o644).unwrap();
        write_atomic(&rel.join("runtime/desktop.Dockerfile"), desktop_df, 0o644).unwrap();
        let rust_hash = digest(rust_df);
        let desktop_hash = digest(desktop_df);
        let toolchains = format!(
            "DEVKIT_VERSION={version}\nDEVKIT_ENGINE_SHA256={elf_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={rust_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={desktop_hash}\n"
        );
        write_atomic(
            &rel.join("runtime/toolchains.env"),
            toolchains.as_bytes(),
            0o644,
        )
        .unwrap();
        for name in ["devkit.mk", "devkit.sh", "compose.tools.yaml"] {
            write_atomic(
                &rel.join(format!("runtime/{name}")),
                b"services: {}\n",
                0o644,
            )
            .unwrap();
        }
        for (name, body) in [
            (
                "Cargo.toml",
                "[package]\nname = \"fixture-api\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"fixture-api\"\nversion = \"1.0.0\"\n",
            ),
            ("rustfmt.toml", "edition = \"2024\"\n"),
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.99.1\"\n"),
            ("src/main.rs", "fn main() {}\n"),
        ] {
            write_atomic(
                &rel.join(format!("templates/rust-api/{name}")),
                body.as_bytes(),
                0o644,
            )
            .unwrap();
        }
        if devkit::files::SemVer::parse(version).unwrap()
            >= devkit::files::SemVer::parse("0.5.0").unwrap()
        {
            let pin = devkit::runtime::PnpmPin::from_source_pins(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap(),
            )
            .unwrap();
            write_atomic(
                &rel.join("runtime/pnpm.env"),
                pin.encode().as_bytes(),
                0o644,
            )
            .unwrap();
        }
        let env_bytes = format!("env-{version}");
        let env_sri = format!("sha512-{}", STANDARD.encode(Sha512::digest(&env_bytes)));
        let archive = format!("npm/env-{version}.tgz");
        write_atomic(&rel.join(&archive), env_bytes.as_bytes(), 0o644).unwrap();
        let min_from = (devkit::files::SemVer::parse(version).unwrap()
            >= devkit::files::SemVer::parse("0.5.0").unwrap())
        .then(|| "0.4.0".into());
        write_json(
            &rel.join("release.json"),
            &devkit::model::Release {
                schema_version: 1,
                version: version.into(),
                min_supported_from_version: min_from,
                rust_version: "1.99.1".into(),
                environment: devkit::model::Environment {
                    name: "@docouno/env".into(),
                    version: version.into(),
                    archive,
                    integrity: env_sri,
                },
                files: file_map(&rel).unwrap(),
                migrations: vec![],
            },
        )
        .unwrap();
        rel
    };

    let rel_040 = build_release("0.4.0", false);
    let rel_041 = build_release("0.4.1", false);
    let rel_050 = build_release("0.5.0", true);
    let rel_990 = build_release("99.0.0", false);

    let primed_cache = temp.path().join("primed-cache");
    let runner = devkit::processes::ProcessRunner::new().unwrap();
    let cache = devkit::cache::ReleaseCache::new(Some(&primed_cache), &runner).unwrap();
    let adm_041 = cache.import(&rel_041, "0.4.1", None).unwrap();
    cache.prime_engine(&adm_041).unwrap();
    let adm_990 = cache.import(&rel_990, "99.0.0", None).unwrap();
    cache.prime_engine(&adm_990).unwrap();

    // 1. `devkit init --to 0.4.1` delegates to 0.4.1 engine BEFORE prompt/missing-template check.
    let init_delegated = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            primed_cache.to_str().unwrap(),
            "init",
            temp.path().join("delegated-init").to_str().unwrap(),
            "--to",
            "0.4.1",
            "--release-root",
            rel_041.to_str().unwrap(),
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(init_delegated.status.success());
    assert_eq!(decoded(&init_delegated)["result"]["engine"], "0.4.1");

    let make_proj = |rel: &Path, name: &str| -> std::path::PathBuf {
        let p = temp.path().join(name);
        init_project(
            rel,
            &p,
            &InitOptions {
                template: Some("rust-api".into()),
                package_mode: Some(devkit::model::PackageMode::Registry),
                ..Default::default()
            },
            &NoResolver,
        )
        .unwrap();
        p
    };
    let proj_040 = make_proj(&rel_040, "proj-040");
    let proj_041 = make_proj(&rel_041, "proj-041");
    let proj_050 = make_proj(&rel_050, "proj-050");
    let proj_990 = make_proj(&rel_990, "proj-990");

    // 2. `devkit update <proj-040> --to 0.4.1` symmetrically delegates downward to 0.4.1 engine.
    let upd_041 = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .args([
            "--json",
            "--cache-dir",
            primed_cache.to_str().unwrap(),
            "update",
            proj_040.to_str().unwrap(),
            "--to",
            "0.4.1",
            "--release-root",
            rel_041.to_str().unwrap(),
        ])
        .env_clear()
        .env("CI", "true")
        .output()
        .unwrap();
    assert!(upd_041.status.success());
    assert_eq!(decoded(&upd_041)["result"]["engine"], "0.4.1");

    // 3. With primed cache, project commands on 0.4.1 delegate to 0.4.1 engine.
    for cmd_args in [
        vec!["--json", "sync", proj_041.to_str().unwrap()],
        vec!["--json", "add-app", proj_041.to_str().unwrap()],
        vec!["--json", "add-package", proj_041.to_str().unwrap()],
        vec!["--json", "register", proj_041.to_str().unwrap()],
        vec!["--json", "describe", proj_041.to_str().unwrap()],
        vec!["--json", "status", proj_041.to_str().unwrap()],
        vec![
            "exec",
            "--root",
            proj_041.to_str().unwrap(),
            "--component",
            "apps/api",
            "--",
            "true",
        ],
        vec!["task", "check", "--root", proj_041.to_str().unwrap()],
    ] {
        let mut full_args = vec!["--cache-dir", primed_cache.to_str().unwrap()];
        full_args.extend_from_slice(&cmd_args);
        let out = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args(&full_args)
            .env_clear()
            .env("CI", "true")
            .output()
            .unwrap();
        assert!(out.status.success(), "{cmd_args:?}");
        assert_eq!(decoded(&out)["result"]["engine"], "0.4.1", "{cmd_args:?}");
    }

    // With primed cache, describe and status on 99.0.0 delegate to 99.0.0 engine.
    for meta_cmd in ["describe", "status"] {
        let out = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args([
                "--json",
                "--cache-dir",
                primed_cache.to_str().unwrap(),
                meta_cmd,
                proj_990.to_str().unwrap(),
            ])
            .env_clear()
            .env("CI", "true")
            .output()
            .unwrap();
        assert!(out.status.success(), "{meta_cmd}");
        assert_eq!(decoded(&out)["result"]["engine"], "99.0.0", "{meta_cmd}");
    }

    // 4. With EMPTY cache and dead network proxy, describe and status NEVER touch the network:
    //    - 0.4.1 and 0.5.0 succeed locally;
    //    - unprimed 99.0.0 fails locally with "run make setup".
    let empty_cache = temp.path().join("empty-cache");
    for (proj, expected_ver) in [(&proj_041, "0.4.1"), (&proj_050, "0.5.0")] {
        let desc = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args([
                "--json",
                "--cache-dir",
                empty_cache.to_str().unwrap(),
                "describe",
                proj.to_str().unwrap(),
            ])
            .env_clear()
            .env("CI", "true")
            .env("PATH", &fake_bin)
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert!(
            desc.status.success(),
            "describe {expected_ver}: {}",
            String::from_utf8_lossy(&desc.stdout)
        );
        assert_eq!(decoded(&desc)["result"]["version"], expected_ver);

        let stat = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args([
                "--json",
                "--cache-dir",
                empty_cache.to_str().unwrap(),
                "status",
                proj.to_str().unwrap(),
            ])
            .env_clear()
            .env("CI", "true")
            .env("PATH", &fake_bin)
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert!(
            stat.status.success(),
            "status {expected_ver}: {}",
            String::from_utf8_lossy(&stat.stdout)
        );
        assert_eq!(decoded(&stat)["ok"], true);
        assert!(!empty_cache.exists());
    }

    for meta_cmd in ["describe", "status"] {
        let unprimed = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .args([
                "--json",
                "--cache-dir",
                empty_cache.to_str().unwrap(),
                meta_cmd,
                proj_990.to_str().unwrap(),
            ])
            .env_clear()
            .env("CI", "true")
            .env("PATH", &fake_bin)
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert!(!unprimed.status.success(), "{meta_cmd}");
        assert!(
            decoded(&unprimed)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("run make setup"),
            "{meta_cmd}"
        );
    }

    // 5. If the genuine v0.4.1 baseline ELF is staged locally, prove it parses 0.5.0 release.json
    //    with minSupportedFromVersion and forward-delegates cleanly to 0.5.0.
    let genuine_041 = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts/baseline/v0.4.1/payload/devkit");
    if genuine_041.is_file() {
        let rel_050_marker = temp.path().join("release-0.5.0-marker");
        fs::create_dir_all(&rel_050_marker).unwrap();
        let marker_elf = minimal_static_x86_64_elf(
            b"{\"schemaVersion\":1,\"ok\":true,\"command\":\"delegated\",\"result\":{\"engine\":\"0.5.0-target\"}}\n",
        );
        let marker_sha = digest(&marker_elf);
        write_atomic(&rel_050_marker.join("devkit"), &marker_elf, 0o755).unwrap();
        let recipe_hash = "c".repeat(64);
        let tc = format!(
            "DEVKIT_VERSION=0.5.0\nDEVKIT_ENGINE_SHA256={marker_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
        );
        write_atomic(
            &rel_050_marker.join("runtime/toolchains.env"),
            tc.as_bytes(),
            0o644,
        )
        .unwrap();
        let env_b = b"env-0.5.0";
        let env_s = format!("sha512-{}", STANDARD.encode(Sha512::digest(env_b)));
        write_atomic(&rel_050_marker.join("npm/env-0.5.0.tgz"), env_b, 0o644).unwrap();
        write_json(
            &rel_050_marker.join("release.json"),
            &devkit::model::Release {
                schema_version: 1,
                version: "0.5.0".into(),
                min_supported_from_version: Some("0.4.0".into()),
                rust_version: "1.99.1".into(),
                environment: devkit::model::Environment {
                    name: "@docouno/env".into(),
                    version: "0.5.0".into(),
                    archive: "npm/env-0.5.0.tgz".into(),
                    integrity: env_s,
                },
                files: file_map(&rel_050_marker).unwrap(),
                migrations: vec![],
            },
        )
        .unwrap();
        let from_041 = Command::new(&genuine_041)
            .args([
                "--json",
                "--cache-dir",
                temp.path().join("genuine-041-cache").to_str().unwrap(),
                "update",
                proj_041.to_str().unwrap(),
                "--to",
                "0.5.0",
                "--release-root",
                rel_050_marker.to_str().unwrap(),
            ])
            .env_clear()
            .env("CI", "true")
            .output()
            .unwrap();
        assert!(
            from_041.status.success(),
            "genuine 0.4.1 failed to forward-delegate to 0.5.0: stdout={} stderr={}",
            String::from_utf8_lossy(&from_041.stdout),
            String::from_utf8_lossy(&from_041.stderr)
        );
        assert_eq!(decoded(&from_041)["result"]["engine"], "0.5.0-target");
    }
}

#[test]
fn info_and_stand_enforce_version_floors_and_delegate_offline_to_cached_accepted_engine() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha512};
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let temp = TempDir::new().unwrap();
    let cache_sentinel = temp.path().join("invalid-cache-file");
    fs::write(&cache_sentinel, "sentinel").unwrap();

    // 1. Pre-0.4.0 checkouts (`schemaVersion: 1` and `0.3.2`) fail before locking with UNSUPPORTED_PROJECT_VERSION and 0 mutations.
    for (name, manifest) in [
        ("legacy-s1", serde_json::json!({"schemaVersion": 1})),
        (
            "legacy-032",
            serde_json::json!({"schemaVersion": 2, "version": "0.3.2"}),
        ),
    ] {
        let dir = temp.path().join(name);
        fs::create_dir_all(dir.join(".devkit")).unwrap();
        write_json(&dir.join(MANIFEST_PATH), &manifest).unwrap();
        for args in [
            vec!["info", ".", "--json"],
            vec!["stand", "reserve", ".", "--json"],
            vec!["stand", "remove", ".", "--json"],
        ] {
            let out = invoke(&dir, &args);
            assert!(!out.status.success(), "{name} {args:?}");
            assert_eq!(
                decoded(&out)["error"]["code"],
                "UNSUPPORTED_PROJECT_VERSION"
            );
            assert!(!dir.join(".devkit/state").exists(), "{name} {args:?}");
        }
    }

    // 2. On a 0.4.1 checkout: `stand reserve|remove` (CLI and direct runtime fns) refuse pre-lock with UNSUPPORTED_PROJECT_VERSION,
    //    while `devkit info . --json` succeeds locally with real apps/packages and `readiness: "unknown"`.
    let proj_041 = temp.path().join("proj-041");
    fs::create_dir_all(proj_041.join(".devkit/runtime")).unwrap();
    let dummy_id = serde_json::json!({"sha256": "0".repeat(64), "mode": 420});
    write_json(
        &proj_041.join(MANIFEST_PATH),
        &serde_json::json!({
            "schemaVersion": 2,
            "version": "0.4.1",
            "releaseIdentity": "a".repeat(64),
            "generation": uuid::Uuid::new_v4().to_string(),
            "packageMode": "registry",
            "managed": {
                ".devkit/runtime.sha256": dummy_id,
                ".devkit/runtime/toolchains.env": dummy_id,
                ".devkit/project.mk": dummy_id,
                ".devkit/compose.apps.yaml": dummy_id,
                ".devkit/tooling-profiles.json": dummy_id
            },
            "components": {
                "apps/web": {
                    "role": "app",
                    "toolchain": "node",
                    "toolingProfile": "web",
                    "origin": { "template": "react-web", "version": "0.4.1" },
                    "toolDependencies": {},
                    "dependencies": []
                },
                "packages/ui": {
                    "role": "library",
                    "toolchain": "node",
                    "toolingProfile": "react-ui",
                    "origin": { "template": "react-ui", "version": "0.4.1" },
                    "toolDependencies": {},
                    "dependencies": []
                }
            }
        }),
    )
    .unwrap();

    for action in ["reserve", "remove"] {
        let out = invoke(&proj_041, &["stand", action, ".", "--json"]);
        assert!(!out.status.success(), "{action}");
        let body = decoded(&out);
        assert_eq!(body["error"]["code"], "UNSUPPORTED_PROJECT_VERSION");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("requires Devkit 0.5.0 or newer")
        );
        assert!(!proj_041.join(".devkit/state").exists());
    }
    let runner = devkit::processes::ProcessRunner::new().unwrap();
    let r_err = devkit::runtime::reserve_stand(&proj_041, None, &runner).unwrap_err();
    assert_eq!(
        devkit::cli::error_code(&r_err),
        "UNSUPPORTED_PROJECT_VERSION"
    );
    let rm_err = devkit::runtime::remove_stand(&proj_041, &runner).unwrap_err();
    assert_eq!(
        devkit::cli::error_code(&rm_err),
        "UNSUPPORTED_PROJECT_VERSION"
    );
    assert!(!proj_041.join(".devkit/state").exists());

    let info_041 = invoke(&proj_041, &["info", ".", "--json"]);
    assert!(
        info_041.status.success(),
        "{}",
        String::from_utf8_lossy(&info_041.stderr)
    );
    let info_041_res = &decoded(&info_041)["result"];
    assert_eq!(info_041_res["version"], "0.4.1");
    assert_eq!(info_041_res["apps"], serde_json::json!(["web"]));
    assert_eq!(info_041_res["packages"], serde_json::json!(["ui"]));
    assert_eq!(info_041_res["readiness"], "unknown");
    assert!(!proj_041.join(".devkit/state").exists());

    // 3. >= 0.5.0 delegation (including unknown future `toolingProfile: "future-wasm"`), local cache priming without network,
    //    unavailable-cache graceful behavior, and non-blocking FIFO safety.
    let cache_dir = temp.path().join("cache");
    let rel_future = temp.path().join("release-99.0.0");
    fs::create_dir_all(&rel_future).unwrap();
    let delegated_stdout =
        b"{\"schemaVersion\":1,\"ok\":true,\"command\":\"delegated\",\"result\":{\"engine\":\"99.0.0\"}}\n";
    let elf_bytes = minimal_static_x86_64_elf(delegated_stdout);
    let elf_sha = digest(&elf_bytes);
    write_atomic(&rel_future.join("devkit"), &elf_bytes, 0o755).unwrap();
    let env_bytes = b"future env archive";
    let env_sri = format!("sha512-{}", STANDARD.encode(Sha512::digest(env_bytes)));
    write_atomic(&rel_future.join("npm/env-99.0.0.tgz"), env_bytes, 0o644).unwrap();
    let recipe_hash = "d".repeat(64);
    let toolchains = format!(
        "DEVKIT_VERSION=99.0.0\nDEVKIT_ENGINE_SHA256={elf_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(
        &rel_future.join("runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    write_json(
        &rel_future.join("release.json"),
        &devkit::model::Release {
            schema_version: 1,
            version: "99.0.0".into(),
            min_supported_from_version: Some("0.4.0".into()),
            rust_version: "1.99.1".into(),
            environment: devkit::model::Environment {
                name: "@docouno/env".into(),
                version: "99.0.0".into(),
                archive: "npm/env-99.0.0.tgz".into(),
                integrity: env_sri,
            },
            files: file_map(&rel_future).unwrap(),
            migrations: vec![],
        },
    )
    .unwrap();
    let cache = devkit::cache::ReleaseCache::new(Some(&cache_dir), &runner).unwrap();
    let admitted = cache.import(&rel_future, "99.0.0", None).unwrap();
    // Remove `engines/` so `stand` and `info` must prime locally from `releases/99.0.0/<id>` without network!
    fs::remove_dir_all(cache_dir.join("engines")).unwrap();

    let proj_future = temp.path().join("proj-future");
    fs::create_dir_all(proj_future.join(".devkit/runtime")).unwrap();
    write_atomic(
        &proj_future.join(".devkit/runtime/toolchains.env"),
        toolchains.as_bytes(),
        0o644,
    )
    .unwrap();
    let tc_id = identity(&proj_future.join(".devkit/runtime/toolchains.env"))
        .unwrap()
        .unwrap();
    write_json(
        &proj_future.join(MANIFEST_PATH),
        &serde_json::json!({
            "schemaVersion": 2,
            "version": "99.0.0",
            "releaseIdentity": admitted.identity,
            "generation": uuid::Uuid::new_v4().to_string(),
            "packageMode": "local",
            "managed": {
                ".devkit/runtime/toolchains.env": tc_id
            },
            "components": {
                "apps/wasm": {
                    "role": "app",
                    "toolchain": "rust",
                    "toolingProfile": "future-wasm",
                    "origin": { "template": "wasm-app", "version": "99.0.0" },
                    "toolDependencies": {},
                    "dependencies": []
                }
            }
        }),
    )
    .unwrap();

    // Uncached engine: `stand` fails fast locally ("unavailable; run make setup"), while `info` falls back gracefully!
    let empty_cache = temp.path().join("empty-cache");
    for args in [vec!["stand", "reserve", "."], vec!["stand", "remove", "."]] {
        let miss = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .current_dir(&proj_future)
            .args(["--json", "--cache-dir", empty_cache.to_str().unwrap()])
            .args(&args)
            .env_clear()
            .env("CI", "true")
            .output()
            .unwrap();
        assert!(!miss.status.success(), "{args:?}");
        assert!(
            decoded(&miss)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("The accepted native engine is unavailable; run make setup")
        );
        assert!(!proj_future.join(".devkit/state").exists());
    }
    // `info` with sentinel cache-file and unset HOME does not crash; returns ok=true, readiness="unknown".
    let info_uncached = Command::new(env!("CARGO_BIN_EXE_devkit"))
        .current_dir(&proj_future)
        .args(["info", ".", "--json"])
        .env_clear()
        .env("CI", "true")
        .env("DEVKIT_CACHE_DIR", &cache_sentinel)
        .output()
        .unwrap();
    assert!(info_uncached.status.success());
    assert_eq!(decoded(&info_uncached)["result"]["readiness"], "unknown");

    // Cached 99.0.0 release (with `engines/` initially absent!): `stand reserve`, `stand remove`, and `info` prime & delegate offline!
    for (idx, args) in [
        vec!["stand", "reserve", "."],
        vec!["info", "."],
        vec!["stand", "remove", "."],
    ]
    .into_iter()
    .enumerate()
    {
        if idx == 1 {
            // Also verify `info` itself primes `engines/` when absent!
            fs::remove_dir_all(cache_dir.join("engines")).unwrap();
        }
        let delegated = Command::new(env!("CARGO_BIN_EXE_devkit"))
            .current_dir(&proj_future)
            .args(["--json", "--cache-dir", cache_dir.to_str().unwrap()])
            .args(&args)
            .env_clear()
            .env("CI", "true")
            .output()
            .unwrap();
        assert!(
            delegated.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&delegated.stdout),
            String::from_utf8_lossy(&delegated.stderr)
        );
        assert_eq!(decoded(&delegated)["result"]["engine"], "99.0.0");
    }

    // FIFO manifest.json never blocks `devkit info`!
    fs::remove_file(proj_future.join(MANIFEST_PATH)).unwrap();
    let fifo_path = CString::new(proj_future.join(MANIFEST_PATH).as_os_str().as_bytes()).unwrap();
    // SAFETY: valid NUL-terminated path in disposable tempdir.
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let fifo_info = invoke(&proj_future, &["info", ".", "--json"]);
    assert!(fifo_info.status.success());
    assert_eq!(decoded(&fifo_info)["result"]["readiness"], "unknown");
}
