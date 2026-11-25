//! Checkout admission and container-first daily operations. Make owns the DAG.
use crate::{
    Result, catalog::*, compose::*, files::*, model::*, native::CargoMember, processes::*,
    projections::*, state::*,
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const HELP: &str = "make help\nmake info [app=<name>] [json=1]\nmake setup|check|fix|test|build [app=<name> | package=<name>]\nmake dev|up [app=<name>] [headless=1]\nmake package [app=<desktop> | package=<name>]\nmake status|logs [app=<name>] [tail=100] [follow=1]\nmake down [cleanup=1]  (preserves data; cleanup releases leases)\nmake exec [app=<name> | package=<name> | component=apps/<name>|packages/<name> | toolchain=rust|node] [write=1] [network=none|bridge] [prepare=1] command=ARGV\nmake doctor\ndevkit info [TARGET] [--app <name>] [--json]\ndevkit status [TARGET] [--json]\ndevkit doctor [TARGET] [--json]\ndevkit describe [TARGET] [--json]\ndevkit exec [--component apps/web|packages/lib | --toolchain rust|node] [--write] [--network none|bridge] [--prepare] -- ARGV\n";
/// Compose 5.0 fixed config --no-env-resolution reading inactive service env files.
/// Desktop builds carry a packaging suffix; untested prereleases are not admitted.
pub fn validate_compose_version(value: &str) -> Result<()> {
    static COMPOSE_VERSION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-desktop\.(0|[1-9][0-9]*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$").unwrap()
    });
    let value = value.trim();
    let supported = COMPOSE_VERSION.captures(value).is_some_and(|parts| {
        let numbers: Option<Vec<u64>> = (1..=3).map(|i| parts[i].parse().ok()).collect();
        numbers.is_some_and(|v| (v[0], v[1], v[2]) >= (5, 0, 0))
    });
    ensure!(
        supported,
        "Docker Compose >=5.0.0 is required; found {value:?}. Install a supported stable Compose release (older versions resolve inactive service env files during admission)."
    );
    Ok(())
}
fn require_compose(runner: &ProcessRunner, docker: &mut Command) -> Result<()> {
    let output = runner.capture(docker.args(["compose", "version", "--short"]))?;
    validate_compose_version(output.text()?)
}
#[derive(Clone, Debug)]
pub struct RuntimeRequest {
    pub root: PathBuf,
    pub command: String,
    pub component: Option<String>,
    pub app: Option<String>,
    pub package: Option<String>,
    pub expected_manifest: Option<String>,
    pub expected_plan: Option<String>,
    pub headless: bool,
    pub cleanup: bool,
    pub package_aggregate: bool,
    pub dev_group: Vec<String>,
    pub follow: bool,
    pub tail: u64,
    pub cache_dir: Option<PathBuf>,
}
impl RuntimeRequest {
    pub fn new(root: PathBuf, command: String) -> Self {
        Self {
            root,
            command,
            component: None,
            app: None,
            package: None,
            expected_manifest: None,
            expected_plan: None,
            headless: false,
            cleanup: false,
            package_aggregate: false,
            dev_group: Vec::new(),
            follow: false,
            tail: 100,
            cache_dir: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub runtime: RuntimeRequest,
    pub toolchain: Option<Toolchain>,
    pub argv: Vec<OsString>,
    pub write: bool,
    pub network: String,
    pub prepare: bool,
}
mod pins;
pub use pins::{EnginePins, PnpmPin, ToolPins};
pub(crate) use pins::{Records, managed_bytes, render_headless_toolchains};

/// This admission deliberately does not read native dependency graphs/locks.
/// It permits service operations and explicit repair of stale native inputs.
pub struct ToolAdmission {
    pub root: PathBuf,
    pub state: ProjectState,
    pub pins: ToolPins,
    pub manifest_identity: FileIdentity,
}
impl ToolAdmission {
    pub fn capture(request: &RuntimeRequest) -> Result<Self> {
        Self::capture_kind(request, false, false)
    }
    pub fn capture_service(request: &RuntimeRequest) -> Result<Self> {
        Self::capture_kind(request, true, false)
    }
    pub fn capture_status_compat(request: &RuntimeRequest) -> Result<Self> {
        Self::capture_kind(request, true, true)
    }
    fn capture_kind(
        request: &RuntimeRequest,
        service: bool,
        allow_older_accepted: bool,
    ) -> Result<Self> {
        let root = project_root(&request.root)?;
        ensure!(root.is_dir(), "Checkout does not exist");
        runtime_pending(&root)?;
        let manifest_path = safe_path(&root, MANIFEST_PATH)?;
        check_manifest_regular_bounded(&manifest_path)?;
        let manifest_identity = identity(&manifest_path)?.context("Missing project manifest")?;
        if let Some(expected) = &request.expected_manifest {
            ensure!(
                SHA256_RE.is_match(expected) && expected == &manifest_identity.sha256,
                "accepted toolkit changed during this Make invocation; rerun the command"
            );
        }
        let state = read_project_state(&root)?;
        if let Some(token) = &request.expected_plan {
            verify_make_plan(&state, token)?;
        }
        // Runtime/integration is trusted even when product manifests need repair.
        verify_managed_set(&root, &state)?;
        for (path, id) in &state.managed {
            if path.starts_with(".devkit/runtime/")
                || path == CHECKSUM_PATH
                || path == ".devkit/compose.apps.yaml"
            {
                ensure!(
                    identity(&safe_path(&root, path)?)?
                        .as_ref()
                        .is_some_and(|actual| actual.matches_git_tracked(id)),
                    "Managed file conflict: {path}; restore with 'git checkout -- {path}' before running 'make sync' (or 'devkit sync')"
                );
            }
        }
        if !service {
            // Product locks may be stale, while accepted Rust pins and managed
            // tooling payloads still retain their exact ownership boundary.
            assert_managed(&root, &state)?;
        }
        let pins = ToolPins::admit(&root, &state)?;
        if allow_older_accepted {
            ensure!(
                SemVer::parse(&state.version)? <= SemVer::parse(env!("CARGO_PKG_VERSION"))?,
                "Native engine version {} is older than accepted project {}; run the accepted engine",
                env!("CARGO_PKG_VERSION"),
                state.version
            );
        } else {
            ensure!(
                state.version == env!("CARGO_PKG_VERSION"),
                "Native engine version {} does not match accepted project {}; run the accepted engine",
                env!("CARGO_PKG_VERSION"),
                state.version
            );
        }
        let admitted = Self {
            root,
            state,
            pins,
            manifest_identity,
        };
        admitted.target(request)?;
        admitted.recheck()?;
        Ok(admitted)
    }
    pub fn recheck(&self) -> Result<()> {
        ensure!(
            identity(&safe_path(&self.root, MANIFEST_PATH)?)?.as_ref()
                == Some(&self.manifest_identity),
            "Accepted project generation changed during operation"
        );
        runtime_pending(&self.root)
    }
    fn registered_summary(&self) -> String {
        let format_list = |role: Role| -> String {
            let items: Vec<&str> = self
                .state
                .components
                .iter()
                .filter(|(_, c)| c.role == role)
                .map(|(p, _)| p.rsplit('/').next().unwrap_or(p.as_str()))
                .collect();
            if items.is_empty() {
                "none".to_owned()
            } else {
                items.join(", ")
            }
        };
        format!(
            "(registered apps: {}; registered packages: {})",
            format_list(Role::App),
            format_list(Role::Library)
        )
    }
    pub fn target(&self, request: &RuntimeRequest) -> Result<Option<String>> {
        let summary = self.registered_summary();
        ensure!(
            request.app.is_none() || request.package.is_none(),
            "app and package selectors are mutually exclusive {summary}"
        );
        for (name, area) in [(&request.app, "apps"), (&request.package, "packages")] {
            if let Some(name) = name {
                slug(name)
                    .with_context(|| format!("Invalid {area} selector '{name}' {summary}"))?;
                ensure!(
                    self.state
                        .components
                        .contains_key(&format!("{area}/{name}")),
                    "Unregistered {area} selector: {name} {summary}"
                );
            }
        }
        let target = request
            .component
            .clone()
            .or_else(|| request.app.as_ref().map(|s| format!("apps/{s}")))
            .or_else(|| request.package.as_ref().map(|s| format!("packages/{s}")));
        if let Some(t) = &target {
            valid_component_path(t)?;
            ensure!(
                self.state.components.contains_key(t),
                "Unregistered target component: {t} {summary}"
            );
        }
        Ok(target)
    }
}
#[derive(Clone, Copy)]
enum TaskAccess {
    SetupNode,
    SetupElectron,
    SetupRust,
    Finite,
    PackageLibrary,
    Fix,
    ExecReadOnly,
    ExecWrite,
}
struct ContainerTask<'a> {
    component: Option<&'a str>,
    profile: ToolingProfile,
    electron_cache: bool,
    image: &'a str,
    argv: &'a [OsString],
    network: &'a str,
    forward_npm_secrets: bool,
    access: TaskAccess,
    inputs: Option<&'a InputIdentitySet>,
}
pub(crate) fn docker_command() -> Command {
    let mut command = Command::new("docker");
    command.env_remove("DOCKER_DEFAULT_PLATFORM");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("COMPOSE_") {
            command.env_remove(key);
        }
    }
    command
}

/// Confirm the pinned reference resolves to the native daemon image even when
/// the local image store also contains manifests for another architecture.
pub(crate) fn ensure_image(
    runner: &ProcessRunner,
    image: &str,
    platform: &str,
    prepare: bool,
) -> Result<()> {
    let matches = || -> Result<bool> {
        let result = runner.run(
            docker_command().args([
                "image",
                "inspect",
                "--format",
                "{{.Os}}/{{.Architecture}}",
                image,
            ]),
            OutputMode::Capture(1024 * 1024),
        )?;
        Ok(result.code == 0 && result.text()?.trim() == platform)
    };
    if !matches()? {
        ensure!(
            prepare,
            "accepted image {image} is absent for {platform}; run make setup (or exec --prepare)"
        );
        runner.checked(
            docker_command().args(["pull", "--platform", platform, image]),
            OutputMode::Inherit,
        )?;
        ensure!(matches()?, "Pulled image {image} does not match {platform}");
    }
    Ok(())
}

