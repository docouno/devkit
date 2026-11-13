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
        serde_json::json!(["fullstack-desktop", "design-system"])
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
