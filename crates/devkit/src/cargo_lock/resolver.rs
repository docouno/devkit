use crate::{
    Result,
    catalog::rust_base_image,
    files::{project_root, safe_path},
    model::ToolingProfile,
    processes::{OutputMode, OwnedContainer, ProcessRunner},
    release::AdmittedRelease,
    resources::HostResourceBudget,
    runtime::{
        assert_clean_cargo_home, bind_mount, cargo_safety_env, mount_sensitive_workspace_masks,
    },
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

pub trait CargoLockResolver {
    /// Resolve manifests in the transaction's staging directory. The caller
    /// checks the resulting graph against previously accepted selections.
    fn resolve_cargo(&self, root: &Path, release: &AdmittedRelease, offline: bool) -> Result<()>;

    /// Resolve otherwise indistinguishable checkout paths in an already locked
    /// graph. All member features and targets participate in Cargo.lock.
    fn selected_local_paths(
        &self,
        _root: &Path,
        _release: &AdmittedRelease,
        _offline: bool,
    ) -> Result<BTreeSet<String>> {
        anyhow::bail!("This Cargo resolver cannot report selected local package paths")
    }

    /// Verify the existing lock for the whole workspace without writing to the checkout.
    fn validate_locked(
        &self,
        _root: &Path,
        _release: &AdmittedRelease,
        _offline: bool,
    ) -> Result<()> {
        anyhow::bail!("This Cargo resolver does not support locked workspace validation")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockOperation {
    Update,
    Validate,
    Paths,
}
#[derive(Default)]
pub struct DockerCargoLockResolver {
    pub cache_dir: Option<PathBuf>,
    pub runner: Option<ProcessRunner>,
}
impl CargoLockResolver for DockerCargoLockResolver {
    fn resolve_cargo(&self, root: &Path, release: &AdmittedRelease, offline: bool) -> Result<()> {
        self.run(root, release, offline, LockOperation::Update)
            .map(drop)
    }

    fn selected_local_paths(
        &self,
        root: &Path,
        release: &AdmittedRelease,
        offline: bool,
    ) -> Result<BTreeSet<String>> {
        let output = self.run(root, release, offline, LockOperation::Paths)?;
        #[derive(serde::Deserialize)]
        struct Metadata {
            packages: Vec<Package>,
        }
        #[derive(serde::Deserialize)]
        struct Package {
            source: Option<String>,
            manifest_path: PathBuf,
        }
        let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
        metadata
            .packages
            .into_iter()
            .filter(|p| p.source.is_none())
            .map(|package| {
                let dir = package
                    .manifest_path
                    .strip_prefix("/workspace")?
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Invalid Cargo metadata manifest_path"))?;
                let rel = dir
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid Cargo metadata path"))?;
                anyhow::ensure!(
                    !rel.is_empty()
                        && dir
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_))),
                    "Invalid Cargo metadata path {rel}"
                );
                Ok(rel.to_owned())
            })
            .collect()
    }

    fn validate_locked(&self, root: &Path, release: &AdmittedRelease, offline: bool) -> Result<()> {
        self.run(root, release, offline, LockOperation::Validate)
            .map(drop)
    }
}