struct PreparedContext<'a> {
    admission: ToolAdmission,
    runner: &'a ProcessRunner,
    project: String,
    daemon: std::cell::OnceCell<crate::resources::DockerDaemonInfo>,
    helper: std::cell::OnceCell<PathBuf>,
    store: std::cell::OnceCell<crate::shared_cache::UnifiedStorePlan>,
    rust_image: Option<String>,
    desktop_image: Option<String>,
    node_image_verified: std::cell::Cell<bool>,
    node_ready_verified: std::cell::Cell<bool>,
    budget: std::cell::RefCell<Option<crate::resources::HostResourceBudget>>,
    cache_dir: Option<PathBuf>,
    pnpm_path: std::cell::RefCell<Option<PathBuf>>,
    cargo_cache: std::cell::RefCell<Option<PathBuf>>,
}
impl<'a> PreparedContext<'a> {
    fn new(
        admission: ToolAdmission,
        runner: &'a ProcessRunner,
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        Self::new_inner(admission, runner, true, cache_dir)
    }
    fn new_for_status(
        admission: ToolAdmission,
        runner: &'a ProcessRunner,
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        Self::new_inner(admission, runner, false, cache_dir)
    }
    fn new_inner(
        admission: ToolAdmission,
        runner: &'a ProcessRunner,
        verify_pin: bool,
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        if verify_pin {
            crate::delivery::verify_current_engine(&admission.pins.host_engine()?)?;
        }
        let project = crate::doctor::compose_project_name(&admission.root);
        Ok(Self {
            admission,
            runner,
            project,
            daemon: Default::default(),
            helper: Default::default(),
            store: Default::default(),
            rust_image: None,
            desktop_image: None,
            node_image_verified: std::cell::Cell::new(false),
            node_ready_verified: std::cell::Cell::new(false),
            budget: std::cell::RefCell::new(None),
            cache_dir: cache_dir.map(Path::to_owned),
            pnpm_path: std::cell::RefCell::new(None),
            cargo_cache: std::cell::RefCell::new(None),
        })
    }
    fn cache_root(&self) -> Result<PathBuf> {
        crate::cache::cache_root(self.cache_dir.as_deref())
    }
    fn cache_directory(&self, name: &str) -> Result<PathBuf> {
        let root = self.cache_root()?;
        crate::cache::ensure_cache_root(&root)?;
        for entry in [
            "pnpm-store",
            "pnpm-cache",
            "cargo",
            "electron",
            "tools",
            "empty-mask",
        ] {
            ensure_directory(&safe_path(&root, entry)?)?;
        }
        let path = safe_path(&root, name)?;
        crate::shared_cache::directory(&path)?;
        if name == "cargo" {
            *self.cargo_cache.borrow_mut() = Some(path.clone());
        }
        Ok(path)
    }
    fn daemon(&self) -> Result<&crate::resources::DockerDaemonInfo> {
        self.daemon
            .get()
            .context("Docker daemon has not been admitted")
    }
    fn ensure_linux_helper(&self, allow_network: bool) -> Result<&Path> {
        if self.helper.get().is_none() {
            let pin = self
                .admission
                .pins
                .engines
                .for_target(self.daemon()?.target)?;
            let cache = crate::cache::ReleaseCache::new(self.cache_dir.as_deref(), self.runner)?;
            let path = cache.ensure_linux_helper(
                &pin,
                &self.admission.state.release_identity,
                allow_network,
            )?;
            self.helper.set(path).expect("first helper resolution");
        }
        Ok(self.helper.get().expect("resolved helper"))
    }
    fn store_plan(&self) -> Result<&crate::shared_cache::UnifiedStorePlan> {
        if self.store.get().is_none() {
            let root = self.cache_root()?;
            let plan = crate::shared_cache::UnifiedStorePlan::resolve_with_probe(
                &self.admission.root,
                &root,
                |plan| {
                    if cfg!(target_os = "macos") {
                        crate::shared_cache::probe_guest_hardlinks(
                            plan,
                            &root.join("empty-mask"),
                            self.docker(),
                            self.ensure_linux_helper(false)?,
                            &self.admission.pins.node_image,
                            self.daemon()?.platform,
                            self.runner,
                        )
                    } else {
                        Ok(true)
                    }
                },
            )?;
            self.store.set(plan).expect("first store selection");
        }
        Ok(self.store.get().expect("resolved store"))
    }
    fn pnpm(&self, allow_network: bool) -> Result<PathBuf> {
        if let Some(path) = self.pnpm_path.borrow().as_ref() {
            return Ok(path.clone());
        }
        let pin = PnpmPin::from_checkout_for_target(&self.admission.root, self.daemon()?.target)?;
        let policy = crate::native::parse_manifest_npmrc_policy(
            &self.admission.root,
            crate::native::NodeLockKind::for_state(&self.admission.state)?,
        )?;
        let cache = crate::cache::ReleaseCache::new(self.cache_dir.as_deref(), self.runner)?;
        let path = cache.ensure_pnpm(&pin, Some(&policy), allow_network)?;
        *self.pnpm_path.borrow_mut() = Some(path.clone());
        Ok(path)
    }
    fn setup_electron(&self, path: &str, inputs: &InputIdentitySet) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let root = &self.admission.root;
        let package = fs::canonicalize(root.join(path).join("node_modules/electron"))?;
        ensure!(
            package.starts_with(fs::canonicalize(root.join("node_modules/.pnpm"))?),
            "Electron package resolves outside the isolated workspace store"
        );
        let manifest: Value = serde_json::from_slice(
            &read_electron_metadata(&package.join("package.json"), 1024 * 1024)?
                .context("Electron package manifest is missing")?,
        )?;
        let version = text(&manifest["version"], "Electron version")?;
        crate::files::version(version)?;
        let plan = self.store_plan()?;
        let daemon_target = self.daemon()?.target;
        let (arch, machine) = daemon_target
            .electron_arch()
            .zip(daemon_target.elf_machine())
            .context("Electron requires a Linux daemon")?;
        let locks = plan.store.join(".locks");
        crate::shared_cache::directory(&locks)?;
        let _lock = crate::cache::lock_file(
            &locks.join(format!("electron-dist-{version}-linux-{arch}.lock")),
            self.runner,
        )?;
        let distributions = plan.store.join("electron-dist");
        crate::shared_cache::directory(&distributions)?;
        let cached = safe_path(&distributions, &format!("{version}-linux-{arch}"))?;
        ensure_directory(&cached)?;
        let marker = safe_path(&cached, ".complete")?;
        if !exists(&marker)? {
            ensure!(
                !exists(&cached)? || fs::read_dir(&cached)?.next().is_none(),
                "Incomplete Electron cache: {}; remove it and rerun make setup",
                cached.display()
            );
            // A previous local install may be partial even when its executable
            // exists. Force a complete verified extraction before first publish.
            let dist = safe_path(&package, "dist")?;
            if exists(&dist)? {
                ensure_directory(&dist)?;
                fs::remove_dir_all(&dist)?;
            }
            let installed = safe_path(&package, "path.txt")?;
            if exists(&installed)? {
                ensure!(
                    fs::symlink_metadata(&installed)?.is_file(),
                    "Electron metadata must be a regular file: {}",
                    installed.display()
                );
            }
            remove_file(&installed)?;
            self.task(ContainerTask {
                component: Some(path),
                profile: ToolingProfile::Node,
                electron_cache: true,
                image: &self.admission.pins.node_image,
                argv: &node_argv(
                    self,
                    vec!["pnpm".into(), "exec".into(), "install-electron".into()],
                ),
                network: "bridge",
                forward_npm_secrets: true,
                access: TaskAccess::SetupElectron,
                inputs: Some(inputs),
            })?;
            ensure!(
                package.join("dist/electron").is_file(),
                "Electron installation omitted its executable"
            );
            let stage = tempfile::Builder::new()
                .prefix(".stage-")
                .tempdir_in(&distributions)?;
            let inventory = distribution_inventory(&package.join("dist"), self.runner)?;
            ensure_electron_executable(&package.join("dist/electron"), &inventory, machine)?;
            hardlink_distribution(
                &package.join("dist"),
                &stage.path().join("dist"),
                self.runner,
            )?;
            ensure!(
                distribution_inventory(&stage.path().join("dist"), self.runner)? == inventory,
                "Electron distribution changed while linking"
            );
            write_json(
                &stage.path().join(".complete"),
                &ElectronDistributionReceipt {
                    schema_version: 1,
                    version: version.into(),
                    entries: inventory,
                },
            )?;
            crate::archive::publish_new_directory(stage.path(), &cached)?;
        }
        let receipt = read_distribution_receipt(&marker)?;
        ensure!(
            receipt.schema_version == 1 && receipt.version == version,
            "Invalid cached Electron distribution marker"
        );
        ensure_electron_executable(&cached.join("dist/electron"), &receipt.entries, machine)?;
        ensure!(
            distribution_inventory(&safe_path(&cached, "dist")?, self.runner)? == receipt.entries,
            "Cached Electron distribution is incomplete or corrupt: {}; remove it and rerun make setup",
            cached.display()
        );
        let source = safe_path(&cached, "dist/electron")?;
        let cached_metadata = fs::metadata(&source)?;
        let local = package.join("dist/electron");
        let linked = fs::metadata(&local)
            .is_ok_and(|m| m.dev() == cached_metadata.dev() && m.ino() == cached_metadata.ino());
        let copied_across_mounts = local.is_file()
            && crate::shared_cache::mount_identity(&local)?
                != crate::shared_cache::mount_identity(&source)?;
        let installed = read_electron_metadata(&package.join("path.txt"), 256)?;
        let ready = (linked || copied_across_mounts)
            && installed.as_deref().is_some_and(|bytes| {
                std::str::from_utf8(bytes).is_ok_and(|p| p.trim() == "electron")
            })
            && distribution_inventory(&package.join("dist"), self.runner)
                .is_ok_and(|inventory| inventory == receipt.entries);
        if ready {
            return Ok(());
        }
        let dist = package.join("dist");
        if let Ok(metadata) = fs::symlink_metadata(&dist) {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "Invalid Electron dist directory"
            );
            fs::remove_dir_all(&dist)?;
        }
        hardlink_distribution(&cached.join("dist"), &dist, self.runner)?;
        ensure!(
            distribution_inventory(&dist, self.runner)? == receipt.entries,
            "Electron distribution changed while linking"
        );
        write_atomic(&package.join("path.txt"), b"electron", 0o644)
    }
    fn docker(&self) -> Command {
        docker_command()
    }
    fn env(&self, c: &mut Command) {
        let a = &self.admission;
        for (k, v) in [
            ("DEVKIT_PROJECT_ROOT", a.root.as_os_str().to_owned()),
            ("DEVKIT_PROJECT_NAME", self.project.clone().into()),
            ("DEVKIT_UID", unsafe { libc::geteuid() }.to_string().into()),
            ("DEVKIT_GID", unsafe { libc::getegid() }.to_string().into()),
            ("DEVKIT_NODE_IMAGE", a.pins.node_image.clone().into()),
            ("DEVKIT_NODE_VERSION", a.pins.node_version.clone().into()),
            ("DEVKIT_RUST_VERSION", a.pins.rust_version.clone().into()),
            (
                "DEVKIT_RUST_IMAGE",
                self.rust_image
                    .clone()
                    .unwrap_or_else(|| "devkit-unprepared-rust".into())
                    .into(),
            ),
            (
                "DEVKIT_DESKTOP_IMAGE",
                self.desktop_image
                    .clone()
                    .unwrap_or_else(|| "devkit-unprepared-desktop".into())
                    .into(),
            ),
            // The accepted Compose interface names the binary mounted in Linux.
            (
                "DEVKIT_ENGINE_PATH",
                self.helper
                    .get()
                    .map(|path| path.as_os_str().to_owned())
                    .unwrap_or_else(|| "/devkit/unused-engine".into()),
            ),
            (
                "DEVKIT_PNPM_PATH",
                self.pnpm_path
                    .borrow()
                    .as_ref()
                    .map(|p| p.as_os_str().to_owned())
                    .unwrap_or_else(|| "/devkit/unused-pnpm".into()),
            ),
            (
                "DEVKIT_CARGO_CACHE_DIR",
                self.cargo_cache
                    .borrow()
                    .as_ref()
                    .map(|p| p.as_os_str().to_owned())
                    .unwrap_or_else(|| "/devkit/unused-cargo".into()),
            ),
            (
                "DEVKIT_ELECTRON_CACHE_DIR",
                "/devkit/unused-electron".into(),
            ),
        ] {
            c.env(k, v);
        }
    }
    fn compose_project(&self) -> Command {
        let mut c = self.docker();
        c.args(["compose", "--project-directory"])
            .arg(&self.admission.root)
            .args(["--project-name", &self.project, "--env-file", "/dev/null"]);
        self.env(&mut c);
        c
    }
    fn compose(&self) -> Result<Command> {
        let mut c = self.compose_project();
        c.arg("--file").arg(safe_path(
            &self.admission.root,
            ".devkit/compose.apps.yaml",
        )?);
        if identity(&safe_path(&self.admission.root, "compose.yaml")?)?.is_some() {
            c.arg("--file")
                .arg(self.admission.root.join("compose.yaml"));
        }
        Ok(c)
    }
    fn prerequisite_service(&self) -> Result<()> {
        require_compose(self.runner, &mut self.docker())?;
        let info = crate::resources::query_docker_info(self.runner, &mut self.docker())?;
        self.daemon.set(info).expect("first daemon admission");
        Ok(())
    }
    fn prerequisite(&self) -> Result<()> {
        require_compose(self.runner, &mut self.docker())?;
        let budget = crate::resources::HostResourceBudget::probe_with_docker(
            self.runner,
            &mut self.docker(),
        )?;
        let info = budget
            .daemon_info
            .clone()
            .context("Missing Docker daemon information")?;
        self.daemon.set(info).expect("first daemon admission");
        *self.budget.borrow_mut() = Some(budget);
        Ok(())
    }
    fn node_image(&self, prepare: bool) -> Result<()> {
        if !self.node_image_verified.get() {
            ensure_image(
                self.runner,
                &self.admission.pins.node_image,
                self.daemon()?.platform,
                prepare,
            )?;
            self.node_image_verified.set(true);
        }
        Ok(())
    }
    fn derived_image(&mut self, kind: &str, prepare: bool) -> Result<String> {
        if !prepare {
            let cached = if kind == "rust" {
                self.rust_image.as_ref()
            } else {
                self.desktop_image.as_ref()
            };
            if let Some(existing) = cached {
                return Ok(existing.clone());
            }
        }
        let (recipe, pin, label) = if kind == "rust" {
            (
                "rust.Dockerfile",
                self.admission.pins.rust_recipe.as_str(),
                "io.docouno.devkit.rust-recipe",
            )
        } else {
            (
                "desktop.Dockerfile",
                self.admission
                    .pins
                    .desktop_recipe
                    .as_deref()
                    .context("Missing desktop recipe pin for Electron")?,
                "io.docouno.devkit.desktop-recipe",
            )
        };
        let recipe_path = safe_path(&self.admission.root, &format!(".devkit/runtime/{recipe}"))?;
        let (recipe_bytes, id) = read_regular_file(&recipe_path, 1024 * 1024, "image recipe")?
            .context("Missing accepted image recipe")?;
        ensure!(
            id.sha256 == pin,
            "{recipe} differs from the accepted recipe"
        );
        let platform = self.daemon()?.platform;
        let arch = platform
            .strip_prefix("linux/")
            .context("Expected Linux image platform")?;
        let derived_tag = format!("devkit-derived-{kind}:{arch}-{pin}");
        let legacy_state = safe_path(&self.admission.root, &format!(".devkit/state/{kind}-image"))?;
        let state = safe_path(
            &self.admission.root,
            &format!(".devkit/state/{kind}-image.json"),
        )?;
        let inspect = |reference: &str| -> Result<Option<String>> {
            let format = format!(
                "{{{{.Id}}}}\n{{{{.Os}}}}/{{{{.Architecture}}}}\n{{{{index .Config.Labels \"{label}\"}}}}\n{{{{index .Config.Labels \"io.docouno.devkit.platform\"}}}}"
            );
            let output = self.runner.run(
                self.docker()
                    .args(["image", "inspect", "--format", &format, reference]),
                OutputMode::Capture(1024 * 1024),
            )?;
            if output.code != 0 {
                return Ok(None);
            }
            let mut lines = output.text()?.lines();
            let id = lines.next().unwrap_or_default();
            Ok((id
                .strip_prefix("sha256:")
                .is_some_and(|h| SHA256_RE.is_match(h))
                && lines.next() == Some(platform)
                && lines.next() == Some(pin)
                && lines.next() == Some(platform)
                && lines.next().is_none())
            .then(|| id.to_owned()))
        };
        let record = |id: &str| -> Result<()> {
            self.admission.recheck()?;
            let receipt = crate::doctor::DerivedImageReceipt::new(platform, id, pin);
            write_json(&state, &receipt)?;
            remove_file(&legacy_state)
        };
        if let Some(receipt) = crate::doctor::read_image_receipt(&self.admission.root, kind, pin)?
            && receipt.platform == platform
            && inspect(&receipt.image_id)?.as_ref() == Some(&receipt.image_id)
        {
            if prepare {
                self.runner.checked(
                    self.docker()
                        .args(["image", "tag", &receipt.image_id, &derived_tag]),
                    OutputMode::Capture(1024 * 1024),
                )?;
            }
            self.set_image(kind, receipt.image_id.clone());
            return Ok(receipt.image_id);
        }
        ensure!(
            prepare,
            "accepted {kind} image is absent/stale; run make setup (or exec --prepare)"
        );
        if let Some(image) = inspect(&derived_tag)? {
            record(&image)?;
            self.set_image(kind, image.clone());
            return Ok(image);
        }
        let base = std::str::from_utf8(&recipe_bytes)?
            .lines()
            .find_map(|line| line.strip_prefix("FROM "))
            .context("Missing pinned base image in accepted recipe")?;
        ensure!(
            base.rsplit_once("@sha256:")
                .is_some_and(|(_, hash)| SHA256_RE.is_match(hash)),
            "Expected a digest-pinned base image in {recipe}"
        );
        ensure_image(self.runner, base, platform, true)?;
        let tmp = tempfile::Builder::new()
            .prefix("image-")
            .tempfile_in(state.parent().context("Image state parent")?)?;
        self.runner
            .run_with_input(
                self.docker()
                    .args([
                        "build",
                        "--platform",
                        platform,
                        "--pull=false",
                        "--label",
                        &format!("io.docouno.devkit.platform={platform}"),
                        "--build-arg",
                        &format!("DEVKIT_RECIPE_SHA={pin}"),
                        "--label",
                        &format!("{label}={pin}"),
                        "--tag",
                        &derived_tag,
                        "--iidfile",
                    ])
                    .arg(tmp.path())
                    .args(["--file", "-"])
                    .arg(self.admission.root.join(".devkit/runtime")),
                OutputMode::Inherit,
                &recipe_bytes,
            )?
            .success("Build accepted tool image")?;
        let image = fs::read_to_string(tmp.path())?.trim().to_owned();
        ensure!(
            image
                .strip_prefix("sha256:")
                .is_some_and(|h| SHA256_RE.is_match(h)),
            "Invalid {kind} image identity"
        );
        ensure!(
            inspect(&image)?.as_ref() == Some(&image),
            "Built {kind} image does not match {platform} and its accepted recipe"
        );
        record(&image)?;
        self.set_image(kind, image.clone());
        Ok(image)
    }
    fn load_recorded_images(&mut self, allow_legacy: bool) -> Result<()> {
        // Existing recorded derived image IDs allow interpolation; never inspect or
        // acquire tool images for service status/logs/down.
        for kind in ["rust", "desktop"] {
            if kind == "desktop" && !has_electron(&self.admission.state) {
                continue;
            }
            let json_path = safe_path(
                &self.admission.root,
                &format!(".devkit/state/{kind}-image.json"),
            )?;
            if let Some((bytes, _)) = read_regular_file(&json_path, 4096, "image receipt")?
                && let Ok(receipt) =
                    serde_json::from_slice::<crate::doctor::DerivedImageReceipt>(&bytes)
                && receipt.schema_version == 1
                && receipt
                    .image_id
                    .strip_prefix("sha256:")
                    .is_some_and(|h| SHA256_RE.is_match(h))
            {
                self.set_image(kind, receipt.image_id);
                continue;
            }
            if allow_legacy {
                let legacy =
                    safe_path(&self.admission.root, &format!(".devkit/state/{kind}-image"))?;
                if let Some((bytes, _)) = read_regular_file(&legacy, 4096, "legacy image receipt")?
                    && let Ok(text) = std::str::from_utf8(&bytes)
                {
                    let image = text.trim();
                    if image
                        .strip_prefix("sha256:")
                        .is_some_and(|h| SHA256_RE.is_match(h))
                    {
                        self.set_image(kind, image.to_owned());
                    }
                }
            }
        }
        Ok(())
    }
    fn set_image(&mut self, kind: &str, image: String) {
        if kind == "rust" {
            self.rust_image = Some(image);
        } else {
            self.desktop_image = Some(image);
        }
    }
    fn dirs(&self, paths: impl IntoIterator<Item = String>) -> Result<()> {
        for path in paths {
            let p = input_path(&self.admission.root, &path)?;
            ensure_directory(&p)?;
            fs::create_dir_all(p)?;
        }
        Ok(())
    }
    fn task(&self, task: ContainerTask<'_>) -> Result<()> {
        self.run_task(task, OutputMode::TeeStderr(64 * 1024))
            .map(|_| ())
    }
    fn run_task(&self, task: ContainerTask<'_>, mode: OutputMode) -> Result<ProcessOutput> {
        let ContainerTask {
            component,
            profile,
            electron_cache,
            image,
            argv,
            network,
            forward_npm_secrets,
            access,
            inputs,
        } = task;
        let write_root = matches!(access, TaskAccess::ExecWrite);
        let setup = matches!(access, TaskAccess::SetupNode);
        let setup_electron = matches!(access, TaskAccess::SetupElectron);
        let node = profile_toolchain(profile) == Toolchain::Node;
        let fix = matches!(access, TaskAccess::Fix);
        let package_release_dir = if matches!(access, TaskAccess::PackageLibrary) {
            Some(format!(
                "{}/release",
                component.context("PackageLibrary requires a component")?
            ))
        } else {
            None
        };
        let root = &self.admission.root;
        let mut writes = BTreeSet::new();
        if node {
            if let Some(inputs) = inputs {
                writes.extend(workspace_write_directories(&self.admission.state, inputs)?);
            }
            for (path, c) in node_components(&self.admission.state) {
                writes.insert(format!("{path}/node_modules"));
                for output in profile_outputs(c.tooling_profile) {
                    writes.insert(format!("{path}/{output}"));
                }
            }
            writes.insert("node_modules".into());
        } else if !matches!(access, TaskAccess::ExecReadOnly | TaskAccess::SetupRust) {
            writes.insert("target".into());
        }
        let policy = if node {
            let directories = self
                .admission
                .state
                .components
                .iter()
                .filter(|(_, c)| c.toolchain == Toolchain::Node)
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            Some(crate::native::parse_and_admit_npmrc(
                root,
                &directories,
                crate::native::NodeLockKind::for_state(&self.admission.state)?,
            )?)
        } else {
            None
        };
        let online = network != "none" && forward_npm_secrets;
        if let Some(policy) = policy.as_ref().filter(|_| online) {
            crate::native::validate_vm_registries(policy, None)?;
        }
        self.dirs(writes.iter().cloned())?;
        let pnpm = if node { Some(self.pnpm(setup)?) } else { None };
        let cargo_cache = if node {
            None
        } else {
            Some(self.cache_directory("cargo")?)
        };
        let pnpm_cache = if node {
            Some(self.cache_directory("pnpm-cache")?)
        } else {
            None
        };
        let electron_cache_dir = if electron_cache {
            Some(self.cache_directory("electron")?)
        } else {
            None
        };
        let root_cache = self.cache_root()?;
        let unified = if node && (setup || write_root) {
            Some(self.store_plan()?)
        } else {
            if node {
                crate::shared_cache::prepare_mount_masks(&root_cache)?;
            }
            None
        };
        let userconfig = if let Some(policy) = &policy {
            use std::os::unix::fs::PermissionsExt;
            let mut file = tempfile::Builder::new()
                .prefix("devkit-npmrc-")
                .tempfile_in(root_cache.join(".tmp"))?;
            use std::io::Write;
            for line in &policy.sanitized_lines {
                if online || !line.contains("${") {
                    writeln!(file, "{line}")?;
                }
            }
            file.flush()?;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
            Some(file)
        } else {
            None
        };
        let ca_cert = if network != "none" && (!node || forward_npm_secrets) {
            crate::native::resolve_ca_cert_path()?
                .map(|p| crate::download::additional_ca_file_in(&p, &root_cache.join(".tmp")))
                .transpose()?
        } else {
            None
        };
        if let Some(cache) = cargo_cache.as_deref() {
            assert_clean_cargo_home(cache)?;
        }
        let cargo_cache_readonly = !matches!(access, TaskAccess::SetupRust | TaskAccess::ExecWrite);
        let mut budget = self
            .budget
            .borrow()
            .clone()
            .context("Missing host resource budget; prerequisite not checked")?;
        let build_command = |budget: &crate::resources::HostResourceBudget,
                             owned: &OwnedContainer|
         -> Result<Command> {
            let spec = budget.for_task(profile);
            let env_plan = budget.toolchain_env_for_task(profile, &spec)?;
            let mut c = self.docker();
            c.args([
                "run",
                "--platform",
                self.daemon()?.platform,
                "--pull",
                "never",
                "--rm",
                "--init",
                "--name",
                &owned.name,
                "--label",
                &format!("{}={}", owned.label, owned.token),
                "--label",
                &format!("io.docouno.devkit.checkout={}", self.project),
                "--label",
                "io.docouno.devkit.role=tool",
                "--user",
                &format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                    libc::getegid()
                }),
                "--network",
                network,
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges:true",
            ]);
            spec.write_docker_args(&mut c);
            if !setup {
                crate::shared_cache::mount(&mut c, root, Path::new("/workspace"), !write_root)?;
            }
            if let Some(plan) = &unified {
                let mut writable = Vec::new();
                for path in &writes {
                    if path.ends_with("node_modules") {
                        writable.push(input_path(root, path)?);
                    }
                }
                plan.mount(&mut c, &root_cache.join("empty-mask"), setup, &writable)?;
                if write_root {
                    plan.mask_checkout_files(&mut c, Path::new("/workspace"))?;
                }
                c.args([
                    "--env",
                    &format!("PNPM_CONFIG_STORE_DIR={}", plan.store_container.display()),
                ]);
            }
            if fix {
                let target = component.context("Fix requires a component")?;
                c.args([
                    "--mount",
                    &bind_mount(
                        &input_path(root, target)?,
                        &Path::new("/workspace").join(target),
                        false,
                    )?,
                ]);
            }
            for path in writes.iter().filter(|_| !setup && !write_root) {
                let readonly = match access {
                    TaskAccess::PackageLibrary => {
                        package_release_dir.as_deref() != Some(path.as_str())
                    }
                    TaskAccess::ExecReadOnly => path.ends_with("node_modules"),
                    TaskAccess::SetupElectron => !path.ends_with("node_modules"),
                    _ => false,
                };
                c.args([
                    "--mount",
                    &bind_mount(
                        &input_path(root, path)?,
                        &Path::new("/workspace").join(path),
                        readonly,
                    )?,
                ]);
            }
            if node && matches!(access, TaskAccess::Finite | TaskAccess::Fix) {
                let virtual_store = input_path(root, "node_modules/.pnpm")?;
                if virtual_store.is_dir() {
                    crate::shared_cache::mount(
                        &mut c,
                        &virtual_store,
                        Path::new("/workspace/node_modules/.pnpm"),
                        true,
                    )?;
                }
            }
            for (source, target, readonly) in [
                (cargo_cache.as_deref(), "/cache/cargo", cargo_cache_readonly),
                (pnpm_cache.as_deref(), "/cache/pnpm", false),
                (electron_cache_dir.as_deref(), "/cache/electron", false),
            ] {
                if let Some(source) = source {
                    crate::shared_cache::mount(&mut c, source, Path::new(target), readonly)?;
                }
            }
            if !node && matches!(access, TaskAccess::SetupRust) {
                mount_sensitive_workspace_masks(&mut c, root)?;
            }
            if let Some(pnpm) = &pnpm {
                crate::shared_cache::mount(&mut c, pnpm, Path::new("/devkit/bin/pnpm"), true)?;
            }
            if let Some(file) = &userconfig {
                crate::shared_cache::mount(
                    &mut c,
                    file.path(),
                    Path::new("/devkit/user.npmrc"),
                    true,
                )?;
            }
            if node && !setup && !write_root {
                if setup_electron {
                    crate::shared_cache::mask_checkout_secrets(
                        &mut c,
                        root,
                        &root_cache,
                        Path::new("/workspace"),
                    )?;
                } else {
                    for relative in crate::shared_cache::npmrc_targets(root)? {
                        crate::shared_cache::mount(
                            &mut c,
                            &root_cache.join("empty-file-mask"),
                            &Path::new("/workspace").join(relative),
                            true,
                        )?;
                    }
                }
            }
            let work_root = unified
                .as_ref()
                .map(|p| p.checkout_container.clone())
                .unwrap_or_else(|| PathBuf::from("/workspace"));
            let workdir = match component {
                Some(path)
                    if node
                        || matches!(access, TaskAccess::ExecReadOnly | TaskAccess::ExecWrite) =>
                {
                    work_root.join(path)
                }
                _ => work_root,
            };
            c.args([
                "--mount",
                &bind_mount(
                    self.ensure_linux_helper(false)?,
                    Path::new("/devkit/devkit"),
                    true,
                )?,
                "--workdir",
            ])
            .arg(workdir);
            for value in [
                "HOME=/tmp/devkit-home",
                "CI=true",
                "PATH=/devkit/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "PNPM_CONFIG_NPMRC_AUTH_FILE=/devkit/user.npmrc",
                "PNPM_CONFIG_IGNORE_PNPMFILE=true",
                "PNPM_CONFIG_PM_ON_FAIL=ignore",
                "PNPM_CONFIG_RUNTIME=false",
                "PNPM_CONFIG_RUNTIME_ON_FAIL=ignore",
                "XDG_CACHE_HOME=/cache/pnpm",
                "NPM_CONFIG_CACHE=/tmp/devkit-npm-cache",
                "NPM_CONFIG_USERCONFIG=/devkit/user.npmrc",
                "NPM_CONFIG_GLOBALCONFIG=/tmp/devkit-global.npmrc",
                "NPM_CONFIG_UPDATE_NOTIFIER=false",
                "NPM_CONFIG_FUND=false",
                "NPM_CONFIG_AUDIT=false",
                "CARGO_HOME=/cache/cargo",
                "RUSTUP_AUTO_INSTALL=0",
            ] {
                c.args(["--env", value]);
            }
            if electron_cache {
                c.args(["--env", "ELECTRON_CACHE=/cache/electron"]);
            }
            env_plan.write_docker_args(&mut c);
            if !node {
                c.args([
                    "--env",
                    &format!("DEVKIT_RUST_VERSION={}", self.admission.pins.rust_version),
                    "--env",
                    if matches!(access, TaskAccess::ExecReadOnly) {
                        "CARGO_TARGET_DIR=/tmp/cargo-target"
                    } else {
                        "CARGO_TARGET_DIR=/workspace/target"
                    },
                ]);
                if matches!(access, TaskAccess::ExecReadOnly) {
                    c.args(["--env", "CARGO_INCREMENTAL=0"]);
                }
                if matches!(
                    access,
                    TaskAccess::SetupRust | TaskAccess::Finite | TaskAccess::Fix
                ) {
                    for value in cargo_safety_env() {
                        c.args(["--env", value]);
                    }
                }
                if network != "none"
                    && let Some(ca_cert) = &ca_cert
                {
                    c.args([
                        "--mount",
                        &bind_mount(ca_cert.path(), Path::new("/devkit/ca.pem"), true)?,
                        "--env",
                        "CARGO_HTTP_CAINFO=/devkit/ca.pem",
                        "--env",
                        "SSL_CERT_FILE=/devkit/ca.pem",
                    ]);
                }
            }
            if node {
                let npmrc_policy = policy.as_ref().context("Missing Node npmrc policy")?;
                if network != "none" && forward_npm_secrets {
                    crate::native::verify_required_npm_env_vars(npmrc_policy)?;
                    for var in &npmrc_policy.required_env_vars {
                        c.args(["--env", var]);
                    }
                    if let Some(ca_cert) = &ca_cert {
                        c.args([
                            "--mount",
                            &bind_mount(ca_cert.path(), Path::new("/devkit/ca.pem"), true)?,
                            "--env",
                            "NODE_EXTRA_CA_CERTS=/devkit/ca.pem",
                            "--env",
                            "SSL_CERT_FILE=/devkit/ca.pem",
                        ]);
                    }
                } else {
                    for var in &npmrc_policy.required_env_vars {
                        c.args(["--env", &format!("{var}=devkit-offline-unused")]);
                    }
                }
            }
            if component.is_some_and(|p| {
                self.admission.state.components[p]
                    .development
                    .as_ref()
                    .is_some_and(|d| d.api.is_some())
            }) && profile == ToolingProfile::Web
            {
                c.args(["--env", "VITE_API_ENABLED=true"]);
            }
            if profile == ToolingProfile::Electron {
                let seccomp = verify_managed_seccomp(root, &self.admission.state)?;
                c.args(["--security-opt", &format!("seccomp={}", seccomp.display())]);
            }
            c.arg(image).args(argv);
            if setup {
                let plan = unified.as_ref().context("Missing unified setup store")?;
                c.arg("--store-dir").arg(&plan.store_container).args([
                    "--config.package-import-method=hardlink",
                    "--npmrc-auth-file=/devkit/user.npmrc",
                ]);
            }
            Ok(c)
        };
        let mut retried = false;
        loop {
            let token = uuid::Uuid::new_v4().to_string();
            let owned = OwnedContainer::new(format!("{}-task-{token}", self.project), token);
            let mut c = build_command(&budget, &owned)?;
            let mut output = self
                .runner
                .run_owned_unchecked(&mut c, mode, &owned, None)?;
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
                *self.budget.borrow_mut() = Some(budget.clone());
                retried = true;
                continue;
            }
            if matches!(mode, OutputMode::TeeStderr(_)) {
                // Streamed diagnostics are already visible to the operator.
                output.stderr.clear();
            }
            return output.success("Container task");
        }
    }
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum DistributionEntry {
    Directory {
        mode: u32,
    },
    File {
        sha256: String,
        size: u64,
        mode: u32,
    },
    Symlink {
        target: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ElectronDistributionReceipt {
    schema_version: u32,
    version: String,
    entries: BTreeMap<String, DistributionEntry>,
}

fn ensure_electron_executable(
    path: &Path,
    entries: &BTreeMap<String, DistributionEntry>,
    machine: u16,
) -> Result<()> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    ensure!(
        matches!(entries.get("electron"), Some(DistributionEntry::File {size, mode, ..}) if *size > 0 && mode & 0o111 != 0),
        "Electron distribution omitted its executable"
    );
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "Expected regular Electron executable"
    );
    let mut header = [0u8; 20];
    (&file).read_exact(&mut header)?;
    ensure!(
        &header[..7] == b"\x7fELF\x02\x01\x01"
            && u16::from_le_bytes([header[18], header[19]]) == machine,
        "Electron executable does not match the Docker daemon architecture"
    );
    Ok(())
}

