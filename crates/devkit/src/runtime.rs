//! Checkout admission and container-first daily operations. Make owns the DAG.
use crate::{
    Result, catalog::*, compose::*, files::*, model::*, processes::*, projections::*, state::*,
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const HELP: &str = "make setup|check|fix|test|build [app=<name> | package=<name>]\nmake dev|up [app=<name>] [headless=1]\nmake package [app=<desktop> | package=<name>]\nmake status|logs [app=<name>] [tail=100] [follow=1]\nmake down  (preserves data)\nmake doctor\ndevkit status [TARGET] [--json]\ndevkit doctor [TARGET] [--json]\ndevkit describe [TARGET] [--json]\ndevkit exec --component apps/web|packages/lib [--write] [--network none|bridge] [--prepare] -- ARGV\n";
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
    pub headless: bool,
    pub package_aggregate: bool,
    pub follow: bool,
    pub tail: u64,
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
            headless: false,
            package_aggregate: false,
            follow: false,
            tail: 100,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub runtime: RuntimeRequest,
    pub argv: Vec<OsString>,
    pub write: bool,
    pub network: String,
    pub prepare: bool,
}
#[derive(Clone, Debug)]
pub struct ToolPins {
    pub version: String,
    pub node_image: String,
    pub node_version: String,
    pub rust_version: String,
    pub rust_recipe: String,
    pub desktop_recipe: String,
    pub engine: Option<crate::delivery::EnginePin>,
}
impl ToolPins {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut values = BTreeMap::new();
        for line in std::str::from_utf8(bytes)?.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .context("Invalid toolchains.env record")?;
            ensure!(
                [
                    "DEVKIT_VERSION",
                    "DEVKIT_NODE_IMAGE",
                    "DEVKIT_NODE_VERSION",
                    "DEVKIT_RUST_VERSION",
                    "DEVKIT_RUST_RECIPE_HASH",
                    "DEVKIT_DESKTOP_RECIPE_HASH",
                    "DEVKIT_ENGINE_SHA256",
                    "DEVKIT_ENGINE_TARGET"
                ]
                .contains(&k)
                    && !v.is_empty()
                    && !v
                        .chars()
                        .any(|c| c.is_whitespace() || ['\'', '"', '\0'].contains(&c))
                    && values.insert(k, v).is_none(),
                "Invalid/duplicate toolchain key: {k}"
            );
        }
        let get = |k| -> Result<String> {
            Ok(values.get(k).context("Missing toolchain pin")?.to_string())
        };
        let release_version = get("DEVKIT_VERSION")?;
        let engine = if crate::cache::legacy(&release_version) {
            ensure!(
                !values.contains_key("DEVKIT_ENGINE_SHA256")
                    && !values.contains_key("DEVKIT_ENGINE_TARGET"),
                "Legacy tool pins cannot select a native engine"
            );
            None
        } else {
            let engine = crate::delivery::EnginePin {
                version: release_version.clone(),
                target: get("DEVKIT_ENGINE_TARGET")?,
                sha256: get("DEVKIT_ENGINE_SHA256")?,
            };
            engine.validate()?;
            Some(engine)
        };
        let s = Self {
            version: release_version,
            node_image: get("DEVKIT_NODE_IMAGE")?,
            node_version: get("DEVKIT_NODE_VERSION")?,
            rust_version: get("DEVKIT_RUST_VERSION")?,
            rust_recipe: get("DEVKIT_RUST_RECIPE_HASH")?,
            desktop_recipe: get("DEVKIT_DESKTOP_RECIPE_HASH")?,
            engine,
        };
        version(&s.version)?;
        version(&s.node_version)?;
        version(&s.rust_version)?;
        let (name, hash) = s
            .node_image
            .rsplit_once("@sha256:")
            .context("Node image must be digest pinned")?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"./:-".contains(&c))
                && SHA256_RE.is_match(hash)
                && SHA256_RE.is_match(&s.rust_recipe)
                && SHA256_RE.is_match(&s.desktop_recipe),
            "Invalid immutable tool pins"
        );
        Ok(s)
    }
}
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
        let manifest_identity =
            identity(&safe_path(&root, MANIFEST_PATH)?)?.context("Missing project manifest")?;
        if let Some(expected) = &request.expected_manifest {
            ensure!(
                SHA256_RE.is_match(expected) && expected == &manifest_identity.sha256,
                "accepted toolkit changed during this Make invocation; rerun the command"
            );
        }
        let state = read_project_state(&root)?;
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
        let pins = ToolPins::parse(&fs::read(safe_path(
            &root,
            ".devkit/runtime/toolchains.env",
        )?)?)?;
        ensure!(
            pins.version == state.version,
            "Manifest version differs from accepted toolchains.env"
        );
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
    Finite,
    PackageLibrary,
    Fix,
    ExecReadOnly,
    ExecWrite,
}
struct ContainerTask<'a> {
    component: Option<&'a str>,
    profile: ToolingProfile,
    image: &'a str,
    argv: &'a [OsString],
    network: &'a str,
    forward_npm_secrets: bool,
    access: TaskAccess,
    inputs: Option<&'a InputIdentitySet>,
}
struct PreparedContext<'a> {
    admission: ToolAdmission,
    runner: &'a ProcessRunner,
    project: String,
    engine: PathBuf,
    rust_image: Option<String>,
    desktop_image: Option<String>,
    node_image_verified: std::cell::Cell<bool>,
    node_ready_verified: std::cell::Cell<bool>,
    budget: std::cell::RefCell<Option<crate::resources::HostResourceBudget>>,
}
impl<'a> PreparedContext<'a> {
    fn new(admission: ToolAdmission, runner: &'a ProcessRunner) -> Result<Self> {
        Self::new_inner(admission, runner, true)
    }
    fn new_for_status(admission: ToolAdmission, runner: &'a ProcessRunner) -> Result<Self> {
        Self::new_inner(admission, runner, false)
    }
    fn new_inner(
        admission: ToolAdmission,
        runner: &'a ProcessRunner,
        verify_pin: bool,
    ) -> Result<Self> {
        let engine = fs::canonicalize(std::env::current_exe()?)?;
        ensure!(
            identity(&engine)?.is_some(),
            "Current native engine is not a regular executable"
        );
        if verify_pin && let Some(pin) = &admission.pins.engine {
            crate::delivery::verify_engine(&engine, pin)?;
        }
        let project = crate::doctor::compose_project_name(&admission.root);
        Ok(Self {
            admission,
            runner,
            project,
            engine,
            rust_image: None,
            desktop_image: None,
            node_image_verified: std::cell::Cell::new(false),
            node_ready_verified: std::cell::Cell::new(false),
            budget: std::cell::RefCell::new(None),
        })
    }
    fn docker(&self) -> Command {
        let mut c = Command::new("docker");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("COMPOSE_") {
                c.env_remove(key);
            }
        }
        c
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
            ("DEVKIT_ENGINE_PATH", self.engine.as_os_str().to_owned()),
        ] {
            c.env(k, v);
        }
    }
    fn compose(&self) -> Result<Command> {
        let mut c = self.docker();
        c.args(["compose", "--project-directory"])
            .arg(&self.admission.root)
            .args([
                "--project-name",
                &self.project,
                "--env-file",
                "/dev/null",
                "--file",
            ])
            .arg(safe_path(
                &self.admission.root,
                ".devkit/compose.apps.yaml",
            )?);
        if identity(&safe_path(&self.admission.root, "compose.yaml")?)?.is_some() {
            c.arg("--file")
                .arg(self.admission.root.join("compose.yaml"));
        }
        self.env(&mut c);
        Ok(c)
    }
    fn prerequisite_service(&self) -> Result<()> {
        require_compose(self.runner, &mut self.docker())?;
        let info = self
            .runner
            .capture(self.docker().args(["info", "--format", "{{.OSType}}"]))?;
        ensure!(
            info.text()?.trim() == "linux",
            "A Linux Docker daemon is required"
        );
        Ok(())
    }
    fn prerequisite(&self) -> Result<()> {
        require_compose(self.runner, &mut self.docker())?;
        let budget = crate::resources::HostResourceBudget::probe_with_docker(
            self.runner,
            &mut self.docker(),
        )?;
        *self.budget.borrow_mut() = Some(budget);
        Ok(())
    }
    fn image(&self, image: &str) -> Result<bool> {
        let result = self.runner.run(
            self.docker().args(["image", "inspect", image]),
            OutputMode::Capture(1024 * 1024),
        )?;
        if result.code == 0 {
            return Ok(true);
        }
        let info = self
            .runner
            .capture(self.docker().args(["info", "--format", "{{.OSType}}"]))?;
        ensure!(
            info.text()?.trim() == "linux",
            "Cannot verify accepted image; Linux Docker daemon is required"
        );
        Ok(false)
    }
    fn node_image(&self, prepare: bool) -> Result<()> {
        if !prepare && self.node_image_verified.get() {
            return Ok(());
        }
        if !self.image(&self.admission.pins.node_image)? {
            ensure!(
                prepare,
                "accepted Node image is absent; run make setup (or exec --prepare)"
            );
            self.runner.checked(
                self.docker()
                    .args(["pull", &self.admission.pins.node_image]),
                OutputMode::Inherit,
            )?;
        }
        self.node_image_verified.set(true);
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
                &self.admission.pins.rust_recipe,
                "io.docouno.devkit.rust-recipe",
            )
        } else {
            (
                "desktop.Dockerfile",
                &self.admission.pins.desktop_recipe,
                "io.docouno.devkit.desktop-recipe",
            )
        };
        let recipe_path = safe_path(&self.admission.root, &format!(".devkit/runtime/{recipe}"))?;
        ensure!(
            identity(&recipe_path)?.is_some_and(|id| &id.sha256 == pin),
            "{recipe} differs from the accepted recipe"
        );
        let state = safe_path(&self.admission.root, &format!(".devkit/state/{kind}-image"))?;
        if let Some(id) = identity(&state)? {
            let recorded = fs::read_to_string(&state)?;
            let recorded = recorded.trim();
            if recorded
                .strip_prefix("sha256:")
                .is_some_and(|h| SHA256_RE.is_match(h))
            {
                let inspected = self.runner.run(
                    self.docker().args([
                        "image",
                        "inspect",
                        "--format",
                        &format!("{{{{index .Config.Labels \"{label}\"}}}}"),
                        recorded,
                    ]),
                    OutputMode::Capture(1024 * 1024),
                )?;
                if inspected.code == 0 && inspected.text()?.trim() == pin {
                    let result = recorded.to_owned();
                    self.set_image(kind, result.clone());
                    return Ok(result);
                }
            }
            drop(id);
        }
        ensure!(
            prepare,
            "accepted {kind} image is absent/stale; run make setup (or exec --prepare)"
        );
        let tmp = tempfile::Builder::new()
            .prefix("image-")
            .tempfile_in(state.parent().context("Image state parent")?)?;
        self.runner.checked(
            self.docker()
                .args([
                    "build",
                    "--pull=false",
                    "--build-arg",
                    &format!("DEVKIT_RECIPE_SHA={pin}"),
                    "--label",
                    &format!("{label}={pin}"),
                    "--iidfile",
                ])
                .arg(tmp.path())
                .arg("--file")
                .arg(recipe_path)
                .arg(self.admission.root.join(".devkit/runtime")),
            OutputMode::Inherit,
        )?;
        let image = fs::read_to_string(tmp.path())?.trim().to_owned();
        ensure!(
            image
                .strip_prefix("sha256:")
                .is_some_and(|h| SHA256_RE.is_match(h)),
            "Invalid {kind} image identity"
        );
        let inspected = self.runner.capture(self.docker().args([
            "image",
            "inspect",
            "--format",
            &format!("{{{{index .Config.Labels \"{label}\"}}}}"),
            &image,
        ]))?;
        ensure!(
            inspected.text()?.trim() == pin,
            "Invalid {kind} recipe label"
        );
        self.admission.recheck()?;
        write_atomic(&state, format!("{image}\n").as_bytes(), 0o644)?;
        self.set_image(kind, image.clone());
        Ok(image)
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
        let ContainerTask {
            component,
            profile,
            image,
            argv,
            network,
            forward_npm_secrets,
            access,
            inputs,
        } = task;
        let write_root = matches!(access, TaskAccess::ExecWrite);
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
        if let Some(inputs) = inputs {
            writes.extend(workspace_write_directories(&self.admission.state, inputs)?);
        }
        for (path, c) in &self.admission.state.components {
            if c.toolchain == Toolchain::Node {
                writes.insert(format!("{path}/node_modules"));
                for output in profile_outputs(c.tooling_profile) {
                    writes.insert(format!("{path}/{output}"));
                }
            } else {
                writes.insert(format!("{path}/target"));
            }
        }
        if self
            .admission
            .state
            .components
            .values()
            .any(|c| c.toolchain == Toolchain::Node)
        {
            writes.insert("node_modules".into());
        }
        self.dirs(writes.iter().cloned())?;
        self.dirs([
            ".devkit/cache/npm".into(),
            ".devkit/cache/cargo".into(),
            ".devkit/cache/electron".into(),
        ])?;
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
            c.args([
                "--mount",
                &bind_mount(root, Path::new("/workspace"), !write_root)?,
            ]);
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
            for path in &writes {
                let readonly = match access {
                    TaskAccess::PackageLibrary => {
                        package_release_dir.as_deref() != Some(path.as_str())
                    }
                    TaskAccess::ExecReadOnly => path.ends_with("node_modules"),
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
            for (source, target) in [
                ("npm", "/cache/npm"),
                ("cargo", "/cache/cargo"),
                ("electron", "/cache/electron"),
            ] {
                c.args([
                    "--mount",
                    &bind_mount(
                        &safe_path(root, &format!(".devkit/cache/{source}"))?,
                        Path::new(target),
                        false,
                    )?,
                ]);
            }
            c.args([
                "--mount",
                &bind_mount(&self.engine, Path::new("/devkit/devkit"), true)?,
                "--workdir",
                &component
                    .map(|p| format!("/workspace/{p}"))
                    .unwrap_or_else(|| "/workspace".into()),
            ]);
            for value in [
                "HOME=/tmp/devkit-home",
                "CI=true",
                "NPM_CONFIG_CACHE=/cache/npm",
                "NPM_CONFIG_USERCONFIG=/tmp/devkit-user.npmrc",
                "NPM_CONFIG_GLOBALCONFIG=/tmp/devkit-global.npmrc",
                "NPM_CONFIG_UPDATE_NOTIFIER=false",
                "NPM_CONFIG_FUND=false",
                "NPM_CONFIG_AUDIT=false",
                "ELECTRON_CACHE=/cache/electron",
                "CARGO_HOME=/cache/cargo",
                "RUSTUP_AUTO_INSTALL=0",
            ] {
                c.args(["--env", value]);
            }
            env_plan.write_docker_args(&mut c);
            c.args([
                "--env",
                &format!("DEVKIT_RUST_VERSION={}", self.admission.pins.rust_version),
                "--env",
                &format!(
                    "CARGO_TARGET_DIR=/workspace/{}/target",
                    component.unwrap_or(".")
                ),
            ]);
            if profile != ToolingProfile::Rust {
                let node_dirs: Vec<String> = self
                    .admission
                    .state
                    .components
                    .iter()
                    .filter(|(_, c)| c.toolchain == Toolchain::Node)
                    .map(|(p, _)| p.clone())
                    .collect();
                let npmrc_policy = crate::native::parse_and_admit_npmrc(root, &node_dirs)?;
                if network != "none" && forward_npm_secrets {
                    crate::native::verify_required_npm_env_vars(&npmrc_policy)?;
                    for var in &npmrc_policy.required_env_vars {
                        c.args(["--env", var]);
                    }
                    if let Some(ca_cert) = crate::native::resolve_ca_cert_path()? {
                        c.args([
                            "--mount",
                            &bind_mount(&ca_cert, Path::new("/devkit/ca.pem"), true)?,
                            "--env",
                            "NODE_EXTRA_CA_CERTS=/devkit/ca.pem",
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
                c.args([
                    "--security-opt",
                    &format!(
                        "seccomp={}",
                        safe_path(root, ".devkit/runtime/desktop-seccomp.json")?.display()
                    ),
                ]);
            }
            c.arg(image).args(argv);
            Ok(c)
        };
        for attempt in 0..2 {
            let token = uuid::Uuid::new_v4().to_string();
            let owned = OwnedContainer::new(format!("{}-task-{token}", self.project), token);
            let mut c = build_command(&budget, &owned)?;
            let mut output = self.runner.run_owned_unchecked(
                &mut c,
                OutputMode::TeeStderr(64 * 1024),
                &owned,
                None,
            )?;
            if output.code == 0 {
                return Ok(());
            }
            if attempt == 0
                && crate::resources::is_cgroup_delegation_failure(output.code, &output.stderr)
            {
                eprintln!(
                    "devkit: warning: Docker daemon lacks cgroup resource delegation; retrying without container cgroup limits"
                );
                budget.disable_cgroup_limits();
                *self.budget.borrow_mut() = Some(budget.clone());
                continue;
            }
            output.stderr.clear();
            output.success("Container task")?;
            return Ok(());
        }
        Ok(())
    }
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
fn runtime_paths(admission: &ToolAdmission) -> Result<()> {
    for d in [
        ".devkit/state",
        ".devkit/cache",
        ".devkit/cache/npm",
        ".devkit/cache/cargo",
        ".devkit/cache/electron",
    ] {
        ensure_directory(&safe_path(&admission.root, d)?)?;
    }
    for p in [
        ".devkit/state/node-workspace.lock",
        ".devkit/state/rust-image",
        ".devkit/state/desktop-image",
    ] {
        identity(&safe_path(&admission.root, p)?)?;
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
fn node_fingerprint(admission: &RuntimeAdmission) -> Result<String> {
    Ok(digest(format!(
        "{}\n{}",
        admission.state.release_identity, admission.native.node_inputs.digest
    )))
}
fn node_ready(ctx: &PreparedContext<'_>, fingerprint: &str) -> Result<()> {
    if ctx.node_ready_verified.get() {
        return Ok(());
    }
    let receipt = safe_path(&ctx.admission.root, ".devkit/state/node-workspace.lock")?;
    ensure!(
        identity(&receipt)?.is_some()
            && fs::read_to_string(receipt)?.trim() == fingerprint
            && safe_path(&ctx.admission.root, "node_modules")?.is_dir(),
        "workspace dependencies are absent/stale; run make setup"
    );
    ctx.node_ready_verified.set(true);
    Ok(())
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
    runtime_pending(&project_root(&request.root)?)?;
    let shared_lock = matches!(request.command.as_str(), "doctor" | "status" | "logs");
    let lock_op = format!("task {}", request.command);
    let lock = if shared_lock {
        CheckoutLock::acquire_shared(&request.root, &lock_op, Some(runner))?
    } else {
        CheckoutLock::acquire_exclusive(&request.root, &lock_op, Some(runner))?
    };
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
        Some(admit_runtime_snapshot(
            &tool.root,
            tool.state.clone(),
            tool.manifest_identity.clone(),
        )?)
    };
    let target = tool.target(&request)?;
    let mut ctx = PreparedContext::new(tool, runner)?;
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
    let fingerprint = node_fingerprint(&full)?;
    ctx.dirs(workspace_write_directories(
        &full.state,
        &full.native.inputs,
    )?)?;
    let command = request.command.as_str();
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
            image: &ctx.admission.pins.node_image,
            argv: &node_argv(
                &ctx,
                vec![
                    "npm".into(),
                    "ci".into(),
                    "--ignore-scripts".into(),
                    "--include=dev".into(),
                    "--strict-peer-deps".into(),
                ],
            ),
            network: "bridge",
            forward_npm_secrets: true,
            access: TaskAccess::Finite,
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
                        image: &ctx.admission.pins.node_image,
                        argv: &node_argv(&ctx, vec!["npm".into(), "run".into(), hook.into()]),
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
                    image: &ctx.admission.pins.node_image,
                    argv: &node_argv(&ctx, vec!["npm".into(), "run".into(), hook.into()]),
                    network: "none",
                    forward_npm_secrets: false,
                    access: TaskAccess::Finite,
                    inputs: Some(&full.native.inputs),
                })?;
            }
        }
        for (path, c) in &full.state.components {
            if c.tooling_profile == ToolingProfile::Electron {
                ctx.task(ContainerTask {
                    component: None,
                    profile: ToolingProfile::Node,
                    image: &ctx.admission.pins.node_image,
                    argv: &node_argv(
                        &ctx,
                        vec![
                            "npm".into(),
                            "exec".into(),
                            "--offline".into(),
                            format!("--workspace={path}").into(),
                            "--".into(),
                            "install-electron".into(),
                        ],
                    ),
                    network: "bridge",
                    forward_npm_secrets: false,
                    access: TaskAccess::Finite,
                    inputs: Some(&full.native.inputs),
                })?;
            }
        }
        full.recheck()?;
        runner.check_cancelled()?;
        write_atomic(&receipt, format!("{fingerprint}\n").as_bytes(), 0o644)?;
        return Ok(());
    }
    if command == "doctor" {
        crate::doctor::ensure_disk_space(&ctx.admission.root)?;
        for (path, c) in full.state.components.clone() {
            if c.toolchain == Toolchain::Rust {
                ctx.derived_image("rust", false)?;
            } else {
                ctx.node_image(false)?;
                node_ready(&ctx, &fingerprint)?;
            }
            if c.tooling_profile == ToolingProfile::Electron {
                ctx.derived_image("desktop", false)?;
            }
            let _ = path;
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
        run_development(&request, &target, &full, &fingerprint, &mut ctx)?;
        full.recheck()?;
        return Ok(());
    }
    let setup = command == "setup";
    if component.toolchain == Toolchain::Rust {
        ensure!(
            command != "package",
            "package is defined for desktop applications and publishable libraries"
        );
        let image = ctx.derived_image("rust", setup)?;
        ctx.task(ContainerTask {
            component: Some(&target),
            profile: component.tooling_profile,
            image: &image,
            argv: &[
                "/devkit/devkit".into(),
                "internal".into(),
                "rust".into(),
                command.into(),
            ],
            network: if setup { "bridge" } else { "none" },
            forward_npm_secrets: false,
            access: if command == "fix" {
                TaskAccess::Fix
            } else {
                TaskAccess::Finite
            },
            inputs: Some(&full.native.inputs),
        })?;
    } else {
        ctx.node_image(false)?;
        node_ready(&ctx, &fingerprint)?;
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
                image: &image,
                argv: &node_argv(&ctx, vec!["npm".into(), "run".into(), command.into()]),
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
pub fn exec(request: ExecRequest, runner: &ProcessRunner) -> Result<()> {
    ensure!(!request.argv.is_empty(), "exec requires ARGV after --");
    ensure!(
        ["none", "bridge"].contains(&request.network.as_str()),
        "network must be none or bridge"
    );
    runtime_pending(&project_root(&request.runtime.root)?)?;
    let lock_mode = if request.write || request.prepare {
        CheckoutLockMode::Exclusive
    } else {
        CheckoutLockMode::Shared
    };
    let _lock = CheckoutLock::acquire_with(
        &request.runtime.root,
        lock_mode,
        if lock_mode == CheckoutLockMode::Exclusive {
            "exec --write"
        } else {
            "exec"
        },
        Some(runner),
    )?;
    let tool = ToolAdmission::capture(&request.runtime)?;
    runtime_paths(&tool)?;
    let target = tool
        .target(&request.runtime)?
        .context("exec requires --component (or app/package selector)")?;
    let component = tool.state.components[&target].clone();
    let native = if component.toolchain == Toolchain::Node {
        "package.json"
    } else {
        "Cargo.toml"
    };
    ensure!(
        identity(&input_path(&tool.root, format!("{target}/{native}"))?)?.is_some(),
        "Missing registered native manifest: {target}/{native}"
    );
    let competing = if component.toolchain == Toolchain::Node {
        "Cargo.toml"
    } else {
        "package.json"
    };
    ensure!(
        identity(&input_path(&tool.root, format!("{target}/{competing}"))?)?.is_none(),
        "Ambiguous native component: {target}"
    );
    let mut ctx = PreparedContext::new(tool, runner)?;
    ctx.prerequisite()?;
    if request.write && component.toolchain == Toolchain::Node {
        let receipt = safe_path(&ctx.admission.root, ".devkit/state/node-workspace.lock")?;
        if identity(&receipt)?.is_some() {
            fs::remove_file(receipt)?;
            eprintln!(
                "devkit: note: workspace lock receipt invalidated by --write; run 'make setup' after dependency changes"
            );
        }
    }
    let image = if component.toolchain == Toolchain::Rust {
        ctx.derived_image("rust", request.prepare)?
    } else {
        ctx.node_image(request.prepare)?;
        ctx.admission.pins.node_image.clone()
    };
    let task_result = ctx.task(ContainerTask {
        component: Some(&target),
        profile: if component.toolchain == Toolchain::Rust {
            ToolingProfile::Rust
        } else {
            ToolingProfile::Node
        },
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
        if (!request.write || request.network == "none")
            && let Some(cf) = err.downcast_ref::<CommandFailure>()
        {
            return Err(failure(
                cf.code,
                format!(
                    "{}; add --write to modify checkout files, --network bridge for network access, or --prepare to pull/build missing tool images",
                    cf.message
                ),
            ));
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
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
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
    if selected.is_none()
        && !ctx
            .admission
            .state
            .components
            .values()
            .any(|c| c.role == Role::App)
        && identity(&safe_path(&ctx.admission.root, "compose.yaml")?)?.is_none()
    {
        assert_compose_inventory(&ctx.admission.root, &inventory)?;
        ctx.admission.recheck()?;
        drop(lock);
        return Ok(());
    }
    let mut command = ctx.compose()?;
    // Existing recorded derived image IDs allow interpolation; never inspect or
    // acquire tool images for service status/logs/down.
    for kind in ["rust", "desktop"] {
        let p = safe_path(&ctx.admission.root, &format!(".devkit/state/{kind}-image"))?;
        if identity(&p)?.is_some() {
            let image = fs::read_to_string(p)?.trim().to_owned();
            if image
                .strip_prefix("sha256:")
                .is_some_and(|h| SHA256_RE.is_match(h))
            {
                ctx.set_image(kind, image);
            }
        }
    }
    ctx.env(&mut command);
    match request.command.as_str() {
        "down" => {
            command.args(["--profile", "*", "down"]);
        }
        "status" => {
            command.args(["--profile", "*", "ps", "--all"]);
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
    ctx.runner.checked(&mut command, OutputMode::Inherit)?;
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
fn run_development(
    request: &RuntimeRequest,
    target: &str,
    full: &RuntimeAdmission,
    fingerprint: &str,
    ctx: &mut PreparedContext<'_>,
) -> Result<()> {
    use std::io::Write;
    ensure!(
        request.package.is_none() && full.state.components[target].role == Role::App,
        "Development requires an app selector"
    );
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
    let service = service_name(target, &full.state.components[target]).to_owned();
    let output = ctx.runner.capture(ctx.compose()?.args([
        "--profile",
        "*",
        "config",
        "--no-env-resolution",
        "--format",
        "json",
    ]))?;
    let model: Value = serde_json::from_slice(&output.stdout)?;
    let active_services = active_compose_services(&model, &service)?;
    let reached = development_components(&full.state, &model, &service)?;
    ensure!(
        !reached.is_empty(),
        "Selected Compose service has no registered application"
    );
    let state_dir = safe_path(&ctx.admission.root, ".devkit/state")?;
    let mut overlays = Vec::new();
    for path in reached {
        let c = &full.state.components[&path];
        if c.toolchain == Toolchain::Rust {
            ctx.derived_image("rust", false)?;
        } else {
            ctx.node_image(false)?;
            node_ready(ctx, fingerprint)?;
        }
        if c.tooling_profile == ToolingProfile::Electron {
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
    ctx.dirs(full.state.components.iter().flat_map(|(p, c)| {
        profile_outputs(c.tooling_profile)
            .iter()
            .map(move |o| format!("{p}/{o}"))
    }))?;
    ctx.dirs([".devkit/cache/cargo".into()])?;
    let legacy_npm_overlay = safe_path(&ctx.admission.root, ".devkit/state/compose.npm-env.json")?;
    if identity(&legacy_npm_overlay)?.is_some() {
        let _ = remove_file(&legacy_npm_overlay);
    }
    let npmrc_policy = crate::native::parse_manifest_npmrc_policy(&ctx.admission.root)?;
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
    let mut budget = ctx
        .budget
        .borrow()
        .clone()
        .context("Missing host resource budget; prerequisite not checked")?;
    for attempt in 0..2 {
        let mut res_overlay_file = None;
        if let Some(res_spec) = crate::resources::build_compose_resource_overlay(
            &budget,
            &full.state,
            &ctx.admission.pins.rust_version,
            &model,
            &active_services,
        )? {
            let mut file = tempfile::Builder::new()
                .prefix("compose-resources-")
                .suffix(".json")
                .tempfile_in(&state_dir)?;
            file.write_all(&json_bytes(&res_spec)?)?;
            res_overlay_file = Some(file);
        }
        let mut command = ctx.compose()?;
        for f in &overlays {
            command.arg("--file").arg(f.path());
        }
        if let Some(f) = &res_overlay_file {
            command.arg("--file").arg(f.path());
        }
        command.args(["up", "-d", "--wait", "--pull", "missing", &service]);
        full.recheck()?;
        assert_compose_inventory(&ctx.admission.root, &inventory)?;
        let mut up_output = ctx
            .runner
            .run(&mut command, OutputMode::TeeStderr(64 * 1024))?;
        if up_output.code == 0 {
            break;
        }
        if attempt == 0
            && crate::resources::is_compose_cgroup_delegation_failure(
                up_output.code,
                &up_output.stderr,
            )
        {
            eprintln!(
                "devkit: warning: Docker daemon lacks cgroup resource delegation; retrying Compose without container cgroup limits"
            );
            budget.disable_cgroup_limits();
            *ctx.budget.borrow_mut() = Some(budget.clone());
            continue;
        }
        up_output.stderr.clear();
        up_output.success("docker")?;
    }
    let service_to_component = app_service_component_map(&full.state);
    let mut ps_cmd = ctx.compose()?;
    ps_cmd.args(["--profile", "*", "ps", "--format", "json"]);
    let ps_out = ctx.runner.capture(&mut ps_cmd)?;
    for entry in crate::doctor::parse_compose_ps_json(&ps_out.stdout, &service_to_component)? {
        if !active_services.contains(&entry.service) || entry.state != "running" {
            continue;
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

pub fn enrich_describe(full: &RuntimeAdmission) -> Result<Value> {
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
    let receipts = crate::doctor::local_receipts(
        &full.root,
        &full.state,
        Some(&full.native.node_inputs.digest),
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

pub fn inspect_status(root: &Path, runner: &ProcessRunner) -> Result<(Value, String)> {
    let root = project_root(root)?;
    assert_no_pending(&root)?;
    let lock = CheckoutLock::acquire_shared(&root, "status", Some(runner))?;
    let req = RuntimeRequest::new(root.clone(), "status".into());
    let tool = ToolAdmission::capture_status_compat(&req)?;
    runtime_paths(&tool)?;
    let mut ctx = PreparedContext::new_for_status(tool, runner)?;
    ctx.prerequisite_service()?;
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
    let service_to_component = app_service_component_map(&ctx.admission.state);
    let has_apps = ctx
        .admission
        .state
        .components
        .values()
        .any(|c| c.role == Role::App);
    let services =
        if !has_apps && identity(&safe_path(&ctx.admission.root, "compose.yaml")?)?.is_none() {
            Vec::new()
        } else {
            for kind in ["rust", "desktop"] {
                let p = safe_path(&ctx.admission.root, &format!(".devkit/state/{kind}-image"))?;
                if identity(&p)?.is_some() {
                    let image = fs::read_to_string(p)?.trim().to_owned();
                    if image
                        .strip_prefix("sha256:")
                        .is_some_and(|h| SHA256_RE.is_match(h))
                    {
                        ctx.set_image(kind, image);
                    }
                }
            }
            let mut command = ctx.compose()?;
            ctx.env(&mut command);
            command.args(["--profile", "*", "ps", "--all", "--format", "json"]);
            let out = ctx.runner.capture(&mut command)?;
            crate::doctor::parse_compose_ps_json(&out.stdout, &service_to_component)?
        };
    assert_compose_inventory(&ctx.admission.root, &inventory)?;
    let receipts = crate::doctor::local_receipts(&ctx.admission.root, &ctx.admission.state, None)?;
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
                format!(" -> {}", svc.urls.join(", "))
            };
            lines.push(format!(
                "Service {}{}: {}{}{}",
                svc.service, comp_suffix, svc.state, health_suffix, url_suffix
            ));
        }
    }
    let result = json!({
        "root": ctx.admission.root,
        "composeProject": ctx.project,
        "services": services,
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
        require_compose(&self.runner, &mut Command::new("docker"))?;
        let mut command = Command::new("docker");
        command
            .args(["compose", "--project-directory"])
            .arg(snapshot)
            .args([
                "--project-name",
                "devkit-inventory",
                "--env-file",
                "/dev/null",
            ]);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("COMPOSE_") {
                command.env_remove(key);
            }
        }
        // This query resolves service names, not tool images. Supply inert
        // integration values and never acquire an image during normalization.
        command
            .env("DEVKIT_PROJECT_ROOT", snapshot)
            .env("DEVKIT_PROJECT_NAME", "devkit-inventory")
            .env("DEVKIT_UID", "1000")
            .env("DEVKIT_GID", "1000")
            .env("DEVKIT_NODE_IMAGE", "devkit-inventory-node:local")
            .env("DEVKIT_ENGINE_PATH", "/devkit/inventory-engine");
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
        Ok(serde_json::from_slice(&output.stdout)?)
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
