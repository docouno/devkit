//! Author-only native lifecycle entry with a local npm registry fixture.
//! The shipped CLI does not expose this test registry override.
use devkit::{
    Result,
    files::*,
    model::PackageMode,
    processes::{OutputMode, OwnedContainer, ProcessRunner},
    project::{DockerPnpmResolver, InitOptions, UpdateOptions, init_project, update_project},
    state::{CHECKSUM_PATH, MANIFEST_PATH, read_project_state},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 4,
        "Expected init|update RELEASE TARGET REGISTRY"
    );
    let release = PathBuf::from(&args[1]);
    let target = PathBuf::from(&args[2]);
    match args[0].as_str() {
        "init" => {
            let metadata: Value = read_json(&release.join("release.json"))?;
            if SemVer::parse(
                metadata["version"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Release version"))?,
            )? < SemVer::parse("0.5.0")?
            {
                return init_legacy_registry(&release, &target, &args[3]);
            }
            init_project(
                &release,
                &target,
                &InitOptions {
                    template: Some("node-api".into()),
                    package_mode: Some(PackageMode::Registry),
                    registry: Some(&args[3]),
                    ..Default::default()
                },
                &DockerPnpmResolver::default(),
            )?;
        }
        "update" => {
            update_project(
                &release,
                &target,
                &UpdateOptions {
                    registry: Some(&args[3]),
                    ..Default::default()
                },
                &DockerPnpmResolver::default(),
            )?;
        }
        _ => anyhow::bail!("Expected init or update"),
    }
    Ok(())
}

fn init_legacy_registry(release: &Path, target: &Path, registry: &str) -> Result<()> {
    let runner = ProcessRunner::new()?;
    let metadata: Value = read_json(&release.join("release.json"))?;
    let version = metadata["version"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Release version"))?;
    runner.run(
        Command::new(release.join("devkit"))
            .args(["--non-interactive", "init"])
            .arg(target)
            .args(["--to", version, "--release-root"])
            .arg(release)
            .args(["--template", "node-api", "--package-source", "local"]),
        OutputMode::Inherit,
    )?;
    let mut state = read_project_state(target)?;
    state.package_mode = PackageMode::Registry;
    state
        .components
        .get_mut("apps/main")
        .ok_or_else(|| anyhow::anyhow!("Main component"))?
        .tool_dependencies
        .insert("@docouno/env".into(), version.into());
    let manifest_path = target.join("apps/main/package.json");
    let mut manifest: Value = read_json(&manifest_path)?;
    manifest["devDependencies"]["@docouno/env"] = json!(version);
    write_json(&manifest_path, &manifest)?;
    let profiles_path = target.join(".devkit/tooling-profiles.json");
    let mut profiles: Value = read_json(&profiles_path)?;
    profiles["packageMode"] = json!("registry");
    profiles["profiles"]["node"]["@docouno/env"] = json!(version);
    write_json(&profiles_path, &profiles)?;
    state.managed.insert(
        ".devkit/tooling-profiles.json".into(),
        identity(&profiles_path)?.ok_or_else(|| anyhow::anyhow!("Tool profiles"))?,
    );
    state
        .managed
        .retain(|name, _| !name.starts_with(".devkit/npm/"));
    fs::remove_dir_all(target.join(".devkit/npm"))?;
    state.managed.remove(CHECKSUM_PATH);
    let checksums = state
        .managed
        .iter()
        .map(|(name, id)| format!("{}  {name}\n", id.sha256))
        .collect::<String>();
    write_atomic(&target.join(CHECKSUM_PATH), checksums.as_bytes(), 0o644)?;
    state.managed.insert(
        CHECKSUM_PATH.into(),
        identity(&target.join(CHECKSUM_PATH))?.ok_or_else(|| anyhow::anyhow!("Checksums"))?,
    );
    write_json(&target.join(MANIFEST_PATH), &state)?;
    // npm can reuse a file identity when its version satisfies the new registry
    // specifier. Remove only that env selection before refreshing the v3 lock.
    let lock_path = target.join("package-lock.json");
    let mut lock: Value = read_json(&lock_path)?;
    lock["packages"]
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Legacy packages"))?
        .retain(|location, _| !location.ends_with("node_modules/@docouno/env"));
    write_json(&lock_path, &lock)?;
    // The baseline's real npm resolves its original v3 graph, independently of the new engine.
    let pins =
        devkit::runtime::ToolPins::parse(&fs::read(release.join("runtime/toolchains.env"))?)?;
    let budget = devkit::resources::HostResourceBudget::probe_with_docker(
        &runner,
        &mut Command::new("docker"),
    )?;
    let spec = budget.for_npm_resolver();
    let token = uuid::Uuid::new_v4().to_string();
    let name = format!("devkit-registry-baseline-{token}");
    let meta = fs::metadata(target)?;
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--rm",
        "--init",
        "--name",
        &name,
        "--label",
        &format!("io.docouno.devkit.invocation={token}"),
        "--network",
        "host",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
        "--tmpfs",
        "/tmp:mode=1777,size=256m",
        "--user",
        &format!("{}:{}", meta.uid(), meta.gid()),
        "--mount",
        &devkit::runtime::bind_mount(target, Path::new("/workspace"), false)?,
        "--workdir",
        "/workspace",
        "--env",
        "HOME=/tmp/home",
        "--env",
        "npm_config_cache=/tmp/npm-cache",
        "--env",
        "npm_config_userconfig=/tmp/user.npmrc",
        "--env",
        "npm_config_globalconfig=/tmp/global.npmrc",
        "--env",
        &format!("npm_config_registry={registry}"),
    ]);
    spec.write_docker_args(&mut command);
    command.arg(&pins.node_image).args([
        "npm",
        "install",
        "--package-lock-only",
        // The migrated graph must clear pnpm's one-day cutoff before testing the fresh env release.
        "--min-release-age=1",
        "--ignore-scripts",
        "--include=dev",
        "--strict-peer-deps",
        "--no-audit",
        "--no-fund",
    ]);
    runner.run_owned(
        &mut command,
        OutputMode::Inherit,
        &OwnedContainer::new(name, token),
    )?;
    let mut lock: Value = read_json(&lock_path)?;
    for entry in lock["packages"]
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Legacy packages"))?
        .values_mut()
    {
        if let Some(url) = entry["resolved"]
            .as_str()
            .and_then(|url| url.strip_prefix(registry))
        {
            entry["resolved"] = json!(format!("https://registry.npmjs.org/{url}"));
        }
    }
    write_json(&lock_path, &lock)?;
    Ok(())
}