fn read_electron_metadata(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>> {
    Ok(read_regular_file(path, maximum, "Electron metadata")?.map(|(bytes, _)| bytes))
}

fn read_distribution_receipt(path: &Path) -> Result<ElectronDistributionReceipt> {
    let bytes = read_electron_metadata(path, 4 * 1024 * 1024)?
        .context("Electron distribution marker is missing")?;
    let receipt: ElectronDistributionReceipt = serde_json::from_slice(&bytes)
        .context("Invalid Electron distribution marker; remove the cache and rerun make setup")?;
    ensure!(
        receipt.entries.len() <= 16_384,
        "Electron distribution exceeds entry budget"
    );
    Ok(receipt)
}

fn distribution_inventory(
    root: &Path,
    runner: &ProcessRunner,
) -> Result<BTreeMap<String, DistributionEntry>> {
    use sha2::{Digest, Sha256};
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    };
    const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    let root = project_root(root)?;
    ensure!(
        fs::symlink_metadata(&root)?.is_dir(),
        "Expected Electron distribution directory"
    );
    let mut entries = BTreeMap::new();
    let mut pending = vec![(root.clone(), 0_u32)];
    let mut total = 0_u64;
    while let Some((path, depth)) = pending.pop() {
        runner.check_cancelled()?;
        ensure!(
            depth <= 32 && entries.len() < 16_384,
            "Electron distribution exceeds tree budget"
        );
        let metadata = fs::symlink_metadata(&path)?;
        let mode = metadata.permissions().mode() & 0o7777;
        let entry = if metadata.is_dir() {
            for child in fs::read_dir(&path)? {
                let child = child?;
                pending.push((child.path(), depth + 1));
                ensure!(
                    pending.len() + entries.len() <= 16_384,
                    "Electron distribution exceeds entry budget"
                );
            }
            DistributionEntry::Directory { mode }
        } else if metadata.is_file() {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let before = file.metadata()?;
            ensure!(
                before.is_file()
                    && before.dev() == metadata.dev()
                    && before.ino() == metadata.ino(),
                "Electron distribution changed while reading"
            );
            let mut hash = Sha256::new();
            let mut bytes = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                runner.check_cancelled()?;
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                total = total
                    .checked_add(count as u64)
                    .context("Electron distribution size overflow")?;
                ensure!(
                    total <= MAX_BYTES,
                    "Electron distribution exceeds byte budget"
                );
                bytes += count as u64;
                hash.update(&buffer[..count]);
            }
            let after = file.metadata()?;
            let current = fs::symlink_metadata(&path)?;
            ensure!(
                before.len() == bytes
                    && after.len() == bytes
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec()
                    && before.permissions().mode() == after.permissions().mode()
                    && metadata.permissions().mode() == before.permissions().mode()
                    && current.permissions().mode() == before.permissions().mode()
                    && current.dev() == before.dev()
                    && current.ino() == before.ino(),
                "Electron distribution changed while reading"
            );
            DistributionEntry::File {
                sha256: format!("{:x}", hash.finalize()),
                size: bytes,
                mode,
            }
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            ensure!(
                !target.is_absolute()
                    && lexical(
                        &path
                            .parent()
                            .context("Electron symlink parent")?
                            .join(&target)
                    )?
                    .starts_with(&root),
                "Electron distribution symlink escapes its root"
            );
            DistributionEntry::Symlink { target }
        } else {
            anyhow::bail!("Unsupported Electron distribution entry");
        };
        let name = if path == root {
            ".".into()
        } else {
            relative_name(&root, &path)?
        };
        entries.insert(name, entry);
    }
    Ok(entries)
}

fn hardlink_distribution(source: &Path, target: &Path, runner: &ProcessRunner) -> Result<()> {
    let source = project_root(source)?;
    let target = project_root(target)?;
    fn copy(
        root: &Path,
        source: &Path,
        target: &Path,
        runner: &ProcessRunner,
        depth: u32,
        count: &mut usize,
    ) -> Result<()> {
        runner.check_cancelled()?;
        *count += 1;
        ensure!(
            depth <= 32 && *count <= 16_384,
            "Electron distribution exceeds tree budget"
        );
        let metadata = fs::symlink_metadata(source)?;
        if metadata.is_dir() {
            fs::create_dir(target)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy(
                    root,
                    &entry.path(),
                    &target.join(entry.file_name()),
                    runner,
                    depth + 1,
                    count,
                )?;
            }
            fs::set_permissions(target, metadata.permissions())?;
        } else if metadata.is_file() {
            match fs::hard_link(source, target) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
                    fs::copy(source, target)?;
                }
                Err(error) => return Err(error.into()),
            }
        } else if metadata.file_type().is_symlink() {
            let link = fs::read_link(source)?;
            ensure!(
                !link.is_absolute()
                    && lexical(
                        &source
                            .parent()
                            .context("Electron symlink parent")?
                            .join(&link)
                    )?
                    .starts_with(root),
                "Electron distribution symlink escapes its root"
            );
            std::os::unix::fs::symlink(link, target)?;
        } else {
            anyhow::bail!("Unsupported Electron distribution entry");
        }
        Ok(())
    }
    copy(&source, &source, &target, runner, 0, &mut 0)
}

fn ensure_directory(path: &Path) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(path) {
        ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "Expected directory, refusing symlink runtime path: {}",
            path.display()
        );
    }
    Ok(())
}
pub fn bind_mount(source: &Path, target: &Path, readonly: bool) -> Result<String> {
    let quote = |key: &str, s: &OsStr| -> Result<String> {
        let s = s.to_str().context("Non-UTF8 Docker bind path")?;
        ensure!(!s.contains(['\0', '\n', '\r']), "Invalid Docker bind path");
        Ok(format!("\"{key}={}\"", s.replace('"', "\"\"")))
    };
    Ok(format!(
        "type=bind,{},{}{}",
        quote("source", source.as_os_str())?,
        quote("target", target.as_os_str())?,
        if readonly { ",readonly" } else { "" }
    ))
}
fn verify_managed_seccomp(root: &Path, state: &ProjectState) -> Result<PathBuf> {
    let seccomp_name = ".devkit/runtime/desktop-seccomp.json";
    let expected = state
        .managed
        .get(seccomp_name)
        .context("Missing managed desktop-seccomp.json")?;
    let seccomp = safe_path(root, seccomp_name)?;
    ensure!(
        identity(&seccomp)?.is_some_and(|actual| actual.matches_git_tracked(expected)),
        "Managed desktop-seccomp.json differs from the accepted profile"
    );
    Ok(seccomp)
}

