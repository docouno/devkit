//! Native Rust policy, used by the same ELF inside the pinned task container.
use crate::{
    Result,
    processes::{OutputMode, ProcessOutput, ProcessRunner},
};
use anyhow::{Context, bail, ensure};
use std::{
    collections::BTreeSet,
    ffi::OsString,
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
    for pair in crate::runtime::cargo_safety_env() {
        if let Some((key, value)) = pair.split_once('=') {
            c.env(key, value);
        }
    }
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
const ROOT: &str = "/workspace";
const MANIFEST: &str = "/workspace/Cargo.toml";

/// The host resolves members once during admission. The in-container gate
/// checks their directory boundaries before it invokes any tool on the sources.
struct Gate<'a> {
    command: &'a str,
    members: Vec<&'a str>,
    fmt_packages: Vec<&'a str>,
    packages: Vec<&'a str>,
}
impl<'a> Gate<'a> {
    fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut args = args
            .iter()
            .map(|arg| arg.to_str().context("Expected UTF-8 Rust gate argument"));
        let command = args.next().context("Expected a Rust task")??;
        ensure!(
            ["setup", "check", "fix", "test", "build"].contains(&command),
            "Unknown Rust command: {command}"
        );
        let mut workspace_root = None;
        let mut component = None;
        let mut members = Vec::new();
        let mut fmt_packages = Vec::new();
        let mut packages = Vec::new();
        while let Some(flag) = args.next() {
            let flag = flag?;
            let value = args
                .next()
                .with_context(|| format!("Missing value for {flag}"))??;
            match flag {
                "--workspace-root" if workspace_root.is_none() => workspace_root = Some(value),
                "--component" if component.is_none() => {
                    component = Some(crate::catalog::valid_component_path(value)?)
                }
                "--member" => members.push(value),
                "--fmt-package" => fmt_packages.push(value),
                "--package" => packages.push(value),
                _ => bail!("Unknown or repeated Rust gate option: {flag}"),
            }
        }
        ensure!(
            workspace_root == Some(ROOT),
            "Rust gates require --workspace-root /workspace"
        );
        ensure!(
            std::env::current_dir()? == Path::new(ROOT),
            "Rust gates must run from /workspace"
        );
        if command == "setup" {
            ensure!(
                component.is_none()
                    && members.is_empty()
                    && fmt_packages.is_empty()
                    && packages.is_empty(),
                "Rust workspace setup does not accept component selectors"
            );
        } else {
            let component = component.context("Rust gate requires a component")?;
            ensure!(
                !members.is_empty()
                    && members.len() == packages.len()
                    && members.len() == fmt_packages.len(),
                "Provide --member, --fmt-package and --package for each selected workspace member"
            );
            let mut ancestors = BTreeSet::new();
            for ((member, &name), &pkgid) in members.iter().zip(&fmt_packages).zip(&packages) {
                let rel = Path::new(member);
                ensure!(
                    rel.starts_with(component)
                        && rel
                            .components()
                            .all(|part| matches!(part, std::path::Component::Normal(_))),
                    "Rust gate member {member} is outside {component}"
                );
                let dir = crate::files::input_path(Path::new(ROOT), rel)?;
                ensure!(
                    regular(&dir.join("Cargo.toml")),
                    "Missing member manifest: {member}/Cargo.toml"
                );
                // cargo fmt accepts only a bare package name. Require both
                // selectors to identify the same local crate before allowing fix.
                let mut url = url::Url::from_file_path(&dir)
                    .map_err(|_| anyhow::anyhow!("Invalid Rust member path: {member}"))?;
                url.set_fragment(Some(&format!("{name}@")));
                ensure!(
                    !name.is_empty()
                        && !name.starts_with('-')
                        && pkgid
                            .strip_prefix(&format!("path+{url}"))
                            .is_some_and(|version| !version.is_empty()),
                    "Inconsistent Cargo package selectors for {member}"
                );
                ancestors.extend(rel.ancestors().filter(|path| !path.as_os_str().is_empty()));
            }
            for dir in ancestors {
                let ancestor_manifest =
                    crate::files::input_path(Path::new(ROOT), dir.join("Cargo.toml"))?;
                if regular(&ancestor_manifest) {
                    let parsed: toml::Value =
                        toml::from_str(&fs::read_to_string(&ancestor_manifest)?).with_context(
                            || format!("Invalid Cargo manifest: {}", ancestor_manifest.display()),
                        )?;
                    ensure!(
                        parsed.get("workspace").is_none(),
                        "Nested Cargo workspace at {}",
                        ancestor_manifest.display()
                    );
                }
                for file in [
                    "Cargo.lock",
                    "rust-toolchain",
                    "rust-toolchain.toml",
                    ".rustfmt.toml",
                    "rustfmt.toml",
                    ".cargo/config",
                    ".cargo/config.toml",
                ] {
                    let path = dir.join(file);
                    let path = crate::files::input_path(Path::new(ROOT), &path)?;
                    ensure!(
                        !crate::files::exists(&path)?,
                        "Rust workspace setting is shadowed by {}",
                        path.display()
                    );
                }
            }
        }
        Ok(Self {
            command,
            members,
            fmt_packages,
            packages,
        })
    }
    fn args(&self, task: &'static str) -> Vec<&str> {
        let mut args = vec![task, "--manifest-path", MANIFEST];
        for &package in if task == "fmt" {
            &self.fmt_packages
        } else {
            &self.packages
        } {
            args.extend(["-p", package]);
        }
        args
    }
}

