//! Native Rust policy, used by the same ELF inside the pinned task container.
use crate::{
    Result,
    processes::{OutputMode, ProcessOutput, ProcessRunner},
};
use anyhow::{Context, ensure};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

fn cargo(args: &[&str]) -> Command {
    let mut c = Command::new("cargo");
    c.args(args)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("CARGO_TERM_COLOR", "never");
    c
}
fn captured(runner: &ProcessRunner, c: &mut Command) -> Result<ProcessOutput> {
    let output = runner.run(c, OutputMode::Capture(16 * 1024 * 1024))?;
    io::stderr().write_all(&output.stderr)?;
    Ok(output)
}
fn ignored_config(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).lines().any(|line| {
        line.starts_with("Warning: Unknown configuration option")
            || line.starts_with("Warning: can't set ") && line.contains("unstable")
    })
}
fn cargo_gate(runner: &ProcessRunner, args: &[&str]) -> Result<()> {
    let result = runner.checked(&mut cargo(args), OutputMode::Inherit);
    if result.is_err() {
        eprintln!("devkit: Rust command failed; if dependency caches are missing, run make setup");
    }
    result.map(|_| ())
}
fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
}
pub fn run(command: &str, runner: &ProcessRunner) -> Result<()> {
    ensure!(
        ["setup", "check", "test", "build", "fix"].contains(&command),
        "Unknown Rust command: {command}"
    );
    let version = std::env::var("DEVKIT_RUST_VERSION").unwrap_or_default();
    ensure!(
        version == "1.99.1",
        "This runtime requires DEVKIT_RUST_VERSION=1.99.1"
    );
    ensure!(
        std::env::var_os("RUSTUP_TOOLCHAIN").is_none_or(|v| v.is_empty()),
        "RUSTUP_TOOLCHAIN masks rust-toolchain.toml; remove the override"
    );
    let cwd = std::env::current_dir()?;
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "rustfmt.toml",
    ] {
        ensure!(
            regular(&cwd.join(name)),
            "missing {name}; run this command from the registered Rust app directory"
        );
    }
    for (name, conflict) in [
        ("rust-toolchain", "rust-toolchain.toml"),
        (".rustfmt.toml", "rustfmt.toml"),
    ] {
        ensure!(
            fs::symlink_metadata(cwd.join(name)).is_err(),
            "{name} conflicts with the required {conflict}"
        );
    }
    let active = runner
        .capture(
            Command::new("rustup")
                .args(["show", "active-toolchain"])
                .env("RUSTUP_AUTO_INSTALL", "0"),
        )
        .context(
            "the app toolchain is unavailable; run make setup with a compatible devkit release",
        )?;
    ensure!(
        active.text()?.starts_with(&format!("{version}-"))
            && active.text()?.contains("rust-toolchain.toml"),
        "rust-toolchain.toml must select exactly {version} (active: {})",
        active.text()?.trim()
    );
    for (program, args, prefix) in [
        ("rustc", vec!["--version"], format!("rustc {version} ")),
        (
            "rustfmt",
            vec!["--version"],
            "rustfmt 1.10.0-stable ".to_owned(),
        ),
        (
            "cargo",
            vec!["clippy", "--version"],
            "clippy 0.1.99 ".to_owned(),
        ),
    ] {
        let output = runner.capture(
            Command::new(program)
                .args(args)
                .env("RUSTUP_AUTO_INSTALL", "0"),
        )?;
        ensure!(
            output.text()?.starts_with(&prefix),
            "Tool version mismatch: expected {prefix}, got {}",
            output.text()?.trim()
        );
    }
    let workspace = runner.capture(&mut cargo(&[
        "locate-project",
        "--workspace",
        "--message-format",
        "plain",
    ]))?;
    let authority = PathBuf::from(workspace.text()?.trim());
    ensure!(
        fs::canonicalize(&authority)? == fs::canonicalize(cwd.join("Cargo.toml"))?,
        "Unsupported outer Cargo workspace/lock authority: {}; registered components must use their own Cargo.lock",
        authority.display()
    );
    let scratch = tempfile::Builder::new().prefix("devkit-rust-").tempdir()?;
    let empty = scratch.path().join("empty.rs");
    fs::write(&empty, "")?;
    let config = cwd.join("rustfmt.toml");
    let output = captured(
        runner,
        Command::new("rustfmt")
            .args(["--print-config", "current"])
            .arg(empty)
            .arg("--config-path")
            .arg(&config),
    )?;
    ensure!(
        !ignored_config(&output.stderr),
        "rustfmt ignored a configuration option; use supported stable options"
    );
    output
        .code
        .eq(&0)
        .then_some(())
        .context("Cannot validate rustfmt configuration")?;
    ensure!(
        output
            .text()?
            .lines()
            .any(|line| line == "disable_all_formatting = false"),
        "rustfmt configuration disables mandatory formatting"
    );
    if ["check", "fix"].contains(&command) {
        let mut args = vec!["fmt", "--all", "--"];
        if command == "check" {
            args.push("--check");
        }
        args.extend([
            "--config-path",
            config.to_str().context("Non-UTF8 rustfmt path")?,
        ]);
        let output = captured(runner, &mut cargo(&args))?;
        io::stdout().write_all(&output.stdout)?;
        ensure!(
            !ignored_config(&output.stderr),
            "rustfmt ignored a configuration option; use supported stable options"
        );
        output.success("Rust formatting")?;
    }
    match command {
        "setup" => cargo_gate(runner, &["fetch", "--locked"]),
        "build" => cargo_gate(runner, &["build", "--workspace", "--locked", "--offline"]),
        "fix" => Ok(()),
        "check" | "test" => {
            if command == "check" {
                cargo_gate(
                    runner,
                    &[
                        "clippy",
                        "--workspace",
                        "--all-targets",
                        "--locked",
                        "--offline",
                        "--",
                        "-D",
                        "warnings",
                    ],
                )?;
            }
            let mut counts = Vec::new();
            for ignored in [false, true] {
                let mut args = vec![
                    "test",
                    "--workspace",
                    "--locked",
                    "--offline",
                    "--",
                    "--list",
                    "--color",
                    "never",
                ];
                if ignored {
                    args.push("--ignored");
                }
                let output = captured(runner, &mut cargo(&args))?;
                if output.code != 0 {
                    io::stdout().write_all(&output.stdout)?;
                    eprintln!(
                        "devkit: Rust test discovery failed; run make setup if dependencies are missing"
                    );
                }
                let output = output.success("Rust test discovery")?;
                counts.push(
                    output
                        .text()?
                        .lines()
                        .filter(|line| line.ends_with(": test"))
                        .count(),
                );
            }
            ensure!(
                counts[1] <= counts[0],
                "Inconsistent native test discovery output"
            );
            ensure!(
                counts[0] > counts[1],
                "no non-ignored native Rust test cases found; add a non-ignored unit, integration or documentation test"
            );
            println!(
                "devkit: Rust native test cases: {} non-ignored, {} ignored; discovery may include compile-only doctests",
                counts[0] - counts[1],
                counts[1]
            );
            cargo_gate(runner, &["test", "--workspace", "--locked", "--offline"])
        }
        _ => unreachable!(),
    }
}