fn runtime_paths(admission: &ToolAdmission) -> Result<()> {
    for d in [".devkit/state", ".devkit/cache", ".devkit/cache/pnpm-store"] {
        ensure_directory(&safe_path(&admission.root, d)?)?;
    }
    for p in [
        ".devkit/state/node-workspace.lock",
        ".devkit/state/rust-workspace.lock",
        ".devkit/state/rust-image.json",
        ".devkit/state/desktop-image.json",
    ] {
        read_regular_file(&safe_path(&admission.root, p)?, 4096, "runtime receipt")?;
    }
    Ok(())
}
fn runtime_pending(root: &Path) -> Result<()> {
    let path = safe_path(root, ".devkit")?.join("update-pending.json");
    match fs::symlink_metadata(path) {
        Ok(_) => anyhow::bail!("An update is incomplete; run devkit recover first"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn node_ready(ctx: &PreparedContext<'_>, node_inputs_digest: &str) -> Result<()> {
    if ctx.node_ready_verified.get() {
        return Ok(());
    }
    let expected = crate::doctor::NodeWorkspaceReceipt::new(
        ctx.daemon()?.platform,
        &ctx.admission.state.release_identity,
        node_inputs_digest,
    );
    ensure!(
        crate::doctor::read_node_receipt(&ctx.admission.root)?.as_ref() == Some(&expected)
            && safe_path(&ctx.admission.root, "node_modules")?.is_dir(),
        "workspace dependencies are absent/stale; run make setup"
    );
    ctx.node_ready_verified.set(true);
    Ok(())
}

fn workspace_fingerprint(state: &ProjectState, inputs: &InputIdentitySet) -> String {
    digest(format!("{}\n{}", state.release_identity, inputs.digest))
}

/// Shared Cargo caches must never carry host-wide config or credential hooks
/// that would execute arbitrary wrappers across unrelated checkouts.
pub(crate) fn assert_clean_cargo_home(cache: &Path) -> Result<()> {
    for name in ["config", "config.toml", "credentials", "credentials.toml"] {
        let path = cache.join(name);
        ensure!(
            !exists(&path)?,
            "Refusing unexpected Cargo configuration in shared cache: {}; remove it before continuing",
            path.display()
        );
    }
    Ok(())
}

/// Pin Cargo's compiler and network-helper hooks so project .cargo/config.toml
/// cannot substitute rustc/rustdoc/wrappers or spawn host git CLI helpers.
pub(crate) fn cargo_safety_env() -> [&'static str; 9] {
    [
        "RUSTC=/usr/local/cargo/bin/rustc",
        "RUSTDOC=/usr/local/cargo/bin/rustdoc",
        "RUSTC_WRAPPER=",
        "RUSTC_WORKSPACE_WRAPPER=",
        "CARGO_NET_GIT_FETCH_WITH_CLI=false",
        "RUSTFLAGS=",
        "CARGO_ENCODED_RUSTFLAGS=",
        "RUSTDOCFLAGS=",
        "CARGO_ENCODED_RUSTDOCFLAGS=",
    ]
}

/// Mask checkout-root and component-level secret files (.env, .env.*, .npmrc)
/// with /dev/null inside automatic networked Rust containers (setup-rust and
/// Cargo lock resolution), refusing symlinked secret files fail-closed.
pub(crate) fn mount_sensitive_workspace_masks(command: &mut Command, root: &Path) -> Result<()> {
    let mut rel_paths = BTreeSet::new();
    let mut scan_dirs: Vec<(String, PathBuf)> = vec![(String::new(), root.to_path_buf())];
    for group in ["apps", "packages"] {
        let group_dir = root.join(group);
        if fs::symlink_metadata(&group_dir)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
        {
            for entry in sorted_entries(&group_dir)? {
                let path = entry.path();
                if let Some(child) = entry.file_name().to_str()
                    && fs::symlink_metadata(&path)
                        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
                {
                    scan_dirs.push((format!("{group}/{child}"), path));
                }
            }
        }
    }
    for (prefix, dir) in scan_dirs {
        for entry in sorted_entries(&dir)? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let sensitive = name == ".npmrc"
                || name == ".env"
                || (name.starts_with(".env.")
                    && !matches!(
                        name.as_str(),
                        ".env.example" | ".env.sample" | ".env.template"
                    ));
            if !sensitive {
                continue;
            }
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let meta = fs::symlink_metadata(entry.path())?;
            ensure!(
                !meta.file_type().is_symlink(),
                "Refusing symlinked sensitive file inside Rust workspace: {rel}"
            );
            if meta.is_file() {
                rel_paths.insert(rel);
            }
        }
    }
    for rel in rel_paths {
        crate::shared_cache::mount(
            command,
            Path::new("/dev/null"),
            &Path::new("/workspace").join(rel),
            true,
        )?;
    }
    Ok(())
}

/// One bounded record per checkout kept outside the container-mounted Cargo home.
/// Checking receipts must never create the selected cache (including for info).
fn cargo_cache_receipt(root: &Path, cache_dir: Option<&Path>) -> Result<PathBuf> {
    let cache = crate::cache::cache_root(cache_dir)?;
    let checkout = digest(root.as_os_str().as_encoded_bytes());
    safe_path(&cache, &format!("cargo-receipts/{checkout}"))
}

pub(crate) fn workspace_ready(
    root: &Path,
    state: &ProjectState,
    native: &crate::native::NativeAdmission,
    toolchain: Toolchain,
    cache_dir: Option<&Path>,
) -> Result<bool> {
    let inputs = match toolchain {
        Toolchain::Node if safe_path(root, "node_modules")?.is_dir() => {
            return Ok(crate::doctor::read_node_receipt(root)?.is_some_and(|r| {
                r == crate::doctor::NodeWorkspaceReceipt::new(
                    &r.platform,
                    &state.release_identity,
                    &native.node_inputs.digest,
                )
            }));
        }
        Toolchain::Rust if native.cargo_workspace.is_some() => &native.rust_inputs,
        _ => return Ok(false),
    };
    let path = safe_path(root, &format!(".devkit/state/{toolchain}-workspace.lock"))?;
    let expected = workspace_fingerprint(state, inputs);
    let matches = |path: &Path| -> Result<bool> {
        Ok(
            read_regular_file(path, 4096, "workspace receipt")?.is_some_and(|(bytes, _)| {
                std::str::from_utf8(&bytes).is_ok_and(|value| value.trim() == expected)
            }),
        )
    };
    Ok(matches(&path)?
        && (toolchain == Toolchain::Node || matches(&cargo_cache_receipt(root, cache_dir)?)?))
}

fn require_workspace(
    admission: &RuntimeAdmission,
    toolchain: Toolchain,
    cache_dir: Option<&Path>,
) -> Result<()> {
    ensure!(
        workspace_ready(
            &admission.root,
            &admission.state,
            &admission.native,
            toolchain,
            cache_dir
        )?,
        "{toolchain} workspace dependencies are absent/stale; run make setup"
    );
    Ok(())
}

fn rust_gate_argv(
    command: &str,
    selection: Option<(&str, &[CargoMember])>,
) -> Result<Vec<OsString>> {
    let mut argv: Vec<OsString> = [
        "/devkit/devkit",
        "internal",
        "rust",
        command,
        "--workspace-root",
        "/workspace",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    if let Some((component, members)) = selection {
        argv.extend(["--component".into(), component.into()]);
        for member in members {
            argv.extend([
                "--member".into(),
                member.rel_dir.as_str().into(),
                "--fmt-package".into(),
                member.fmt_package().into(),
                "--package".into(),
                member.pkgid()?.into(),
            ]);
        }
    }
    Ok(argv)
}

fn node_argv(ctx: &PreparedContext<'_>, argv: Vec<OsString>) -> Vec<OsString> {
    [
        vec![
            "/devkit/devkit".into(),
            "internal".into(),
            "node".into(),
            ctx.admission.pins.node_version.clone().into(),
            "--".into(),
        ],
        argv,
    ]
    .concat()
}
fn npmrc_ignores_scripts(root: &Path) -> Result<bool> {
    let path = input_path(root, ".npmrc")?;
    if !exists(&path)? {
        return Ok(false);
    }
    let text = fs::read_to_string(path)?;
    Ok(text.lines().any(|raw| {
        let line = raw.trim();
        !line.is_empty()
            && !line.starts_with(['#', ';'])
            && line
                .split_once('=')
                .is_some_and(|(k, v)| k.trim() == "ignore-scripts" && v.trim() == "true")
    }))
}

fn manifest_install_hooks(root: &Path, manifest_rel: &str) -> Result<Vec<&'static str>> {
    let path = input_path(root, manifest_rel)?;
    if !exists(&path)? {
        return Ok(Vec::new());
    }
    let pkg: Value = read_json(&path)?;
    let mut hooks = Vec::new();
    if let Some(scripts) = pkg.get("scripts").and_then(Value::as_object) {
        for hook in ["preinstall", "install", "postinstall", "prepare"] {
            if scripts
                .get(hook)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
            {
                hooks.push(hook);
            }
        }
    }
    Ok(hooks)
}

fn component_is_public_library(root: &Path, path: &str, c: &ComponentState) -> Result<bool> {
    if matches!(
        c.tooling_profile,
        ToolingProfile::TsLibrary | ToolingProfile::ReactUi
    ) {
        let manifest: Value = read_json(&input_path(root, format!("{path}/package.json"))?)?;
        Ok(manifest.get("private") == Some(&Value::Bool(false)))
    } else {
        Ok(false)
    }
}
pub fn run(request: RuntimeRequest, runner: &ProcessRunner) -> Result<()> {
    if request.command == "help" {
        print!("{HELP}");
        return Ok(());
    }
    ensure!(
        [
            "doctor",
            "setup",
            "setup-node",
            "setup-rust",
            "check",
            "fix",
            "test",
            "build",
            "package",
            "dev",
            "up",
            "down",
            "logs",
            "status",
            "snapshot",
            "dev-snapshot",
            "package-snapshot"
        ]
        .contains(&request.command.as_str()),
        "Unknown task command: {}",
        request.command
    );
    // Pending objects block before creating/opening a lock, regardless of type.
    let pre_root = project_root(&request.root)?;
    runtime_pending(&pre_root)?;
    ensure_supported_checkout_floor(&pre_root, false, MIN_SUPPORTED_PROJECT_VERSION)?;
    if request.command == "down" && !exists(&safe_path(&pre_root, ".devkit/state")?)? {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        crate::stand::history::History::read(&pre_root, runner, deadline)?;
    }
    let shared_lock = matches!(request.command.as_str(), "doctor" | "status" | "logs");
    let lock_op = format!("task {}", request.command);
    let lock = if shared_lock {
        CheckoutLock::acquire_shared(&request.root, &lock_op, Some(runner))?
    } else {
        CheckoutLock::acquire_exclusive(&request.root, &lock_op, Some(runner))?
    };
    if request.command == "down" {
        use crate::stand::{history::History, ledger::Ledger, ports::Ports};
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut history = History::read(&pre_root, runner, deadline)?;
        if let Some(history) = &history {
            history.prove_lease_history(&pre_root)?;
            if history.has_daemon() {
                let ports = Ports::read(&pre_root)?;
                if let Some(ledger) = Ledger::read(&pre_root)? {
                    ensure!(
                        ports.is_some_and(|ports| ports.owner == ledger.owner),
                        "Stand actor history differs from lease ownership"
                    );
                } else {
                    ensure!(
                        !ports.is_some_and(|ports| ports.possible_actors),
                        "Stand actor history is missing; preserving leases until ownership recovery"
                    );
                }
            }
        }
        if request.cleanup && cleanup_without_stand(&request, &pre_root, history.as_mut())? {
            return Ok(());
        }
    }
    let service = ["status", "logs", "down"].contains(&request.command.as_str());
    let tool = if service {
        ToolAdmission::capture_service(&request)?
    } else {
        ToolAdmission::capture(&request)?
    };
    runtime_paths(&tool)?;
    let full = if service {
        None
    } else {
        Some(admit_runtime_snapshot_with_pins(
            &tool.root,
            tool.state.clone(),
            tool.manifest_identity.clone(),
            Some(tool.pins.clone()),
        )?)
    };
    let target = tool.target(&request)?;
    let mut ctx = PreparedContext::new(tool, runner, request.cache_dir.as_deref())?;
    if ["snapshot", "dev-snapshot", "package-snapshot"].contains(&request.command.as_str()) {
        if request.command == "dev-snapshot" {
            ensure!(
                ctx.admission
                    .state
                    .components
                    .values()
                    .any(|c| c.role == Role::App),
                "Project has no runnable apps; add a playground or app with devkit add-app"
            );
            ensure!(
                request.package.is_none(),
                "Development requires an app selector"
            );
        } else if request.command == "package-snapshot" {
            if let Some(pkg) = &request.package {
                let pkg_path = format!("packages/{pkg}");
                let c = &ctx.admission.state.components[&pkg_path];
                ensure!(
                    matches!(
                        c.tooling_profile,
                        ToolingProfile::TsLibrary | ToolingProfile::ReactUi
                    ),
                    "Package {pkg_path} uses internal profile {} and cannot be packaged as a tarball",
                    c.tooling_profile
                );
                ensure!(
                    component_is_public_library(&ctx.admission.root, &pkg_path, c)?,
                    "Package {pkg_path} has \"private\": true in package.json; set \"private\": false and run devkit sync before packaging (then make setup)"
                );
            } else if let Some(app) = &request.app {
                let app_path = format!("apps/{app}");
                ensure!(
                    ctx.admission.state.components[&app_path].tooling_profile
                        == ToolingProfile::Electron,
                    "Application {app_path} is not a desktop Electron app"
                );
            } else {
                let mut any_packageable = false;
                let mut hint_slug: Option<String> = None;
                for (path, c) in &ctx.admission.state.components {
                    if c.tooling_profile == ToolingProfile::Electron {
                        any_packageable = true;
                        break;
                    }
                    if matches!(
                        c.tooling_profile,
                        ToolingProfile::TsLibrary | ToolingProfile::ReactUi
                    ) {
                        if hint_slug.is_none() {
                            hint_slug = path.rsplit('/').next().map(str::to_owned);
                        }
                        if component_is_public_library(&ctx.admission.root, path, c)? {
                            any_packageable = true;
                            break;
                        }
                    }
                }
                if !any_packageable {
                    let slug_hint = hint_slug.as_deref().unwrap_or("<name>");
                    anyhow::bail!(
                        "No packageable components found; set \"private\": false in packages/{slug_hint}/package.json and run devkit sync, or add an Electron app (then make setup)"
                    );
                }
            }
        }
        return Ok(());
    }
    if service {
        ctx.prerequisite_service()?;
        run_service(&request, target.as_deref(), &mut ctx, lock)?;
        return Ok(());
    }
    ctx.prerequisite()?;
    let full = full.context("Missing quality admission")?;
    let node_digest = &full.native.node_inputs.digest;
    if !full.state.components.is_empty() {
        ctx.ensure_linux_helper(matches!(
            request.command.as_str(),
            "setup" | "setup-node" | "setup-rust"
        ))?;
    }
    let command = request.command.as_str();
    if command == "setup-rust" {
        ensure!(
            full.native.cargo_workspace.is_some(),
            "Rust workspace is required"
        );
        let receipt = safe_path(&ctx.admission.root, ".devkit/state/rust-workspace.lock")?;
        let cached = cargo_cache_receipt(&full.root, ctx.cache_dir.as_deref())?;
        remove_file(&receipt)?;
        remove_file(&cached)?;
        let image = ctx.derived_image("rust", true)?;
        ctx.task(ContainerTask {
            component: None,
            profile: ToolingProfile::Rust,
            electron_cache: false,
            image: &image,
            argv: &rust_gate_argv("setup", None)?,
            network: "bridge",
            forward_npm_secrets: false,
            access: TaskAccess::SetupRust,
            inputs: None,
        })?;
        full.recheck()?;
        runner.check_cancelled()?;
        let fingerprint = format!(
            "{}\n",
            workspace_fingerprint(&full.state, &full.native.rust_inputs)
        );
        write_atomic(&cached, fingerprint.as_bytes(), 0o644)?;
        write_atomic(&receipt, fingerprint.as_bytes(), 0o644)?;
        return Ok(());
    }
    if command == "setup-node" {
        if full.state.components.values().any(|c| c.role == Role::App) {
            let running = ctx.runner.capture(ctx.compose()?.args([
                "--profile",
                "*",
                "ps",
                "--status",
                "running",
                "--quiet",
            ]))?;
            ensure!(
                running.text()?.trim().is_empty(),
                "Development services are running; stop them with 'make down' before running 'make setup'"
            );
        }
        let receipt = safe_path(&ctx.admission.root, ".devkit/state/node-workspace.lock")?;
        if identity(&receipt)?.is_some() {
            fs::remove_file(&receipt)?;
        }
        ctx.node_image(true)?;
        ctx.task(ContainerTask {
            component: None,
            profile: ToolingProfile::Node,
            electron_cache: false,
            image: &ctx.admission.pins.node_image,
            argv: &node_argv(
                &ctx,
                vec![
                    "pnpm".into(),
                    "install".into(),
                    "--frozen-lockfile".into(),
                    "--prefer-offline".into(),
                    "--trust-lockfile".into(),
                    "--ignore-scripts".into(),
                    "--ignore-pnpmfile".into(),
                    "--config.strict-peer-dependencies=true".into(),
                ],
            ),
            network: "bridge",
            forward_npm_secrets: true,
            access: TaskAccess::SetupNode,
            inputs: Some(&full.native.inputs),
        })?;
        if !npmrc_ignores_scripts(&ctx.admission.root)? {
            for (path, c) in &full.state.components {
                if c.toolchain != Toolchain::Node {
                    continue;
                }
                for hook in
                    manifest_install_hooks(&ctx.admission.root, &format!("{path}/package.json"))?
                {
                    ctx.task(ContainerTask {
                        component: Some(path),
                        profile: c.tooling_profile,
                        electron_cache: c.tooling_profile == ToolingProfile::Electron,
                        image: &ctx.admission.pins.node_image,
                        argv: &node_argv(&ctx, vec!["pnpm".into(), "run".into(), hook.into()]),
                        network: "none",
                        forward_npm_secrets: false,
                        access: TaskAccess::Finite,
                        inputs: Some(&full.native.inputs),
                    })?;
                }
            }
            for hook in manifest_install_hooks(&ctx.admission.root, "package.json")? {
                ctx.task(ContainerTask {
                    component: None,
                    profile: ToolingProfile::Node,
                    electron_cache: has_electron(&full.state),
                    image: &ctx.admission.pins.node_image,
                    argv: &node_argv(&ctx, vec!["pnpm".into(), "run".into(), hook.into()]),
                    network: "none",
                    forward_npm_secrets: false,
                    access: TaskAccess::Finite,
                    inputs: Some(&full.native.inputs),
                })?;
            }
        }
        for (path, c) in &full.state.components {
            if c.tooling_profile == ToolingProfile::Electron {
                ctx.setup_electron(path, &full.native.inputs)?;
            }
        }
        full.recheck()?;
        runner.check_cancelled()?;
        write_json(
            &receipt,
            &crate::doctor::NodeWorkspaceReceipt::new(
                ctx.daemon()?.platform,
                &full.state.release_identity,
                node_digest,
            ),
        )?;
        return Ok(());
    }
    if command == "doctor" {
        crate::doctor::ensure_disk_space(&ctx.admission.root)?;
        let mut checked_rust = false;
        let mut checked_node = false;
        let mut checked_desktop = false;
        for c in full.state.components.values().cloned().collect::<Vec<_>>() {
            if c.toolchain == Toolchain::Rust {
                if !checked_rust {
                    ctx.derived_image("rust", false)?;
                    require_workspace(&full, Toolchain::Rust, ctx.cache_dir.as_deref())?;
                    checked_rust = true;
                }
            } else if !checked_node {
                ctx.node_image(false)?;
                node_ready(&ctx, node_digest)?;
                checked_node = true;
            }
            if c.tooling_profile == ToolingProfile::Electron && !checked_desktop {
                ctx.derived_image("desktop", false)?;
                checked_desktop = true;
            }
        }
        full.recheck()?;
        runner.check_cancelled()?;
        println!(
            "Devkit {}; Node {}; Rust {}; accepted components verified.",
            ctx.admission.pins.version,
            ctx.admission.pins.node_version,
            ctx.admission.pins.rust_version
        );
        return Ok(());
    }
    let target = target
        .context("Finite/development task must select a registered component through Make")?;
    let component = full.state.components[&target].clone();
    if ["dev", "up"].contains(&command) {
        run_development(&request, &target, &full, node_digest, &mut ctx)?;
        full.recheck()?;
        return Ok(());
    }
    let setup = command == "setup";
    if component.toolchain == Toolchain::Rust {
        ensure!(
            command != "package",
            "package is defined for desktop applications and publishable libraries"
        );
        let image = ctx.derived_image("rust", false)?;
        require_workspace(&full, Toolchain::Rust, ctx.cache_dir.as_deref())?;
        if !setup {
            let workspace = full
                .native
                .cargo_workspace
                .as_ref()
                .context("Missing Cargo workspace")?;
            let argv = rust_gate_argv(command, Some((&target, workspace.members(&target)?)))?;
            ctx.task(ContainerTask {
                component: Some(&target),
                profile: component.tooling_profile,
                electron_cache: false,
                image: &image,
                argv: &argv,
                network: "none",
                forward_npm_secrets: false,
                access: if command == "fix" {
                    TaskAccess::Fix
                } else {
                    TaskAccess::Finite
                },
                inputs: None,
            })?;
        }
    } else {
        ctx.node_image(false)?;
        node_ready(&ctx, node_digest)?;
        let image = if component.tooling_profile == ToolingProfile::Electron {
            ctx.derived_image("desktop", setup)?
        } else {
            ctx.admission.pins.node_image.clone()
        };
        let mut access = if command == "fix" {
            TaskAccess::Fix
        } else {
            TaskAccess::Finite
        };
        if command == "package" {
            match component.tooling_profile {
                ToolingProfile::Electron => {}
                ToolingProfile::TsLibrary | ToolingProfile::ReactUi => {
                    if !component_is_public_library(&ctx.admission.root, &target, &component)? {
                        if request.package_aggregate {
                            full.recheck()?;
                            runner.check_cancelled()?;
                            drop(lock);
                            return Ok(());
                        }
                        anyhow::bail!(
                            "Package {target} has \"private\": true in package.json; set \"private\": false and run devkit sync before packaging (then make setup)"
                        );
                    }
                    access = TaskAccess::PackageLibrary;
                }
                _ => {
                    if component.role == Role::Library {
                        anyhow::bail!(
                            "Package {target} uses internal profile {} and cannot be packaged as a tarball",
                            component.tooling_profile
                        );
                    }
                    anyhow::bail!("Application {target} is not a desktop Electron app");
                }
            }
        }
        if !setup {
            ctx.task(ContainerTask {
                component: Some(&target),
                profile: component.tooling_profile,
                electron_cache: component.tooling_profile == ToolingProfile::Electron,
                image: &image,
                argv: &node_argv(&ctx, vec!["pnpm".into(), "run".into(), command.into()]),
                network: "none",
                forward_npm_secrets: false,
                access,
                inputs: Some(&full.native.inputs),
            })?;
        }
    }
    if command == "fix" {
        ctx.admission.recheck()?;
    } else {
        full.recheck()?;
    }
    runner.check_cancelled()?;
    drop(lock);
    Ok(())
}
/// This path consumes the accepted manifest, engine pin and stand receipts only.
/// Project/Compose/SDK inputs are irrelevant when no launch can need stopping.
fn cleanup_without_stand(
    request: &RuntimeRequest,
    root: &Path,
    history: Option<&mut crate::stand::history::History>,
) -> Result<bool> {
    if request.component.is_some()
        || request.app.is_some()
        || request.package.is_some()
        || !crate::stand::epoch::trusted(root)?
    {
        return Ok(false);
    }
    let ledger = crate::stand::ledger::Ledger::read(root)?;
    if ledger.as_ref().is_some_and(|l| !l.empty_or_prepared()) {
        return Ok(false);
    }
    // After daemon admission accept an explicit empty ledger paired with its
    // lease receipt, or the stopped proof recorded by an earlier cleanup.
    let needs_proof = history
        .as_ref()
        .is_some_and(|history| history.may_have_actors());
    if needs_proof && ledger.is_none() {
        return Ok(false);
    }
    let unarmed = ledger.as_ref().is_some_and(|l| !l.inspections.is_empty());
    let manifest =
        identity(&safe_path(root, MANIFEST_PATH)?)?.context("Missing accepted manifest")?;
    if let Some(expected) = &request.expected_manifest {
        ensure!(
            expected == &manifest.sha256,
            "accepted toolkit changed during this Make invocation; rerun the command"
        );
    }
    let state = read_accepted_header(root)?;
    let pin_path = safe_path(root, ".devkit/runtime/toolchains.env")?;
    let (bytes, actual) = read_regular_file(
        &pin_path,
        TOOLCHAIN_PINS_MAX_BYTES as u64,
        "accepted engine pins",
    )?
    .context("Missing accepted engine pins")?;
    ensure!(
        state
            .managed
            .get(".devkit/runtime/toolchains.env")
            .is_some_and(|id| actual.matches_git_tracked(id)),
        "Managed engine pins changed"
    );
    let pins = EnginePins::parse(&bytes)?;
    ensure!(
        pins.version == state.version && state.version == env!("CARGO_PKG_VERSION"),
        "Accepted engine version differs from its build"
    );
    crate::delivery::verify_current_engine(
        &pins.for_target(crate::distribution::Target::host()?)?,
    )?;
    let mut ports = crate::stand::ports::Ports::read(root)?;
    if let Some(ledger) = &ledger {
        ensure!(
            ports.as_ref().is_some_and(|p| p.owner == ledger.owner),
            "Completed stand history has no matching ownership receipt"
        );
    }
    ensure!(
        unarmed || ports.as_ref().is_none_or(|p| !p.possible_actors),
        "Stand actor history is missing or inconsistent; preserving leases until ownership recovery"
    );
    runtime_pending(root)?;
    ensure!(
        identity(&safe_path(root, MANIFEST_PATH)?)?.as_ref() == Some(&manifest)
            && crate::stand::epoch::trusted(root)?,
        "Accepted runtime changed during cleanup"
    );
    crate::stand::clear_overlay(root)?;
    if unarmed
        && let Some(ports) = &mut ports
        && ports.possible_actors
    {
        ports.possible_actors = false;
        ports.save()?;
    }
    if let Some(history) = history.filter(|_| needs_proof) {
        history.record_stopped(root)?;
    }
    if ledger.is_some() {
        remove_file(&safe_path(root, crate::stand::ledger::ACTIVE)?)?;
    }
    if let Some(ports) = ports {
        ports.release()?;
    }
    Ok(true)
}
fn read_cargo_manifest(directory: &Path) -> Result<toml::Value> {
    let path = input_path(directory, "Cargo.toml")?;
    let source =
        read_input_bytes(&path).with_context(|| format!("Cannot read {}", path.display()))?;
    toml::from_str(std::str::from_utf8(&source)?)
        .with_context(|| format!("Invalid Cargo manifest: {}", path.display()))
}

/// Explicit Cargo commands can repair the lock and member manifests. Only
/// require a virtual root and enough membership evidence to select the workdir.
fn admit_cargo_exec(root: &Path, target: Option<&str>) -> Result<()> {
    let manifest = read_cargo_manifest(root)?;
    let workspace = manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
        .context("Rust exec requires a virtual root Cargo workspace")?;
    ensure!(
        manifest.get("package").is_none(),
        "Rust exec requires a virtual root Cargo workspace"
    );
    if let Some(path) = target {
        let directory = input_path(root, path)?;
        ensure!(
            directory.is_dir(),
            "Missing registered Rust component: {path}"
        );
        ensure!(
            identity(&input_path(root, format!("{path}/package.json"))?)?.is_none(),
            "Ambiguous native component: {path}"
        );
        if identity(&input_path(root, format!("{path}/Cargo.toml"))?)?.is_none() {
            let mut found = false;
            for item in workspace
                .get("members")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                let member = item.as_str().context("Invalid workspace member path")?;
                let manifest = input_path(root, Path::new(member).join("Cargo.toml"))?;
                if manifest.starts_with(&directory) && manifest.is_file() {
                    found = true;
                    break;
                }
            }
            ensure!(found, "No Cargo manifest or workspace member for {path}");
        }
    }
    Ok(())
}

pub fn exec(request: ExecRequest, runner: &ProcessRunner) -> Result<()> {
    ensure!(!request.argv.is_empty(), "exec requires ARGV after --");
    ensure!(
        ["none", "bridge"].contains(&request.network.as_str()),
        "network must be none or bridge"
    );
    let pre_root = project_root(&request.runtime.root)?;
    runtime_pending(&pre_root)?;
    ensure_supported_checkout_floor(&pre_root, false, MIN_SUPPORTED_PROJECT_VERSION)?;
    let lock_mode = if request.write || request.prepare {
        CheckoutLockMode::Exclusive
    } else {
        CheckoutLockMode::Shared
    };
    let _lock = CheckoutLock::acquire_with(
        &request.runtime.root,
        lock_mode,
        if request.write {
            "exec --write"
        } else if request.prepare {
            "exec --prepare"
        } else {
            "exec"
        },
        Some(runner),
    )?;
    let tool = ToolAdmission::capture(&request.runtime)?;
    runtime_paths(&tool)?;
    let target = tool.target(&request.runtime)?;
    let has_rust = tool
        .state
        .components
        .values()
        .any(|c| c.toolchain == Toolchain::Rust);
    let has_node = !node_components(&tool.state).is_empty();
    ensure!(
        target.is_none() || request.toolchain.is_none(),
        "Choose a component or a root toolchain; remove the extra selector"
    );
    let toolchain = target
        .as_ref()
        .map(|t| tool.state.components[t].toolchain)
        .or(request.toolchain)
        .unwrap_or(if has_node {
            Toolchain::Node
        } else {
            Toolchain::Rust
        });
    ensure!(
        match toolchain {
            Toolchain::Node => has_node,
            Toolchain::Rust => has_rust,
        },
        "No registered {toolchain} workspace for exec"
    );
    if toolchain == Toolchain::Rust {
        admit_cargo_exec(&tool.root, target.as_deref())?;
    } else if let Some(path) = &target {
        ensure!(
            identity(&input_path(&tool.root, format!("{path}/package.json"))?)?.is_some(),
            "Missing registered native manifest: {path}/package.json"
        );
        ensure!(
            identity(&input_path(&tool.root, format!("{path}/Cargo.toml"))?)?.is_none(),
            "Ambiguous native component: {path}"
        );
    } else {
        ensure!(
            identity(&input_path(&tool.root, "package.json")?)?.is_some(),
            "Missing root package.json for Node workspace"
        );
    }
    let mut ctx = PreparedContext::new(tool, runner, request.runtime.cache_dir.as_deref())?;
    ctx.prerequisite()?;
    ctx.ensure_linux_helper(request.prepare)?;
    let image = if toolchain == Toolchain::Rust {
        ctx.derived_image("rust", request.prepare)?
    } else {
        ctx.node_image(request.prepare)?;
        ctx.pnpm(request.prepare)?;
        ctx.admission.pins.node_image.clone()
    };
    if request.write {
        let receipt = safe_path(
            &ctx.admission.root,
            &format!(".devkit/state/{toolchain}-workspace.lock"),
        )?;
        let mut invalidated = false;
        if identity(&receipt)?.is_some() {
            remove_file(&receipt)?;
            invalidated = true;
        }
        if toolchain == Toolchain::Rust {
            let cached = cargo_cache_receipt(&ctx.admission.root, ctx.cache_dir.as_deref())?;
            if identity(&cached)?.is_some() {
                remove_file(&cached)?;
                invalidated = true;
            }
        }
        if invalidated {
            eprintln!(
                "devkit: {toolchain} workspace receipt invalidated by --write; run 'make sync' and 'make setup' after dependency changes"
            );
        }
    }
    let task_result = ctx.task(ContainerTask {
        component: target.as_deref(),
        profile: if toolchain == Toolchain::Rust {
            ToolingProfile::Rust
        } else {
            ToolingProfile::Node
        },
        electron_cache: toolchain != Toolchain::Rust
            && target.as_deref().map_or_else(
                || has_electron(&ctx.admission.state),
                |p| ctx.admission.state.components[p].tooling_profile == ToolingProfile::Electron,
            ),
        image: &image,
        argv: &request.argv,
        network: &request.network,
        forward_npm_secrets: request.network != "none",
        access: if request.write {
            TaskAccess::ExecWrite
        } else {
            TaskAccess::ExecReadOnly
        },
        inputs: None,
    });
    if let Err(err) = task_result {
        if let Some(cf) = err.downcast_ref::<CommandFailure>() {
            // The Node image's entrypoint treats an unknown executable as a
            // JavaScript module and returns 1; alternate entrypoints use 127.
            if matches!(cf.code, 1 | 127)
                && has_node
                && has_rust
                && target.is_none()
                && request.toolchain.is_none()
                && request.argv[0]
                    .to_str()
                    .is_some_and(|cmd| matches!(cmd, "cargo" | "rustc" | "rustfmt" | "rustup"))
            {
                return Err(failure(
                    cf.code,
                    format!(
                        "{}; root exec uses Node here. Select toolchain=rust (--toolchain rust), app=<name>, or package=<name>",
                        cf.message
                    ),
                ));
            }
            if !request.write || request.network == "none" {
                return Err(failure(
                    cf.code,
                    format!(
                        "{}; use write=1 (--write) to modify checkout files, network=bridge (--network bridge) for network access, or prepare=1 (--prepare) for missing tool images",
                        cf.message
                    ),
                ));
            }
        }
        return Err(err);
    }
    ctx.admission.recheck()?;
    runner.check_cancelled()?;
    Ok(())
}
fn service_name<'a>(path: &'a str, c: &'a ComponentState) -> &'a str {
    c.development
        .as_ref()
        .and_then(|d| d.service.as_deref())
        .unwrap_or_else(|| path.rsplit('/').next().expect("component basename"))
}
fn run_service(
    request: &RuntimeRequest,
    target: Option<&str>,
    ctx: &mut PreparedContext<'_>,
    lock: CheckoutLock,
) -> Result<()> {
    use crate::stand::history::{History, ResourceDeclarations};
    use std::time::{Duration, Instant};
    ctx.load_recorded_images(false)?;
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
    let mut resource_history = if request.command == "down" {
        let deadline = Instant::now() + Duration::from_secs(3);
        let value = History::read(&ctx.admission.root, ctx.runner, deadline)?;
        if let Some(history) = &value {
            history.prove_lease_history(&ctx.admission.root)?;
            history.verify_daemon(ctx.runner, deadline)?;
        }
        match value.filter(History::has_daemon) {
            Some(history) => {
                let model = normalize_stand(ctx, Some(Instant::now() + Duration::from_secs(3)))?;
                let declarations =
                    ResourceDeclarations::read(&ctx.admission.root, &inventory, &model)?;
                Some((history, declarations))
            }
            None => None,
        }
    } else {
        None
    };
    let selected = if let Some(path) = target {
        let c = &ctx.admission.state.components[path];
        ensure!(
            c.role == Role::App,
            "development services require an app selector"
        );
        Some(service_name(path, c).to_owned())
    } else {
        None
    };
    let mut command = ctx.compose()?;
    ctx.env(&mut command);
    if request.command == "down"
        && let Some(mut ledger) = crate::stand::ledger::Ledger::read(&ctx.admission.root)?
    {
        ledger.recover_launch(&ctx.admission.root, &ctx.project, ctx.runner)?;
        if !ledger.inspections.is_empty() {
            let mut ports = crate::stand::ports::Ports::read(&ctx.admission.root)?
                .context("Missing preflight ownership receipt")?;
            crate::stand::inspection::recover(
                &ctx.admission.root,
                &mut ledger,
                &mut ports,
                ctx.runner,
            )?;
        }
    }
    if request.command == "down" && request.cleanup {
        let rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
        if !rows.is_empty() {
            let ports = crate::stand::ports::Ports::read(&ctx.admission.root)?
                .context("Stand containers lack an ownership receipt; restore it before cleanup")?;
            crate::stand::ledger::verify_owners(&rows, &ports.owner)?;
        }
    }
    let mut down_actors = None;
    if let Some((history, declarations)) = &mut resource_history {
        let mut rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
        if let Some(ports) = crate::stand::ports::Ports::read(&ctx.admission.root)? {
            rows.retain(|row| row.owner == ports.owner.key);
            history.record_anonymous(&ctx.admission.root, &rows, declarations, ctx.runner)?;
            down_actors = Some((ports.owner, rows));
        } else {
            ensure!(
                rows.is_empty(),
                "Stand containers lack an ownership receipt"
            );
        }
        assert_compose_inventory(&ctx.admission.root, &inventory)?;
    }
    match request.command.as_str() {
        "down" => {
            command.args(["--profile", "*", "down"]);
        }
        "status" => {
            command.args(["--profile", "*", "ps", "--all", "--format", "json"]);
            if let Some(s) = &selected {
                command.arg(s);
            }
        }
        "logs" => {
            command.args([
                "--profile",
                "*",
                "logs",
                "--tail",
                &request.tail.to_string(),
            ]);
            if request.follow {
                command.arg("--follow");
            }
            if let Some(s) = &selected {
                command.arg(s);
            }
        }
        _ => unreachable!(),
    };
    if request.command == "logs" && request.follow {
        assert_compose_inventory(&ctx.admission.root, &inventory)?;
        ctx.admission.recheck()?;
        drop(lock);
        ctx.runner.checked(&mut command, OutputMode::Inherit)?;
        return Ok(());
    }
    if request.command == "status" {
        let output = ctx.runner.capture(&mut command)?;
        let mut services = crate::doctor::parse_compose_ps_json(
            &output.stdout,
            &app_service_component_map(&ctx.admission.state),
        )?;
        let reservations = crate::stand::preview::enrich_status(
            &ctx.admission.root,
            &ctx.project,
            ctx.runner,
            selected.as_deref(),
            &mut services,
        )?;
        let reservation_lines = crate::stand::preview::reservation_lines(&reservations, &services);
        for service in services {
            println!(
                "Service {} ({}): {}",
                service.service, service.name, service.state
            );
            for url in service.urls.iter().chain(&service.preview.preview_urls) {
                println!("  {url}");
            }
            if service.preview.pending_restart {
                println!("  Preview settings pending restart");
            }
            if let Some(diagnostic) = service.preview.diagnostic {
                println!("  Preview unverified: {diagnostic}");
            }
        }
        for line in reservation_lines {
            println!("{line}");
        }
    } else if let Some((owner, rows)) = down_actors {
        crate::stand::ledger::stop_owned(&rows, &ctx.project, &owner, ctx.runner)?;
    } else if resource_history.is_none() {
        ctx.runner.checked(&mut command, OutputMode::Inherit)?;
    }
    if request.command == "down" {
        let rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
        if request.cleanup {
            if let Some(mut ports) = crate::stand::ports::Ports::read(&ctx.admission.root)? {
                crate::stand::ledger::remove_owned(&rows, &ctx.project, &ports.owner, ctx.runner)?;
                ports.possible_actors = false;
                ports.save()?;
            } else {
                ensure!(rows.is_empty(), "Unknown stand remnants remain");
            }
        }
        if !request.cleanup
            && rows.is_empty()
            && let Some(ledger) = crate::stand::ledger::Ledger::read(&ctx.admission.root)?
            && let Some(mut ports) = crate::stand::ports::Ports::read(&ctx.admission.root)?
        {
            ensure!(
                ledger.owner == ports.owner && ledger.inspections.is_empty(),
                "Unfinished stand inspections remain"
            );
            ports.possible_actors = false;
            ports.save()?;
        }
        if !request.cleanup
            && rows.is_empty()
            && let Some(mut ports) = crate::stand::ports::Ports::read(&ctx.admission.root)?
        {
            crate::stand::preview::prune(&mut ports, &rows)?;
        }
        if rows.is_empty() || request.cleanup {
            if exists(&safe_path(
                &ctx.admission.root,
                crate::stand::ledger::ACTIVE,
            )?)? {
                remove_file(&safe_path(
                    &ctx.admission.root,
                    crate::stand::ledger::ACTIVE,
                )?)?;
            }
            crate::stand::epoch::anchor(&ctx.admission.root)?;
        }
        if let Some((history, declarations)) = &mut resource_history {
            assert_compose_inventory(&ctx.admission.root, &inventory)?;
            history.remove_resources(
                &ctx.admission.root,
                &ctx.project,
                declarations,
                ctx.runner,
                true,
            )?;
            if rows.is_empty() || request.cleanup {
                history.record_stopped(&ctx.admission.root)?;
            }
        }
        if request.cleanup {
            crate::stand::clear_overlay(&ctx.admission.root)?;
            if let Some(ports) = crate::stand::ports::Ports::read(&ctx.admission.root)? {
                ports.release()?;
            }
        }
    }
    assert_compose_inventory(&ctx.admission.root, &inventory)?;
    ctx.admission.recheck()?;
    drop(lock);
    Ok(())
}
pub fn active_compose_services(model: &Value, selected: &str) -> Result<BTreeSet<String>> {
    let services = object(
        model.get("services").context("Compose services")?,
        "Compose services",
    )?;
    let profiles = |service: &Value| -> Result<Vec<String>> {
        match service.get("profiles") {
            None => Ok(vec![]),
            Some(v) => v
                .as_array()
                .context("Invalid normalized Compose profiles")?
                .iter()
                .map(|p| text(p, "Compose profile").map(str::to_owned))
                .collect(),
        }
    };
    let selected_model = services
        .get(selected)
        .context("Missing selected Compose service")?;
    let active: BTreeSet<_> = profiles(selected_model)?.into_iter().collect();
    let mut seen = BTreeSet::new();
    let mut queue = vec![(selected.to_owned(), true)];
    while let Some((name, required)) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        let Some(service) = services.get(&name) else {
            ensure!(!required, "Missing required Compose dependency: {name}");
            continue;
        };
        let p = profiles(service)?;
        if name != selected && !p.is_empty() && !p.iter().any(|p| active.contains(p)) {
            ensure!(!required, "Compose dependency is not active: {name}");
            continue;
        }
        seen.insert(name);
        for raw in service["volumes_from"].as_array().into_iter().flatten() {
            let raw = raw.as_str().context("Invalid normalized volume provider")?;
            if !raw.starts_with("container:") {
                queue.push((
                    raw.trim_end_matches(":ro").trim_end_matches(":rw").into(),
                    true,
                ));
            }
        }
        if let Some(deps) = service.get("depends_on") {
            for (name, settings) in object(deps, "Compose dependencies")? {
                queue.push((
                    name.clone(),
                    settings.get("required") != Some(&Value::Bool(false)),
                ));
            }
        }
    }
    Ok(seen)
}
/// Normalized Compose, including implicit dependencies and selected profiles,
/// determines which registered applications need a prepared toolchain.
pub fn development_components(
    state: &ProjectState,
    model: &Value,
    selected: &str,
) -> Result<Vec<String>> {
    let seen = active_compose_services(model, selected)?;
    Ok(state
        .components
        .iter()
        .filter(|(p, c)| c.role == Role::App && seen.contains(service_name(p, c)))
        .map(|(p, _)| p.clone())
        .collect())
}
/// Resolve only selected frontend env files for presence/precedence. Resolved
/// service values stay in memory and never reach image inspectors or state.
fn preview_forwarding_presence(
    ctx: &PreparedContext<'_>,
    model: &mut Value,
    frontends: &BTreeSet<String>,
) -> Result<()> {
    let keys = [
        "DEVKIT_ALLOWED_HOSTS",
        "VITE_ALLOWED_HOSTS",
        "DEVKIT_API_TARGET",
    ];
    let mut services = serde_json::Map::new();
    for name in frontends {
        let service = &model["services"][name];
        if let Some(files) = service.get("env_file") {
            services.insert(name.clone(),json!({"image":"scratch","env_file":files,"environment":service.get("environment").cloned().unwrap_or(json!({}))}));
        }
    }
    if services.is_empty() {
        return Ok(());
    }
    let mut spec = json!({"name":ctx.project,"services":services});
    crate::stand::preview::escape_compose(&mut spec);
    let mut command = ctx.docker();
    command
        .current_dir(&ctx.admission.root)
        .args([
            "compose",
            "--project-name",
            &ctx.project,
            "--project-directory",
        ])
        .arg(&ctx.admission.root)
        .args([
            "--env-file",
            "/dev/null",
            "--file",
            "-",
            "config",
            "--format",
            "json",
        ]);
    ctx.env(&mut command);
    let output = ctx
        .runner
        .run_with_input(
            &mut command,
            OutputMode::Capture(16 * 1024 * 1024),
            &json_bytes(&spec)?,
        )?
        .success("Selected frontend environment query")?;
    let resolved: Value = crate::stand::preview::compose_model(&output.stdout)?;
    for name in frontends {
        for key in keys {
            if resolved["services"][name]["environment"].get(key).is_some() {
                if model["services"][name].get("environment").is_none() {
                    model["services"][name]["environment"] = json!({});
                }
                model["services"][name]["environment"]
                    .as_object_mut()
                    .context("Invalid normalized service environment")?
                    .entry(key)
                    .or_insert(Value::Null);
            }
        }
    }
    Ok(())
}
fn normalize_stand(
    ctx: &PreparedContext<'_>,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    let mut compose = ctx.compose()?;
    compose.args([
        "--profile",
        "*",
        "config",
        "--no-env-resolution",
        "--format",
        "json",
    ]);
    let output = match deadline {
        Some(at) => ctx.runner.capture_before(&mut compose, at)?,
        None => ctx.runner.capture(&mut compose)?,
    };
    crate::stand::preview::compose_model(&output.stdout)
}