pub fn run(args: &[OsString], runner: &ProcessRunner) -> Result<()> {
    let gate = Gate::parse(args)?;
    let command = gate.command;
    let version = std::env::var("DEVKIT_RUST_VERSION").unwrap_or_default();
    ensure!(
        version == "1.99.1",
        "This runtime requires DEVKIT_RUST_VERSION=1.99.1"
    );
    ensure!(
        std::env::var_os("RUSTUP_TOOLCHAIN").is_none_or(|v| v.is_empty()),
        "RUSTUP_TOOLCHAIN masks rust-toolchain.toml; remove the override"
    );
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME")
        && !cargo_home.is_empty()
    {
        crate::runtime::assert_clean_cargo_home(Path::new(&cargo_home))?;
    }
    let cwd = Path::new(ROOT);
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "rustfmt.toml",
    ] {
        ensure!(
            regular(&cwd.join(name)),
            "missing {name}; run this command from the Cargo workspace root"
        );
    }
    for (name, conflict) in [
        ("rust-toolchain", "rust-toolchain.toml"),
        (".rustfmt.toml", "rustfmt.toml"),
    ] {
        ensure!(
            !crate::files::exists(&cwd.join(name))?,
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
            "the workspace toolchain is unavailable; run make setup with a compatible devkit release",
        )?;
    ensure!(
        active.text()?.starts_with(&format!("{version}-"))
            && active.text()?.contains("/workspace/rust-toolchain.toml"),
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
        let output = if program == "cargo" {
            runner.capture(&mut cargo(&args))?
        } else {
            runner.capture(
                Command::new(program)
                    .args(args)
                    .env("RUSTUP_AUTO_INSTALL", "0"),
            )?
        };
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
        "Unexpected Cargo workspace authority: {}; expected /workspace/Cargo.toml",
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
        let mut args = gate.args("fmt");
        args.push("--");
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
        "setup" => cargo_gate(runner, &["fetch", "--manifest-path", MANIFEST, "--locked"]),
        "build" => {
            let mut args = gate.args("build");
            args.extend(["--locked", "--offline"]);
            cargo_gate(runner, &args)
        }
        "fix" => Ok(()),
        "check" | "test" => {
            if command == "check" {
                let mut args = gate.args("clippy");
                args.extend([
                    "--all-targets",
                    "--locked",
                    "--offline",
                    "--",
                    "-D",
                    "warnings",
                ]);
                cargo_gate(runner, &args)?;
            }
            let (non_ignored, ignored_count) = discover_tests(&gate, runner)?;
            ensure!(
                non_ignored > 0,
                "no non-ignored native Rust test cases found; add a non-ignored unit, integration or documentation test"
            );
            println!(
                "devkit: Rust native test cases: {non_ignored} non-ignored, {ignored_count} ignored; discovery may include compile-only doctests"
            );
            if command == "check" {
                return Ok(());
            }
            let mut args = gate.args("test");
            args.extend(["--locked", "--offline"]);
            cargo_gate(runner, &args)
        }
        _ => unreachable!(),
    }
}