impl DockerCargoLockResolver {
    fn run(
        &self,
        root: &Path,
        release: &AdmittedRelease,
        offline: bool,
        operation: LockOperation,
    ) -> Result<crate::processes::ProcessOutput> {
        let recipe = release.bytes("runtime/rust.Dockerfile")?;
        let image = rust_base_image(
            std::str::from_utf8(&recipe)?,
            &release.manifest.rust_version,
        )?;
        let root = project_root(root)?;
        let cache = safe_path(
            &crate::cache::cache_root(self.cache_dir.as_deref())?,
            "cargo",
        )?;
        crate::shared_cache::directory(&cache)?;
        assert_clean_cargo_home(&cache)?;
        let runner = match &self.runner {
            Some(existing) => existing.clone(),
            None => ProcessRunner::new()?,
        };
        runner.check_cancelled()?;
        let ca_cert = if !offline {
            crate::native::resolve_ca_cert_path()?
                .map(|p| crate::download::additional_ca_file(&p))
                .transpose()?
        } else {
            None
        };
        let mut budget =
            HostResourceBudget::probe_with_docker(&runner, &mut Command::new("docker"))?;
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let cache_readonly =
            offline && matches!(operation, LockOperation::Paths | LockOperation::Validate);
        let label = match operation {
            LockOperation::Update => "Cargo.lock resolution",
            LockOperation::Paths => "Cargo local package selection",
            LockOperation::Validate => "Cargo.lock validation",
        };
        let build_command =
            |budget: &HostResourceBudget, owned: &OwnedContainer| -> Result<Command> {
                let spec = budget.for_cargo_resolver();
                let env_plan = budget.toolchain_env_for_task(ToolingProfile::Rust, &spec)?;
                let mut command = Command::new("docker");
                command.args([
                    "run",
                    "--rm",
                    "--init",
                    "--read-only",
                    "--name",
                    &owned.name,
                    "--label",
                    &format!("{}={}", owned.label, owned.token),
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges:true",
                    "--user",
                    &format!("{uid}:{gid}"),
                ]);
                if offline {
                    command.args(["--pull", "never", "--network", "none"]);
                } else {
                    command.args(["--network", "bridge"]);
                }
                spec.write_docker_args(&mut command);
                command
                    .arg("--mount")
                    .arg(bind_mount(
                        &root,
                        Path::new("/workspace"),
                        operation != LockOperation::Update,
                    )?)
                    .arg("--mount")
                    .arg(bind_mount(
                        &cache,
                        Path::new("/cache/cargo"),
                        cache_readonly,
                    )?);
                mount_sensitive_workspace_masks(&mut command, &root)?;
                if let Some(ca_cert) = &ca_cert {
                    command
                        .arg("--mount")
                        .arg(bind_mount(
                            ca_cert.path(),
                            Path::new("/devkit/ca.pem"),
                            true,
                        )?)
                        .args([
                            "--env",
                            "CARGO_HTTP_CAINFO=/devkit/ca.pem",
                            "--env",
                            "SSL_CERT_FILE=/devkit/ca.pem",
                        ]);
                }
                command.args([
                    "--workdir",
                    "/workspace",
                    "--env",
                    "HOME=/tmp/devkit-home",
                    "--env",
                    "CARGO_HOME=/cache/cargo",
                    "--env",
                    "RUSTUP_AUTO_INSTALL=0",
                    "--env",
                    &format!("RUSTUP_TOOLCHAIN={}", release.manifest.rust_version),
                ]);
                for value in cargo_safety_env() {
                    command.args(["--env", value]);
                }
                env_plan.write_docker_args(&mut command);
                command.args([&image, "cargo"]);
                match operation {
                    LockOperation::Update => {
                        command.args(["update", "--workspace"]);
                    }
                    LockOperation::Paths => {
                        command.args([
                            "metadata",
                            "--locked",
                            "--all-features",
                            "--format-version=1",
                        ]);
                    }
                    LockOperation::Validate => {
                        // Fetch checks the full workspace lock, including optional and
                        // target-specific dependencies, without compiling product code.
                        command.args(["fetch", "--locked", "--quiet"]);
                    }
                }
                if offline {
                    command.arg("--offline");
                }
                Ok(command)
            };
        let mut retried = false;
        loop {
            let token = uuid::Uuid::new_v4().to_string();
            let owned = OwnedContainer::new(format!("devkit-cargo-resolve-{token}"), token);
            let mut command = build_command(&budget, &owned)?;
            let output = runner.run_owned_unchecked(
                &mut command,
                OutputMode::Capture(16 * 1024 * 1024),
                &owned,
                None,
            )?;
            if output.code == 0 {
                return Ok(output);
            }
            if !retried
                && crate::resources::is_cgroup_delegation_failure(output.code, &output.stderr)
            {
                eprintln!(
                    "devkit: warning: Docker daemon lacks cgroup resource delegation; retrying without container cgroup limits"
                );
                budget.disable_cgroup_limits();
                retried = true;
                continue;
            }
            return output.success(label);
        }
    }
}