fn reservation_fingerprint(
    inventory: &ComposeInventory,
    services: &BTreeSet<String>,
    bindings: &[crate::stand::ports::Binding],
) -> Result<String> {
    Ok(digest(serde_json::to_vec(&(
        &inventory.inputs.digest,
        services,
        bindings,
    ))?))
}

pub fn reserve_stand(root: &Path, cache: Option<&Path>, runner: &ProcessRunner) -> Result<bool> {
    use crate::stand::{branch_name_until, epoch, ledger, ports, preview};
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(1);
    let root = project_root(root)?;
    runtime_pending(&root)?;
    ensure_supported_checkout_floor(&root, false, MIN_SUPPORTED_PROJECT_VERSION)?;
    let request = RuntimeRequest::new(root.clone(), "stand reserve".into());
    let before = read_project_state(&root)?;
    let runnable = |c: &ComponentState| {
        c.role == Role::App
            && c.development
                .as_ref()
                .is_some_and(|d| d.kind == DevelopmentKind::Compose)
    };
    if !before.components.values().any(runnable) {
        return Ok(false);
    }
    let _lock = CheckoutLock::acquire_until(
        &root,
        CheckoutLockMode::Exclusive,
        "stand reserve",
        Some(runner),
        Some(deadline),
    )?;
    let adm = ToolAdmission::capture_service(&request)?;
    ensure!(
        epoch::trusted(&root)?,
        "Accepted stand history is unverified; run make down before reserving or starting the stand"
    );
    let ctx = PreparedContext::new(adm, runner, cache)?;
    if let Some(history) = crate::stand::history::History::read(&root, runner, deadline)? {
        history.allow_start()?;
        history.prove_lease_history(&root)?;
    }
    let mut leases = ports::Ports::open(&root, &ctx.cache_root()?)?;
    if leases.possible_actors || ledger::Ledger::read(&root)?.is_some_and(|l| !l.completed_empty())
    {
        ensure!(
            ports::Ports::verified_snapshot(&root)?.is_some(),
            "Stand history needs recovery before reserving ports"
        );
        return Ok(!leases.groups.is_empty());
    }
    let compose = runner.capture_before(
        ctx.docker().args(["compose", "version", "--short"]),
        deadline,
    )?;
    validate_compose_version(compose.text()?)?;
    let inventory = capture_compose_inputs(&root)?;
    let mut stamps = Vec::new();
    for (name, id) in &inventory.inputs.files {
        let Some(id) = id else {
            continue;
        };
        let path = input_path(&root, name)?;
        let (bytes, current, stamp) =
            read_regular_snapshot(&path, 4 * 1024 * 1024, "Compose reservation input")?
                .context("Compose reservation input disappeared")?;
        ensure!(
            &current == id,
            "Compose input changed before reservation: {name}"
        );
        stamps.push((path, stamp));
        if name == ".devkit/compose.apps.yaml" || name.rsplit('/').next() == Some(".env") {
            continue;
        }
        let mut source = std::str::from_utf8(&bytes)?.replace("$$", "");
        for name in [
            "DEVKIT_PROJECT_ROOT",
            "DEVKIT_PROJECT_NAME",
            "DEVKIT_UID",
            "DEVKIT_GID",
            "DEVKIT_NODE_IMAGE",
            "DEVKIT_NODE_VERSION",
            "DEVKIT_RUST_VERSION",
        ] {
            source = source
                .replace(&format!("${{{name}}}"), "")
                .replace(&format!("${{{name}:?}}"), "");
        }
        ensure!(
            !source.contains('$'),
            "Early reservation requires fixed Compose inputs; {name} uses runtime interpolation"
        );
    }
    let model = normalize_stand(&ctx, Some(deadline))?;
    let multiple = ctx
        .admission
        .state
        .components
        .values()
        .filter(|c| c.role == Role::App)
        .count()
        > 1;
    let skip_desktop = multiple
        && (cfg!(target_os = "macos")
            || std::env::var_os("DISPLAY").is_none_or(|value| value.is_empty()));
    let mut selected = BTreeSet::new();
    let mut plans = BTreeMap::new();
    let mut primary = None;
    for (path, c) in ctx
        .admission
        .state
        .components
        .iter()
        .filter(|(_, c)| runnable(c))
    {
        if skip_desktop && c.tooling_profile == ToolingProfile::Electron {
            continue;
        }
        let service = service_name(path, c);
        let services = active_compose_services(&model, service)?;
        for name in &services {
            let config = &model["services"][name];
            ensure!(
                config.get("links").is_none()
                    && config.get("volumes_from").is_none()
                    && ["network_mode", "ipc", "pid"].iter().all(|key| config[*key]
                        .as_str()
                        .is_none_or(|value| !value.starts_with("service:"))),
                "Early reservation cannot prove the service dependency graph for {name}"
            );
        }
        let bindings = ports::declarations(&model, &services)?;
        plans.insert(
            path.clone(),
            reservation_fingerprint(&inventory, &services, &bindings)?,
        );
        selected.extend(services);
        if c.tooling_profile == ToolingProfile::Web && primary.is_none() {
            primary = Some(service);
        }
    }
    let bindings = ports::declarations(&model, &selected)?;
    if bindings.is_empty() {
        return Ok(false);
    }
    let branch = branch_name_until(&root, runner, Some(deadline))?;
    let renderer = preview::Renderer::from_checkout(&root, &branch, runner, deadline)?;
    leases.allocate_plan_until(
        ports::Allocation {
            declarations: &bindings,
            selected: &selected,
            stopped: &selected,
            primary,
            seed: &branch,
        },
        &BTreeSet::new(),
        &BTreeSet::new(),
        |candidate| {
            candidate.early_plans.clone_from(&plans);
            for service in &selected {
                candidate
                    .service_preview_renderer
                    .insert(service.clone(), renderer.clone());
            }
            let mut history = crate::stand::history::History::open(&root, runner, deadline)?;
            history.allow_start()?;
            history.bind_lease_cache(&root, &candidate.cache_root)?;
            Ok(())
        },
        Some(deadline),
        || {
            runner.check_cancelled()?;
            ctx.admission.recheck()?;
            assert_compose_inventory(&root, &inventory)?;
            for (path, stamp) in &stamps {
                ensure!(
                    stamp.matches(path)?,
                    "Compose input changed before reservation"
                );
            }
            Ok(())
        },
    )?;
    Ok(true)
}