#[derive(Default)]
struct CustomHarness {
    lib_harness_false: bool,
    bins_harness_false: BTreeSet<String>,
    tests_harness_false: BTreeSet<String>,
}

impl CustomHarness {
    fn any(&self) -> bool {
        self.lib_harness_false
            || !self.bins_harness_false.is_empty()
            || !self.tests_harness_false.is_empty()
    }
}

fn inspect_custom_harnesses(
    members: &[&str],
) -> Result<std::collections::BTreeMap<String, CustomHarness>> {
    let mut map = std::collections::BTreeMap::new();
    for &member in members {
        let manifest_path = Path::new(ROOT).join(member).join("Cargo.toml");
        let parsed: toml::Value = toml::from_str(&fs::read_to_string(&manifest_path)?)
            .with_context(|| format!("Invalid Cargo manifest: {}", manifest_path.display()))?;
        let mut info = CustomHarness::default();
        if parsed
            .get("lib")
            .and_then(|v| v.get("harness"))
            .and_then(toml::Value::as_bool)
            == Some(false)
        {
            info.lib_harness_false = true;
        }
        for (section, is_test) in [("bin", false), ("test", true)] {
            if let Some(entries) = parsed.get(section).and_then(toml::Value::as_array) {
                for entry in entries {
                    if entry.get("harness").and_then(toml::Value::as_bool) == Some(false)
                        && let Some(name) = entry.get("name").and_then(toml::Value::as_str)
                    {
                        if is_test {
                            info.tests_harness_false.insert(name.to_owned());
                        } else {
                            info.bins_harness_false.insert(name.to_owned());
                        }
                    }
                }
            }
        }
        if info.any() {
            map.insert(member.to_owned(), info);
        }
    }
    Ok(map)
}

fn default_features_enabled(
    features: &std::collections::BTreeMap<String, Vec<String>>,
) -> BTreeSet<String> {
    let mut enabled = BTreeSet::new();
    if !features.contains_key("default") {
        return enabled;
    }
    let mut queue = std::collections::VecDeque::from(["default".to_owned()]);
    while let Some(feat) = queue.pop_front() {
        if !enabled.insert(feat.clone()) {
            continue;
        }
        if let Some(deps) = features.get(&feat) {
            for item in deps {
                if item.starts_with("dep:") {
                    continue;
                }
                if let Some((dep_name, _)) = item.split_once('/') {
                    if dep_name.ends_with('?') {
                        continue;
                    }
                    if features.contains_key(dep_name) && !enabled.contains(dep_name) {
                        queue.push_back(dep_name.to_owned());
                    }
                } else if !enabled.contains(item) {
                    queue.push_back(item.clone());
                }
            }
        }
    }
    enabled
}

fn list_test_count(runner: &ProcessRunner, base_args: &[&str], ignored: bool) -> Result<usize> {
    let mut args = base_args.to_vec();
    args.extend([
        "--locked",
        "--offline",
        "--",
        "--list",
        "--format=terse",
        "--color",
        "never",
    ]);
    if ignored {
        args.push("--ignored");
    }
    let output = captured(runner, &mut cargo(&args))?;
    if output.code != 0 {
        io::stdout().write_all(&output.stdout)?;
        eprintln!("devkit: Rust test discovery failed; run make setup if dependencies are missing");
    }
    let output = output.success("Rust test discovery")?;
    Ok(output
        .text()?
        .lines()
        .filter(|line| line.ends_with(": test"))
        .count())
}