pub fn remove_stand(root: &Path, runner: &ProcessRunner) -> Result<()> {
    use crate::stand::{epoch, history::History};
    use std::time::{Duration, Instant};

    let root = project_root(root)?;
    runtime_pending(&root)?;
    ensure_supported_checkout_floor(&root, false, MIN_SUPPORTED_PROJECT_VERSION)?;
    let request = RuntimeRequest::new(root.clone(), "stand remove".into());
    let _lock = CheckoutLock::acquire_exclusive(&root, "stand remove", Some(runner))?;
    let admission = ToolAdmission::capture_service(&request)?;
    crate::delivery::verify_current_engine(&admission.pins.host_engine()?)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let previous = History::read(&root, runner, deadline)?;
    ensure!(
        previous.is_some() || epoch::trusted(&root)?,
        "Stand ownership is unknown; recover or stop the legacy stand before removing this checkout"
    );
    let mut history = match previous {
        Some(history) => history,
        None => History::open(&root, runner, deadline)?,
    };
    if history.cleaned {
        return Ok(());
    }
    history.prove_lease_history(&root)?;
    history.retired = true;
    history.save(&root)?;
    if history.has_daemon() {
        use crate::stand::{history::ResourceDeclarations, inspection, ledger, ports::Ports};
        history.verify_daemon(runner, Instant::now() + Duration::from_secs(3))?;
        let ctx = PreparedContext::new(admission, runner, None)?;
        let inventory = capture_compose_inputs(&root)?;
        let model = normalize_stand(&ctx, Some(Instant::now() + Duration::from_secs(3)))?;
        let declarations = ResourceDeclarations::read(&root, &inventory, &model)?;
        let mut ports = Ports::read(&root)?;
        let ledger = ledger::Ledger::read(&root)?;
        ensure!(
            ports
                .as_ref()
                .is_none_or(|p| !p.possible_actors || ledger.is_some()),
            "Stand actor history is missing"
        );
        let rows = ledger::observe(&ctx.project, runner)?;
        if let Some(ports) = &ports {
            ledger::verify_owners(&rows, &ports.owner)?;
        } else {
            ensure!(
                rows.is_empty(),
                "Stand containers lack an ownership receipt"
            );
        }
        if let Some(mut ledger) = ledger {
            let ports = ports.as_mut().context("Missing stand lease ownership")?;
            ensure!(
                ledger.owner == ports.owner,
                "Stand actor history differs from lease ownership"
            );
            ctx.admission.recheck()?;
            assert_compose_inventory(&root, &inventory)?;
            ledger.recover_launch(&root, &ctx.project, runner)?;
            inspection::recover(&root, &mut ledger, ports, runner)?;
        }
        let rows = ledger::observe(&ctx.project, runner)?;
        ctx.admission.recheck()?;
        assert_compose_inventory(&root, &inventory)?;
        history.verify_daemon(runner, Instant::now() + Duration::from_secs(3))?;
        if let Some(ports) = &mut ports {
            ledger::verify_owners(&rows, &ports.owner)?;
            history.record_anonymous(&root, &rows, &declarations, runner)?;
            ledger::remove_owned(&rows, &ctx.project, &ports.owner, runner)?;
            ports.possible_actors = false;
            ports.save()?;
        } else {
            ensure!(
                rows.is_empty(),
                "Stand containers lack an ownership receipt"
            );
        }
        assert_compose_inventory(&root, &inventory)?;
        history.remove_resources(&root, &ctx.project, &declarations, runner, false)?;
        crate::stand::clear_overlay(&root)?;
        remove_file(&safe_path(&root, ledger::ACTIVE)?)?;
        if let Some(ports) = ports {
            ports.release()?;
        }
    } else {
        ensure!(
            cleanup_without_stand(&request, &root, None)?,
            "Could not prove the stand is empty; restore its ownership records before removing this checkout"
        );
    }
    history.cleaned = true;
    history.save(&root)
}