fn discover_tests(gate: &Gate<'_>, runner: &ProcessRunner) -> Result<(usize, usize)> {
    let custom = inspect_custom_harnesses(&gate.members)?;
    if custom.is_empty() {
        let base = gate.args("test");
        let total = list_test_count(runner, &base, false)?;
        let ignored = list_test_count(runner, &base, true)?;
        ensure!(
            ignored <= total,
            "Inconsistent native test discovery output"
        );
        return Ok((total - ignored, ignored));
    }

    // Never pass libtest `--list` flags to `harness = false` targets (which
    // Cargo would execute directly as binaries during `make check`). Discover
    // active targets via cargo metadata (respecting `required-features`), list
    // standard harness targets, and count active `harness = false` test targets
    // as non-ignored native test entry points.
    #[derive(serde::Deserialize)]
    struct Metadata {
        packages: Vec<MetaPackage>,
    }
    #[derive(serde::Deserialize)]
    struct MetaPackage {
        manifest_path: PathBuf,
        #[serde(default)]
        features: std::collections::BTreeMap<String, Vec<String>>,
        targets: Vec<MetaTarget>,
    }
    #[derive(serde::Deserialize)]
    struct MetaTarget {
        name: String,
        kind: Vec<String>,
        test: bool,
        doctest: bool,
        #[serde(default, rename = "required-features")]
        required_features: Vec<String>,
    }
    let meta_out = captured(
        runner,
        &mut cargo(&[
            "metadata",
            "--manifest-path",
            MANIFEST,
            "--no-deps",
            "--locked",
            "--offline",
            "--format-version=1",
        ]),
    )?
    .success("Rust target discovery")?;
    let metadata: Metadata = serde_json::from_slice(&meta_out.stdout)
        .context("Cannot parse cargo metadata for test discovery")?;
    let by_manifest: std::collections::BTreeMap<PathBuf, MetaPackage> = metadata
        .packages
        .into_iter()
        .map(|p| (p.manifest_path.clone(), p))
        .collect();

    let mut total = 0usize;
    let mut ignored = 0usize;
    let mut custom_non_ignored = 0usize;
    for (&member, &pkgid) in gate.members.iter().zip(&gate.packages) {
        let harness = custom.get(member);
        let manifest_path = Path::new(ROOT).join(member).join("Cargo.toml");
        let pkg = by_manifest.get(&manifest_path).with_context(|| {
            format!("cargo metadata did not report {}", manifest_path.display())
        })?;
        let active_features = default_features_enabled(&pkg.features);
        let mut owned_args: Vec<String> = vec![
            "test".into(),
            "--manifest-path".into(),
            MANIFEST.into(),
            "-p".into(),
            pkgid.into(),
        ];
        let mut has_unit_or_integration = false;
        let mut has_doctest = false;
        for target in &pkg.targets {
            if !target
                .required_features
                .iter()
                .all(|req| active_features.contains(req))
            {
                continue;
            }
            let is_lib = target.kind.iter().any(|k| {
                matches!(
                    k.as_str(),
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            });
            if is_lib && target.doctest {
                has_doctest = true;
            }
            if !target.test {
                continue;
            }
            if is_lib {
                if harness.is_some_and(|h| h.lib_harness_false) {
                    custom_non_ignored += 1;
                } else {
                    owned_args.push("--lib".into());
                    has_unit_or_integration = true;
                }
            } else if target.kind.iter().any(|k| k == "bin") {
                if harness.is_some_and(|h| h.bins_harness_false.contains(&target.name)) {
                    custom_non_ignored += 1;
                } else {
                    owned_args.push("--bin".into());
                    owned_args.push(target.name.clone());
                    has_unit_or_integration = true;
                }
            } else if target.kind.iter().any(|k| k == "test") {
                if harness.is_some_and(|h| h.tests_harness_false.contains(&target.name)) {
                    custom_non_ignored += 1;
                } else {
                    owned_args.push("--test".into());
                    owned_args.push(target.name.clone());
                    has_unit_or_integration = true;
                }
            }
        }
        if has_unit_or_integration {
            let refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
            total += list_test_count(runner, &refs, false)?;
            ignored += list_test_count(runner, &refs, true)?;
        }
        if has_doctest {
            let doc_args = ["test", "--manifest-path", MANIFEST, "-p", pkgid, "--doc"];
            total += list_test_count(runner, &doc_args, false)?;
            ignored += list_test_count(runner, &doc_args, true)?;
        }
    }
    ensure!(
        ignored <= total,
        "Inconsistent native test discovery output"
    );
    Ok(((total - ignored) + custom_non_ignored, ignored))
}