#[derive(Deserialize)]
struct CargoRunTarget {
    name: String,
    kind: Vec<String>,
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

#[derive(Deserialize)]
struct CargoPackageTargets {
    default_run: Option<String>,
    targets: Vec<CargoRunTarget>,
}

/// Cargo resolves explicit targets, default paths and edition-specific auto
/// discovery. Query it once at launch only when a managed group needs a choice.
fn workspace_binary_targets(
    ctx: &PreparedContext<'_>,
    image: &str,
) -> Result<BTreeMap<PathBuf, CargoPackageTargets>> {
    #[derive(Deserialize)]
    struct Metadata {
        packages: Vec<Package>,
    }
    #[derive(Deserialize)]
    struct Package {
        manifest_path: PathBuf,
        #[serde(flatten)]
        package: CargoPackageTargets,
    }

    let output = ctx.run_task(
        ContainerTask {
            component: None,
            profile: ToolingProfile::Rust,
            electron_cache: false,
            image,
            argv: &[
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--manifest-path",
                "/workspace/Cargo.toml",
                "--no-deps",
                "--locked",
                "--offline",
            ]
            .map(OsString::from),
            network: "none",
            forward_npm_secrets: false,
            access: TaskAccess::ExecReadOnly,
            inputs: None,
        },
        OutputMode::Capture(16 * 1024 * 1024),
    )?;
    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .context("Cannot read workspace targets from cargo metadata")?;
    Ok(metadata
        .packages
        .into_iter()
        .map(|mut pkg| {
            pkg.package
                .targets
                .retain(|t| t.kind.iter().any(|k| k == "bin"));
            (pkg.manifest_path, pkg.package)
        })
        .collect())
}

fn group_run_command(
    service: &str,
    members: &[CargoMember],
    targets: &BTreeMap<PathBuf, CargoPackageTargets>,
) -> Result<Vec<String>> {
    let mut runnable = Vec::new();
    for member in members {
        let manifest = Path::new("/workspace")
            .join(&member.rel_dir)
            .join("Cargo.toml");
        let package = targets
            .get(&manifest)
            .with_context(|| format!("cargo metadata did not return {}", manifest.display()))?;
        if !package.targets.is_empty() {
            runnable.push((member, package));
        }
    }
    if let [(member, package)] = runnable.as_slice()
        && (package.targets.len() == 1 || package.default_run.is_some())
    {
        return cargo_run_command(Some(member));
    }
    ensure!(
        !runnable.is_empty(),
        "No binary targets in {service}. Set services.{service}.command in compose.yaml or add a binary"
    );
    let mut choices = Vec::new();
    for (member, package) in runnable {
        for bin in &package.targets {
            let mut command = cargo_run_command(Some(member))?;
            if !bin.required_features.is_empty() {
                command.extend([
                    "--features".into(),
                    bin.required_features.join(",").replace('$', "$$"),
                ]);
            }
            command.push(format!("--bin={}", bin.name.replace('$', "$$")));
            choices.push(serde_json::to_string(&command)?);
        }
    }
    anyhow::bail!(
        "Cannot select a Cargo binary to run for {service}. Choose a command for services.{service}.command in compose.yaml.\n{}",
        choices.join("\n")
    );
}

fn run_development(
    request: &RuntimeRequest,
    target: &str,
    full: &RuntimeAdmission,
    node_inputs_digest: &str,
    ctx: &mut PreparedContext<'_>,
) -> Result<()> {
    use std::io::Write;
    use std::time::{Duration, Instant};
    ensure!(
        request.package.is_none() && full.state.components[target].role == Role::App,
        "Development requires an app selector"
    );
    let root = ctx.admission.root.clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    if let Some(history) = crate::stand::history::History::read(&root, ctx.runner, deadline)? {
        history.allow_start()?;
        history.prove_lease_history(&root)?;
        history.verify_daemon(ctx.runner, deadline)?;
    }
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
    let service = service_name(target, &full.state.components[target]).to_owned();
    let model = normalize_stand(ctx, None)?;
    let active_services = active_compose_services(&model, &service)?;
    let reached = development_components(&full.state, &model, &service)?;
    ensure!(
        !reached.is_empty(),
        "Selected Compose service has no registered application"
    );
    let state_dir = safe_path(&ctx.admission.root, ".devkit/state")?;
    let mut overlays = Vec::new();
    let mut uses_desktop = false;
    let mut rust_services = serde_json::Map::new();
    let mut workspace_targets = None;
    for path in reached {
        let c = &full.state.components[&path];
        if c.toolchain == Toolchain::Rust {
            let image = ctx.derived_image("rust", false)?;
            require_workspace(full, Toolchain::Rust, ctx.cache_dir.as_deref())?;
            let cargo_cache = ctx.cache_directory("cargo")?;
            assert_clean_cargo_home(&cargo_cache)?;
            ctx.dirs(["target".into()])?;
            if c.development
                .as_ref()
                .is_some_and(|d| d.owner == DevelopmentOwner::Managed)
            {
                let name = service_name(&path, c);
                let workspace = full
                    .native
                    .cargo_workspace
                    .as_ref()
                    .context("Missing Cargo workspace")?;
                let members = workspace.members(&path)?;
                if members.len() > 1
                    && model["services"][name].get("command")
                        == Some(&json!(cargo_run_command(None)?))
                {
                    let targets = match &workspace_targets {
                        Some(targets) => targets,
                        None => workspace_targets.insert(workspace_binary_targets(ctx, &image)?),
                    };
                    let command = group_run_command(name, members, targets)?;
                    rust_services.insert(name.to_owned(), json!({"command":command}));
                }
            }
        } else {
            ctx.node_image(false)?;
            ctx.pnpm(false)?;
            node_ready(ctx, node_inputs_digest)?;
        }
        if c.tooling_profile == ToolingProfile::Electron {
            uses_desktop = true;
            ctx.derived_image("desktop", false)?;
            let spec = crate::desktop::display_overlay(service_name(&path, c), request.headless)?;
            let mut file = tempfile::Builder::new()
                .prefix("display-")
                .suffix(".json")
                .tempfile_in(&state_dir)?;
            file.write_all(&json_bytes(&spec)?)?;
            overlays.push(file);
        }
    }
    if !rust_services.is_empty() {
        let mut file = tempfile::Builder::new()
            .prefix("cargo-run-")
            .suffix(".json")
            .tempfile_in(&state_dir)?;
        file.write_all(&json_bytes(&json!({"services":rust_services}))?)?;
        overlays.push(file);
    }
    ctx.dirs(workspace_write_directories(
        &full.state,
        &full.native.node_inputs,
    )?)?;
    let mut protected_services = serde_json::Map::new();
    let virtual_store = input_path(&ctx.admission.root, "node_modules/.pnpm")?;
    for (path, component) in &full.state.components {
        let Some(development) = &component.development else {
            continue;
        };
        let name = service_name(path, component);
        if development.owner != DevelopmentOwner::Managed || !active_services.contains(name) {
            continue;
        }
        let mut settings = json!({"platform":ctx.daemon()?.platform});
        if component.toolchain == Toolchain::Node {
            let mut volumes = Vec::new();
            if virtual_store.is_dir() {
                volumes.push(json!({"type":"bind","source":virtual_store,"target":"/workspace/node_modules/.pnpm","read_only":true,"bind":{"create_host_path":false,"selinux":"z"}}));
            }
            let targets = crate::shared_cache::npmrc_targets(&ctx.admission.root)?;
            if !targets.is_empty() {
                let cache = ctx.cache_root()?;
                crate::shared_cache::prepare_mount_masks(&cache)?;
                for relative in targets {
                    volumes.push(json!({"type":"bind","source":cache.join("empty-file-mask"),"target":Path::new("/workspace").join(relative),"read_only":true,"bind":{"create_host_path":false}}));
                }
            }
            if !volumes.is_empty() {
                settings["volumes"] = json!(volumes);
            }
        }
        protected_services.insert(name.to_owned(), settings);
    }
    if !protected_services.is_empty() {
        let mut file = tempfile::Builder::new()
            .prefix("managed-services-")
            .suffix(".json")
            .tempfile_in(&state_dir)?;
        file.write_all(&json_bytes(&json!({"services":protected_services}))?)?;
        overlays.push(file);
    }
    let legacy_npm_overlay = safe_path(&ctx.admission.root, ".devkit/state/compose.npm-env.json")?;
    if identity(&legacy_npm_overlay)?.is_some() {
        let _ = remove_file(&legacy_npm_overlay);
    }
    let npmrc_policy = crate::native::parse_manifest_npmrc_policy(
        &ctx.admission.root,
        crate::native::NodeLockKind::for_state(&ctx.admission.state)?,
    )?;
    if !npmrc_policy.required_env_vars.is_empty() {
        let mut services_map = serde_json::Map::new();
        for (path, c) in &full.state.components {
            if c.role != Role::App || c.toolchain != Toolchain::Node {
                continue;
            }
            let svc_name = service_name(path, c);
            let existing_env = model
                .get("services")
                .and_then(|s| s.get(svc_name))
                .and_then(|svc| svc.get("environment"));
            let mut env_map = serde_json::Map::new();
            for var in &npmrc_policy.required_env_vars {
                let already_defined = match existing_env {
                    Some(Value::Object(map)) => map.contains_key(var),
                    Some(Value::Array(arr)) => arr
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|entry| entry == var || entry.starts_with(&format!("{var}="))),
                    _ => false,
                };
                if !already_defined {
                    env_map.insert(var.clone(), json!("devkit-offline-unused"));
                }
            }
            if !env_map.is_empty() {
                services_map.insert(svc_name.to_owned(), json!({ "environment": env_map }));
            }
        }
        if !services_map.is_empty() {
            let mut npm_file = tempfile::Builder::new()
                .prefix("npm-env-")
                .suffix(".json")
                .tempfile_in(&state_dir)?;
            npm_file.write_all(&json_bytes(&json!({ "services": services_map }))?)?;
            overlays.push(npm_file);
        }
    }
    // SDK, display and store overlays participate in the namespace actually launched.
    let mut effective = ctx.compose()?;
    for file in &overlays {
        effective.arg("--file").arg(file.path());
    }
    let output = ctx.runner.capture(effective.args([
        "--profile",
        "*",
        "config",
        "--no-env-resolution",
        "--format",
        "json",
    ]))?;
    let mut model: Value = crate::stand::preview::compose_model(&output.stdout)?;
    let mut budget = ctx
        .budget
        .borrow()
        .clone()
        .context("Missing host resource budget; prerequisite not checked")?;
    let declarations = crate::stand::ports::declarations(&model, &active_services)?;
    let mut ports = crate::stand::ports::Ports::open(&ctx.admission.root, &ctx.cache_root()?)?;
    if let Some(early_plan) = ports.early_plans.remove(target)
        && early_plan != reservation_fingerprint(&inventory, &active_services, &declarations)?
    {
        eprintln!(
            "devkit: stand inputs changed since early reservation; revalidating port bindings"
        );
    }
    for name in &active_services {
        if let Some(label) = model
            .pointer("/services")
            .and_then(|v| v.get(name))
            .and_then(|v| v.get("labels"))
            .and_then(|v| v.get(crate::stand::ledger::OWNER_LABEL))
        {
            ensure!(
                label.as_str() == Some(ports.owner.key.as_str()),
                "Reserved stand owner label conflicts on {name}"
            );
        }
    }
    let primary = full
        .state
        .components
        .iter()
        .filter(|(_, c)| c.tooling_profile == ToolingProfile::Web)
        .map(|(p, c)| service_name(p, c))
        .find(|s| active_services.contains(*s));
    let branch = crate::stand::branch_name(&ctx.admission.root, ctx.runner)?;
    let frontends: BTreeSet<_> = full
        .state
        .components
        .iter()
        .filter(|(_, c)| {
            matches!(
                c.tooling_profile,
                ToolingProfile::Web | ToolingProfile::Electron
            )
        })
        .map(|(p, c)| service_name(p, c).to_owned())
        .filter(|name| active_services.contains(name))
        .collect();
    let carrier_frontends: BTreeSet<_> = frontends
        .iter()
        .filter(|name| {
            declarations
                .iter()
                .any(|b| b.service == **name && b.protocol == "tcp")
        })
        .cloned()
        .collect();
    preview_forwarding_presence(ctx, &mut model, &frontends)?;
    let renderer = crate::stand::preview::Renderer::from_checkout(
        &ctx.admission.root,
        &branch,
        ctx.runner,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    )?;
    // Make starts the selected applications individually. Use the same budget
    // group for every prerequisite of an aggregate invocation.
    let mut resource_services = active_services.clone();
    if request.dev_group.len() > 1 {
        for app in &request.dev_group {
            let path = format!("apps/{app}");
            let component = full
                .state
                .components
                .get(&path)
                .context("Unregistered development group application")?;
            resource_services.extend(active_compose_services(
                &model,
                service_name(&path, component),
            )?);
        }
    }
    if ports.possible_actors {
        ensure!(
            crate::stand::ledger::Ledger::read(&ctx.admission.root)?.is_some(),
            "Stand launch history is missing; preserving leases"
        );
    }
    let mut ledger = crate::stand::ledger::Ledger::open(&ctx.admission.root, &ports.owner)?;
    if !crate::stand::epoch::trusted(&root)? {
        let observed = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
        ensure!(
            observed.is_empty() && ledger.completed_empty() && !ports.possible_actors,
            "Stand history changed; run make down with the original Docker context first"
        );
        crate::stand::epoch::anchor(&root)?;
    }
    ledger.recover_launch(&ctx.admission.root, &ctx.project, ctx.runner)?;
    crate::stand::inspection::recover(&ctx.admission.root, &mut ledger, &mut ports, ctx.runner)?;
    let rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
    let active_ports = crate::stand::ledger::active_ports(&rows, &ports.owner)?;
    let mut history = crate::stand::history::History::open(
        &root,
        ctx.runner,
        Instant::now() + Duration::from_secs(3),
    )?;
    history.allow_start()?;
    history.bind_lease_cache(&root, &ports.cache_root)?;
    history.bind_daemon(&root, ctx.runner)?;
    let declarations_for_removal =
        crate::stand::history::ResourceDeclarations::read(&root, &inventory, &model)?;
    let owned_resources = history.prepare_resources(
        &root,
        &ctx.project,
        &model,
        &active_services,
        &declarations_for_removal,
        ctx.runner,
    )?;
    if !owned_resources.is_empty() {
        let mut file = tempfile::Builder::new()
            .prefix("compose-owned-")
            .suffix(".yaml")
            .tempfile_in(&state_dir)?;
        file.write_all(&owned_resources)?;
        overlays.push(file);
        let mut effective = ctx.compose()?;
        for file in &overlays {
            effective.arg("--file").arg(file.path());
        }
        let output = ctx.runner.capture(effective.args([
            "--profile",
            "*",
            "config",
            "--no-env-resolution",
            "--format",
            "json",
        ]))?;
        model = crate::stand::preview::compose_model(&output.stdout)?;
        ensure!(
            crate::stand::ports::declarations(&model, &active_services)? == declarations,
            "Resource mappings changed the admitted port declarations"
        );
        preview_forwarding_presence(ctx, &mut model, &frontends)?;
    }
    let resource_overlay = |budget: &crate::resources::HostResourceBudget| {
        crate::resources::build_compose_resource_overlay(
            budget,
            &full.state,
            &ctx.admission.pins.rust_version,
            &model,
            &resource_services,
        )
    };
    let resources = resource_overlay(&budget)?;
    let mut inspected_model = std::borrow::Cow::Borrowed(&model);
    if renderer.template.is_some()
        && !carrier_frontends.is_empty()
        && let Some(services) = resources
            .as_ref()
            .and_then(|resource| resource["services"].as_object())
    {
        for (name, settings) in services {
            if let Some(tmpfs) = settings.get("tmpfs") {
                inspected_model.to_mut()["services"][name]["tmpfs"] = tmpfs.clone();
            }
        }
    }
    let stopped = |rows: &[crate::stand::ledger::Container]| {
        active_services
            .iter()
            .filter(|name| !rows.iter().any(|row| &row.service == *name))
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    let proof = if renderer.template.is_some() && !carrier_frontends.is_empty() {
        let mount_inputs =
            crate::stand::inspection::mount_dependencies(&model, &carrier_frontends)?;
        ensure!(
            mount_inputs.is_subset(&active_services),
            "Inactive preview mount provider"
        );
        let mut references = BTreeMap::new();
        for name in crate::stand::inspection::image_dependencies(&model, &mount_inputs)? {
            let image = if let Some(image) = model["services"][&name]["image"].as_str() {
                image.to_owned()
            } else {
                ensure!(
                    model["services"][&name].get("build").is_some(),
                    "Service {name} has neither image nor build"
                );
                // Ask the same Compose implementation for its project/service
                // default. A single inert build declaration avoids dependency
                // images in config --images without guessing its naming rule.
                let spec = json_bytes(
                    &json!({"name":ctx.project,"services":{&name:{"build":{"context":"."}}}}),
                )?;
                let mut query = ctx.docker();
                query
                    .current_dir(&ctx.admission.root)
                    .args([
                        "compose",
                        "--project-name",
                        &ctx.project,
                        "--project-directory",
                    ])
                    .arg(&ctx.admission.root)
                    .args([
                        "--env-file",
                        "/dev/null",
                        "--file",
                        "-",
                        "config",
                        "--images",
                    ]);
                let output = ctx
                    .runner
                    .run_with_input(&mut query, OutputMode::Capture(65536), &spec)?
                    .success("Compose effective image query")?;
                let image = output.text()?.trim();
                ensure!(
                    !image.is_empty() && !image.contains('\n'),
                    "Ambiguous effective image for {name}"
                );
                image.into()
            };
            references.insert(name, image);
        }
        crate::stand::inspection::prepare(
            &mut ports,
            &inspected_model,
            &mount_inputs,
            &carrier_frontends,
            &references,
            &mut ledger,
            ctx.runner,
        )?
    } else {
        crate::stand::inspection::Proof {
            slots: BTreeMap::new(),
            images: BTreeMap::new(),
            external: BTreeMap::new(),
        }
    };
    let write_resources = |spec: Option<&Value>| -> Result<Option<tempfile::NamedTempFile>> {
        let Some(spec) = spec else {
            return Ok(None);
        };
        let mut file = tempfile::Builder::new()
            .prefix("compose-resources-")
            .suffix(".json")
            .tempfile_in(&state_dir)?;
        file.write_all(&json_bytes(spec)?)?;
        Ok(Some(file))
    };
    let launch_command =
        |resource_file: Option<&tempfile::NamedTempFile>, stand: &Path| -> Result<Command> {
            let mut command = ctx.compose()?;
            for overlay in &overlays {
                command.arg("--file").arg(overlay.path());
            }
            if let Some(resource_file) = resource_file {
                command.arg("--file").arg(resource_file.path());
            }
            command.arg("--file").arg(stand);
            Ok(command)
        };
    let validate_mounts = |ports: &crate::stand::ports::Ports,
                           resource_file: Option<&tempfile::NamedTempFile>,
                           stand: &Path|
     -> Result<Option<Vec<u8>>> {
        if proof.images.is_empty() {
            return Ok(None);
        }
        let mut command = launch_command(resource_file, stand)?;
        command.args([
            "--profile",
            "*",
            "config",
            "--no-env-resolution",
            "--format",
            "json",
        ]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let output = ctx.runner.capture_before(&mut command, deadline)?;
        ensure!(
            output.stdout.len() <= crate::processes::MAX_NATIVE_INPUT,
            "Normalized Compose launch exceeds native stdin byte budget"
        );
        crate::stand::inspection::verify_launch(
            &crate::stand::preview::compose_model(&output.stdout)?,
            &inspected_model,
            ports,
            &active_services,
            &proof,
        )?;
        // Compose already escaped this output for use as input. Keep the exact
        // verified model in memory so later commands cannot read amended files.
        Ok(Some(output.stdout))
    };
    let allocate = |ports: &mut crate::stand::ports::Ports,
                    rows: &[crate::stand::ledger::Container],
                    active_ports: &crate::stand::ports::ActivePorts,
                    conflicts: &BTreeSet<String>,
                    resource_file: Option<&tempfile::NamedTempFile>|
     -> Result<(PathBuf, Option<Vec<u8>>)> {
        let mut overlay = Vec::new();
        let mut snapshot = None;
        ports.allocate_plan(
            crate::stand::ports::Allocation {
                declarations: &declarations,
                selected: &active_services,
                stopped: &stopped(rows),
                primary,
                seed: &branch,
            },
            active_ports,
            conflicts,
            |candidate| {
                crate::stand::preview::publish_deployment(
                    candidate,
                    &frontends,
                    &active_services,
                    &renderer,
                    &proof,
                    &model,
                )?;
                overlay = crate::stand::deployment_overlay(
                    candidate,
                    &active_services,
                    &frontends,
                    &model,
                    &proof,
                )?;
                if !proof.images.is_empty() {
                    let mut file = tempfile::Builder::new()
                        .prefix("compose.stand.")
                        .suffix(".yaml")
                        .tempfile_in(&state_dir)?;
                    file.write_all(&overlay)?;
                    snapshot = validate_mounts(candidate, resource_file, file.path())?;
                    assert_compose_inventory(&ctx.admission.root, &inventory)?;
                }
                Ok(())
            },
        )?;
        Ok((
            crate::stand::save_overlay(&ctx.admission.root, &overlay)?,
            snapshot,
        ))
    };
    let result = (|| -> Result<()> {
        let mut resource_file = write_resources(resources.as_ref())?;
        let (mut stand_overlay, mut snapshot) = allocate(
            &mut ports,
            &rows,
            &active_ports,
            &BTreeSet::new(),
            resource_file.as_ref(),
        )?;
        let mut bind_attempts = 1;
        let mut cgroup_retried = false;
        loop {
            let compose = || -> Result<Command> {
                if snapshot.is_some() {
                    let mut command = ctx.compose_project();
                    command.args(["--file", "-"]);
                    Ok(command)
                } else {
                    launch_command(resource_file.as_ref(), &stand_overlay)
                }
            };
            full.recheck()?;
            if uses_desktop {
                verify_managed_seccomp(&ctx.admission.root, &ctx.admission.state)?;
            }
            let before = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
            assert_compose_inventory(&ctx.admission.root, &inventory)?;
            let mut create = compose()?;
            // Compose up includes build-only dependencies in image acquisition.
            create.args(["up", "--no-start", &service]);
            ledger = crate::stand::ledger::Ledger::intent(
                &ctx.admission.root,
                &ports.owner,
                &active_services,
            )?;
            if !ports.possible_actors {
                ports.possible_actors = true;
                ports.save()?;
            }
            crate::stand::epoch::anchor(&ctx.admission.root)?;
            ledger.begin_launch(&ctx.admission.root, &model, &active_services, &before)?;
            let mut reaped = false;
            let mode = OutputMode::TeeStderr(64 * 1024);
            let creation =
                ctx.runner
                    .run_tracked(&mut create, mode, snapshot.as_deref(), &mut reaped);
            ledger.launch_finished = reaped;
            ledger.launch.as_mut().unwrap().daemon_finished =
                matches!(creation.as_ref(), Ok(output) if output.code == 0) && reaped;
            ledger.save(&ctx.admission.root)?;
            let mut up_output = creation?;
            if up_output.code == 0 {
                let mut start = compose()?;
                start.args(["start", "--wait", &service]);
                up_output = if let Some(input) = &snapshot {
                    ctx.runner.run_with_input(&mut start, mode, input)?
                } else {
                    ctx.runner.run(&mut start, mode)?
                };
            }
            if up_output.code == 0 {
                break;
            }
            ledger.recover_launch(&ctx.admission.root, &ctx.project, ctx.runner)?;
            if !cgroup_retried
                && crate::resources::is_compose_cgroup_delegation_failure(
                    up_output.code,
                    &up_output.stderr,
                )
            {
                eprintln!(
                    "devkit: warning: Docker daemon lacks cgroup resource delegation; retrying Compose without container cgroup limits"
                );
                budget.disable_cgroup_limits();
                resource_file = write_resources(resource_overlay(&budget)?.as_ref())?;
                snapshot = validate_mounts(&ports, resource_file.as_ref(), &stand_overlay)?;
                cgroup_retried = true;
                *ctx.budget.borrow_mut() = Some(budget.clone());
                continue;
            }
            let conflicts = crate::stand::ports::bind_conflicts(&up_output.stderr, &ports.groups)?;
            if bind_attempts < 3 && !conflicts.is_empty() {
                let rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
                let active_ports = crate::stand::ledger::active_ports(&rows, &ports.owner)?;
                (stand_overlay, snapshot) = allocate(
                    &mut ports,
                    &rows,
                    &active_ports,
                    &conflicts,
                    resource_file.as_ref(),
                )?;
                bind_attempts += 1;
                continue;
            }
            up_output.stderr.clear();
            up_output.success("docker")?;
        }
        let rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
        ledger.observed(&ctx.admission.root, &rows)?;
        ports.reconcile(&rows)?;
        for service in &active_services {
            if frontends.contains(service) {
                ports.service_preview_renderer.remove(service);
            } else if rows.iter().any(|row| {
                &row.service == service
                    && ports.service_desired.get(service) == Some(&row.deployment)
            }) {
                // A backend's rendered URL does not change its container configuration.
                ports
                    .service_preview_renderer
                    .insert(service.clone(), renderer.clone());
            }
        }
        crate::stand::preview::prune(&mut ports, &rows)?;
        let service_to_component = app_service_component_map(&full.state);
        let mut ps_cmd = ctx.compose()?;
        ps_cmd.args(["--profile", "*", "ps", "--format", "json"]);
        let ps_out = ctx.runner.capture(&mut ps_cmd)?;
        let mut entries =
            crate::doctor::parse_compose_ps_json(&ps_out.stdout, &service_to_component)?;
        crate::stand::preview::enrich_observed(&ports, &rows, &renderer, &mut entries)?;
        for entry in entries {
            if !active_services.contains(&entry.service) || entry.state != "running" {
                continue;
            }
            for url in &entry.preview.preview_urls {
                println!("Preview {}: {url}", entry.service);
            }
            for url in &entry.urls {
                if let Some(comp) = &entry.component {
                    println!("Service {} ({comp}): {url}", entry.service);
                } else {
                    println!("Service {}: {url}", entry.service);
                }
            }
        }
        assert_compose_inventory(&ctx.admission.root, &inventory)?;
        Ok(())
    })();
    // Neither OS reap nor an empty observation proves accepted daemon writes
    // have completed. Keep images and files until the whole creator outcome is known.
    let cleanup = ledger
        .recover_launch(&ctx.admission.root, &ctx.project, ctx.runner)
        .and_then(|()| {
            crate::stand::inspection::recover(
                &ctx.admission.root,
                &mut ledger,
                &mut ports,
                ctx.runner,
            )
        });
    if result.is_ok() {
        cleanup?;
        history.record_resources(&root, &declarations_for_removal, ctx.runner)?;
        let rows = crate::stand::ledger::observe(&ctx.project, ctx.runner)?;
        crate::stand::ledger::verify_owners(&rows, &ports.owner)?;
        history.record_anonymous(&root, &rows, &declarations_for_removal, ctx.runner)?;
    } else if let Err(error) = cleanup {
        eprintln!("devkit: stand creator recovery retained: {error:#}");
    }
    result
}

fn app_service_component_map(state: &ProjectState) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (path, c) in &state.components {
        if c.role == Role::App {
            map.insert(service_name(path, c).to_owned(), path.clone());
        }
    }
    map
}

pub fn enrich_describe(full: &RuntimeAdmission, cache_dir: Option<&Path>) -> Result<Value> {
    let mut components = serde_json::Map::new();
    for (path, c) in &full.state.components {
        let mut obj = match serde_json::to_value(c)? {
            Value::Object(m) => m,
            _ => anyhow::bail!("Component state must serialize to an object"),
        };
        let short = path.rsplit('/').next().unwrap_or(path.as_str());
        let selector = if c.role == Role::App {
            format!("app={short}")
        } else {
            format!("package={short}")
        };
        let packageable = c.tooling_profile == ToolingProfile::Electron
            || component_is_public_library(&full.root, path, c)?;
        let exposed_port = if c.role == Role::App
            && c.development
                .as_ref()
                .is_some_and(|d| d.owner == DevelopmentOwner::Managed)
            && c.tooling_profile != ToolingProfile::Electron
        {
            Some(if c.tooling_profile == ToolingProfile::Web {
                5173u16
            } else {
                8080u16
            })
        } else {
            None
        };
        obj.insert("selector".into(), json!(selector));
        obj.insert("packageable".into(), json!(packageable));
        obj.insert("exposedContainerPort".into(), json!(exposed_port));
        components.insert(path.clone(), Value::Object(obj));
    }
    let receipts = crate::doctor::local_receipts_with_pins(
        &full.root,
        &full.state,
        &full.pins,
        Some(&full.native),
        None,
        cache_dir,
    )?;
    Ok(json!({
        "root": full.root,
        "version": full.state.version,
        "releaseIdentity": full.state.release_identity,
        "generation": full.state.generation,
        "composeProject": crate::doctor::compose_project_name(&full.root),
        "components": components,
        "inputs": full.native.inputs,
        "receipts": receipts,
    }))
}

pub fn inspect_status(
    root: &Path,
    cache_dir: Option<&Path>,
    runner: &ProcessRunner,
) -> Result<(Value, String)> {
    let root = project_root(root)?;
    ensure_supported_checkout_floor(&root, false, MIN_SUPPORTED_PROJECT_VERSION)?;
    let lock = CheckoutLock::acquire_shared(&root, "status", Some(runner))?;
    let req = RuntimeRequest::new(root.clone(), "status".into());
    let tool = ToolAdmission::capture_status_compat(&req)?;
    runtime_paths(&tool)?;
    let mut ctx = PreparedContext::new_for_status(tool, runner, cache_dir)?;
    ctx.prerequisite_service()?;
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
    let service_to_component = app_service_component_map(&ctx.admission.state);
    ctx.load_recorded_images(true)?;
    let mut command = ctx.compose()?;
    ctx.env(&mut command);
    command.args(["--profile", "*", "ps", "--all", "--format", "json"]);
    let out = ctx.runner.capture(&mut command)?;
    let mut services = crate::doctor::parse_compose_ps_json(&out.stdout, &service_to_component)?;
    let stand_ports = crate::stand::preview::enrich_status(
        &ctx.admission.root,
        &ctx.project,
        runner,
        None,
        &mut services,
    )?;
    assert_compose_inventory(&ctx.admission.root, &inventory)?;
    let receipts = crate::doctor::local_receipts_with_pins(
        &ctx.admission.root,
        &ctx.admission.state,
        &ctx.admission.pins,
        None,
        Some(ctx.daemon()?.platform),
        ctx.cache_dir.as_deref(),
    )?;
    ctx.admission.recheck()?;
    runner.check_cancelled()?;
    drop(lock);

    let mut lines = vec![format!("Compose project: {}", ctx.project)];
    if services.is_empty() {
        lines.push("Services: none".into());
    } else {
        for svc in &services {
            let comp_suffix = svc
                .component
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            let health_suffix = svc
                .health
                .as_deref()
                .map(|h| format!(" [{h}]"))
                .unwrap_or_default();
            let url_suffix = if svc.urls.is_empty() {
                String::new()
            } else {
                format!(
                    " -> {}",
                    svc.urls
                        .iter()
                        .chain(&svc.preview.preview_urls)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            lines.push(format!(
                "Service {}{}: {}{}{}",
                svc.service, comp_suffix, svc.state, health_suffix, url_suffix
            ));
        }
    }
    lines.extend(crate::stand::preview::reservation_lines(
        &stand_ports,
        &services,
    ));
    let result = json!({
        "root": ctx.admission.root,
        "composeProject": ctx.project,
        "services": services,
        "standPorts": stand_ports,
        "receipts": receipts,
    });
    Ok((result, lines.join("\n")))
}
/// Lifecycle service inventory uses the same cancellable native process facade.
pub struct DockerComposeNormalizer {
    pub runner: ProcessRunner,
}
impl ComposeNormalizer for DockerComposeNormalizer {
    fn normalize(&self, snapshot: &Path, entrypoints: &[String]) -> Result<Value> {
        require_compose(&self.runner, &mut docker_command())?;
        let mut command = docker_command();
        command
            .env("DEVKIT_PNPM_PATH", "/devkit/unused-pnpm")
            .env("DEVKIT_CARGO_CACHE_DIR", "/devkit/unused-cargo")
            .env("DEVKIT_ELECTRON_CACHE_DIR", "/devkit/unused-electron");
        command
            .args(["compose", "--project-directory"])
            .arg(snapshot)
            .args([
                "--project-name",
                "devkit-inventory",
                "--env-file",
                "/dev/null",
            ]);
        // This query resolves service names, not tool images. Supply inert
        // integration values and never acquire an image during normalization.
        command
            .env("DEVKIT_PROJECT_ROOT", snapshot)
            .env("DEVKIT_PROJECT_NAME", "devkit-inventory")
            .env("DEVKIT_UID", "1000")
            .env("DEVKIT_GID", "1000")
            .env("DEVKIT_NODE_IMAGE", "devkit-inventory-node:local")
            .env("DEVKIT_ENGINE_PATH", "/devkit/unused-engine");
        for p in entrypoints {
            command.arg("--file").arg(snapshot.join(p));
        }
        command.args([
            "--profile",
            "*",
            "config",
            "--no-env-resolution",
            "--format",
            "json",
        ]);
        let output = self.runner.capture(&mut command)?;
        crate::stand::preview::compose_model(&output.stdout)
    }
}

fn parse_cargo_jobs_env(var_name: &str, raw: &std::ffi::OsStr) -> Result<usize> {
    let text = raw
        .to_str()
        .with_context(|| format!("Invalid {var_name}: expected UTF-8 integer in 1..=256"))?;
    let jobs: usize = text.trim().parse().with_context(|| {
        format!("Invalid {var_name} value '{text}'; expected an integer in 1..=256")
    })?;
    ensure!(
        (1..=256).contains(&jobs),
        "Invalid {var_name} value '{text}'; expected an integer in 1..=256"
    );
    Ok(jobs)
}

fn resolve_cargo_build_jobs_from(
    devkit_jobs: Option<&std::ffi::OsStr>,
    cargo_jobs: Option<&std::ffi::OsStr>,
    ci: Option<&std::ffi::OsStr>,
    available_cpus: Option<usize>,
) -> Result<usize> {
    if let Some(raw) = devkit_jobs {
        return parse_cargo_jobs_env("DEVKIT_CARGO_BUILD_JOBS", raw);
    }
    if let Some(raw) = cargo_jobs {
        return parse_cargo_jobs_env("CARGO_BUILD_JOBS", raw);
    }
    let cpus = available_cpus.unwrap_or(2).max(1);
    let is_ci = ci
        .and_then(|v| v.to_str())
        .is_some_and(|v| matches!(v.trim(), "true" | "1"));
    Ok(if is_ci { cpus.min(4) } else { cpus.min(8) })
}

pub fn resolve_cargo_build_jobs() -> Result<usize> {
    let devkit_jobs = std::env::var_os("DEVKIT_CARGO_BUILD_JOBS");
    let cargo_jobs = std::env::var_os("CARGO_BUILD_JOBS");
    let ci = std::env::var_os("CI");
    let available = std::thread::available_parallelism().ok().map(usize::from);
    resolve_cargo_build_jobs_from(
        devkit_jobs.as_deref(),
        cargo_jobs.as_deref(),
        ci.as_deref(),
        available,
    )
}

#[cfg(test)]
mod tests {
    use super::resolve_cargo_build_jobs_from;
    use std::ffi::OsStr;

    #[test]
    fn group_launch_uses_cargo_targets_and_emits_composable_override_commands() {
        use super::{cargo_run_command, group_run_command};
        use crate::native::{CargoMember, CargoPackageId};
        use std::{collections::BTreeMap, path::PathBuf};

        let members = ["service", "model"].map(|name| CargoMember {
            rel_dir: format!("apps/api/branch$name/{name}"),
            id: CargoPackageId {
                name: name.into(),
                version: "1.0.0".into(),
                source: None,
            },
            dependencies: Vec::new(),
        });
        let [service, model] = members.each_ref().map(|m| {
            PathBuf::from("/workspace")
                .join(&m.rel_dir)
                .join("Cargo.toml")
        });
        let targets_for = |default_run: Option<&str>, bins: serde_json::Value| {
            serde_json::from_value(
                serde_json::json!({ "default_run": default_run, "targets": bins }),
            )
            .unwrap()
        };
        let server = serde_json::json!({"name":"api","kind":["bin"]});
        let job = serde_json::json!({"name":"maintenance","kind":["bin"],"required-features":["admin","util/jobs"]});
        let mut targets = BTreeMap::from([
            (service.clone(), targets_for(None, serde_json::json!([job]))),
            (model.clone(), targets_for(None, serde_json::json!([]))),
        ]);
        let default = cargo_run_command(Some(&members[0])).unwrap();
        // Cargo handles a single target or an explicit default-run, including its default features.
        assert_eq!(
            group_run_command("api", &members, &targets).unwrap(),
            default
        );
        targets.insert(
            service.clone(),
            targets_for(Some("maintenance"), serde_json::json!([server, job])),
        );
        assert_eq!(
            group_run_command("api", &members, &targets).unwrap(),
            default
        );

        for (service_bins, model_bins, job_member) in [
            (
                serde_json::json!([server, job]),
                serde_json::json!([]),
                &members[0],
            ),
            (
                serde_json::json!([server]),
                serde_json::json!([job]),
                &members[1],
            ),
        ] {
            targets.insert(service.clone(), targets_for(None, service_bins));
            targets.insert(model.clone(), targets_for(None, model_bins));
            let error = group_run_command("api", &members, &targets)
                .unwrap_err()
                .to_string();
            let choices: Vec<Vec<String>> = error
                .lines()
                .skip(1)
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let mut expected_server = default.clone();
            expected_server.push("--bin=api".to_owned());
            let mut expected_job = cargo_run_command(Some(job_member)).unwrap();
            expected_job
                .extend(["--features", "admin,util/jobs", "--bin=maintenance"].map(str::to_owned));
            assert!(expected_job.iter().any(|arg| arg.contains("branch$$name")));
            assert_eq!(choices, [expected_server, expected_job]);
        }
    }

    #[test]
    fn cargo_build_jobs_prioritizes_overrides_caps_defaults_and_rejects_invalid_bounds() {
        assert_eq!(
            resolve_cargo_build_jobs_from(
                Some(OsStr::new("12")),
                Some(OsStr::new("3")),
                Some(OsStr::new("true")),
                Some(16),
            )
            .unwrap(),
            12
        );
        assert_eq!(
            resolve_cargo_build_jobs_from(
                None,
                Some(OsStr::new("6")),
                Some(OsStr::new("1")),
                Some(16),
            )
            .unwrap(),
            6
        );
        assert_eq!(
            resolve_cargo_build_jobs_from(None, None, Some(OsStr::new("true")), Some(16)).unwrap(),
            4
        );
        assert_eq!(
            resolve_cargo_build_jobs_from(None, None, Some(OsStr::new("1")), Some(16)).unwrap(),
            4
        );
        assert_eq!(
            resolve_cargo_build_jobs_from(None, None, None, Some(16)).unwrap(),
            8
        );
        assert_eq!(
            resolve_cargo_build_jobs_from(None, None, None, None).unwrap(),
            2
        );
        for bad in ["", "0", "257", "abc", "-1"] {
            assert!(
                resolve_cargo_build_jobs_from(Some(OsStr::new(bad)), None, None, Some(4)).is_err()
            );
            assert!(
                resolve_cargo_build_jobs_from(None, Some(OsStr::new(bad)), None, Some(4)).is_err()
            );
        }
    }
}

#[cfg(test)]
mod electron_cache_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn generated_metadata_refuses_fifos_symlinks_and_oversized_files_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("path.txt");
        assert!(read_electron_metadata(&path, 256).unwrap().is_none());
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a live, NUL-terminated temporary checkout path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_electron_metadata(&path, 256).is_err());
        fs::remove_file(&path).unwrap();
        let target = temporary.path().join("target");
        fs::write(&target, b"electron").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_electron_metadata(&path, 256).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, vec![b'x'; 257]).unwrap();
        assert!(read_electron_metadata(&path, 256).is_err());
        fs::write(&path, b"electron").unwrap();
        assert_eq!(
            read_electron_metadata(&path, 256).unwrap().unwrap(),
            b"electron"
        );
    }

    #[test]
    fn inventory_detects_missing_changed_and_unexpected_distribution_entries() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::TempDir::new().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("locales")).unwrap();
        let mut elf = [0_u8; 64];
        elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        elf[18..20].copy_from_slice(&183u16.to_le_bytes());
        fs::write(source.join("electron"), elf).unwrap();
        fs::set_permissions(source.join("electron"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(source.join("icudtl.dat"), b"trusted data").unwrap();
        fs::write(source.join("locales/en.pak"), b"locale").unwrap();
        let runner = ProcessRunner::new().unwrap();
        let expected = distribution_inventory(&source, &runner).unwrap();
        ensure_electron_executable(&source.join("electron"), &expected, 183).unwrap();
        assert!(ensure_electron_executable(&source.join("electron"), &expected, 62).is_err());
        let marker = temporary.path().join(".complete");
        write_json(
            &marker,
            &ElectronDistributionReceipt {
                schema_version: 1,
                version: "1.0.0".into(),
                entries: expected,
            },
        )
        .unwrap();
        let receipt = read_distribution_receipt(&marker).unwrap();
        let target = temporary.path().join("target");
        hardlink_distribution(&source, &target, &runner).unwrap();
        assert_eq!(
            distribution_inventory(&target, &runner).unwrap(),
            receipt.entries
        );
        fs::remove_file(target.join("icudtl.dat")).unwrap();
        assert_ne!(
            distribution_inventory(&target, &runner).unwrap(),
            receipt.entries
        );
        fs::write(source.join("icudtl.dat"), b"changed data").unwrap();
        assert_ne!(
            distribution_inventory(&source, &runner).unwrap(),
            receipt.entries
        );
        fs::write(source.join("icudtl.dat"), b"trusted data").unwrap();
        assert_eq!(
            distribution_inventory(&source, &runner).unwrap(),
            receipt.entries
        );
        fs::write(source.join("unexpected"), b"extra").unwrap();
        assert_ne!(
            distribution_inventory(&source, &runner).unwrap(),
            receipt.entries
        );
        fs::remove_file(source.join("unexpected")).unwrap();
        symlink("../outside", source.join("redirected")).unwrap();
        assert!(distribution_inventory(&source, &runner).is_err());
    }

    #[test]
    fn distribution_links_share_inodes_and_refuse_redirected_cache_ancestors() {
        let temporary = tempfile::TempDir::new().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("electron"), b"distribution").unwrap();
        symlink("electron", source.join("relative-link")).unwrap();
        let runner = ProcessRunner::new().unwrap();
        let target = temporary.path().join("target");
        hardlink_distribution(&source, &target, &runner).unwrap();
        assert_eq!(
            fs::metadata(source.join("electron")).unwrap().ino(),
            fs::metadata(target.join("electron")).unwrap().ino()
        );
        assert_eq!(
            fs::read_link(target.join("relative-link")).unwrap(),
            Path::new("electron")
        );
        symlink(&source, temporary.path().join("redirected")).unwrap();
        let refused = temporary.path().join("refused");
        assert!(
            hardlink_distribution(&temporary.path().join("redirected"), &refused, &runner).is_err()
        );
        assert!(!refused.exists());
        assert_eq!(fs::read(source.join("electron")).unwrap(), b"distribution");
    }
}
