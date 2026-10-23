//! Checkout admission and container-first daily operations. Make owns the DAG.
use crate::{
    Result, catalog::*, compose::*, files::*, model::*, processes::*, projections::*, state::*,
};
use anyhow::{Context, ensure};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const HELP: &str = "make setup|check|fix|test|build [app=<name> | package=<name>]\nmake dev|up [app=<name>] [headless=1]\nmake package [app=<desktop>]\nmake status|logs [app=<name>] [tail=100] [follow=1]\nmake down  (preserves data)\nmake doctor\ndevkit exec --component apps/web [--write] [--network none|bridge] [--prepare] -- ARGV\n";
#[derive(Clone, Debug)]
pub struct RuntimeRequest {
    pub root: PathBuf,
    pub command: String,
    pub component: Option<String>,
    pub app: Option<String>,
    pub package: Option<String>,
    pub expected_manifest: Option<String>,
    pub headless: bool,
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
                    "DEVKIT_DESKTOP_RECIPE_HASH"
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
        let s = Self {
            version: get("DEVKIT_VERSION")?,
            node_image: get("DEVKIT_NODE_IMAGE")?,
            node_version: get("DEVKIT_NODE_VERSION")?,
            rust_version: get("DEVKIT_RUST_VERSION")?,
            rust_recipe: get("DEVKIT_RUST_RECIPE_HASH")?,
            desktop_recipe: get("DEVKIT_DESKTOP_RECIPE_HASH")?,
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
        Self::capture_kind(request, false)
    }
    pub fn capture_service(request: &RuntimeRequest) -> Result<Self> {
        Self::capture_kind(request, true)
    }
    fn capture_kind(request: &RuntimeRequest, service: bool) -> Result<Self> {
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
                    identity(&safe_path(&root, path)?)?.as_ref() == Some(id),
                    "Managed file conflict: {path}"
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
        ensure!(
            state.version == env!("CARGO_PKG_VERSION"),
            "Native engine version {} does not match accepted project {}; run the accepted engine",
            env!("CARGO_PKG_VERSION"),
            state.version
        );
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
    pub fn target(&self, request: &RuntimeRequest) -> Result<Option<String>> {
        ensure!(
            request.app.is_none() || request.package.is_none(),
            "app and package selectors are mutually exclusive"
        );
        for (name, area) in [(&request.app, "apps"), (&request.package, "packages")] {
            if let Some(name) = name {
                slug(name)?;
                ensure!(
                    self.state
                        .components
                        .contains_key(&format!("{area}/{name}")),
                    "Unregistered {area} selector: {name}"
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
                "Unregistered target component: {t}"
            );
        }
        Ok(target)
    }
}
#[derive(Clone, Copy)]
enum TaskAccess {
    Finite,
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
}
impl<'a> PreparedContext<'a> {
    fn new(admission: ToolAdmission, runner: &'a ProcessRunner) -> Result<Self> {
        let engine = fs::canonicalize(std::env::current_exe()?)?;
        ensure!(
            identity(&engine)?.is_some(),
            "Current native engine is not a regular executable"
        );
        let project = format!(
            "devkit-{}",
            &digest(admission.root.as_os_str().as_encoded_bytes())[..16]
        );
        Ok(Self {
            admission,
            runner,
            project,
            engine,
            rust_image: None,
            desktop_image: None,
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
    fn prerequisite(&self) -> Result<()> {
        self.runner
            .capture(self.docker().args(["compose", "version"]))?;
        let info = self
            .runner
            .capture(self.docker().args(["info", "--format", "{{.OSType}}"]))?;
        ensure!(
            info.text()?.trim() == "linux",
            "A Linux Docker daemon is required"
        );
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
        Ok(())
    }
    fn derived_image(&mut self, kind: &str, prepare: bool) -> Result<String> {
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
            access,
            inputs,
        } = task;
        let write_root = matches!(access, TaskAccess::ExecWrite);
        let fix = matches!(access, TaskAccess::Fix);
        let readonly_deps = matches!(access, TaskAccess::ExecReadOnly);
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
        let token = uuid::Uuid::new_v4().to_string();
        let owned = OwnedContainer::new(format!("{}-task-{token}", self.project), token);
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
            "--tmpfs",
            if profile == ToolingProfile::Rust {
                "/tmp:mode=1777,exec"
            } else {
                "/tmp:mode=1777"
            },
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
            let readonly = readonly_deps && path.ends_with("node_modules");
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
            "CARGO_BUILD_JOBS=2",
        ] {
            c.args(["--env", value]);
        }
        c.args([
            "--env",
            &format!("DEVKIT_RUST_VERSION={}", self.admission.pins.rust_version),
            "--env",
            &format!(
                "CARGO_TARGET_DIR=/workspace/{}/target",
                component.unwrap_or(".")
            ),
        ]);
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
                "--shm-size",
                "256m",
                "--security-opt",
                &format!(
                    "seccomp={}",
                    safe_path(root, ".devkit/runtime/desktop-seccomp.json")?.display()
                ),
            ]);
        }
        c.arg(image).args(argv);
        self.runner.run_owned(&mut c, OutputMode::Inherit, &owned)?;
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
    let receipt = safe_path(&ctx.admission.root, ".devkit/state/node-workspace.lock")?;
    ensure!(
        identity(&receipt)?.is_some()
            && fs::read_to_string(receipt)?.trim() == fingerprint
            && safe_path(&ctx.admission.root, "node_modules")?.is_dir(),
        "workspace dependencies are absent/stale; run make setup"
    );
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
            "package-snapshot"
        ]
        .contains(&request.command.as_str()),
        "Unknown task command: {}",
        request.command
    );
    // Pending objects block before creating/opening a lock, regardless of type.
    runtime_pending(&project_root(&request.root)?)?;
    let lock = CheckoutLock::acquire(&request.root)?;
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
    if ["snapshot", "package-snapshot"].contains(&request.command.as_str()) {
        if request.command == "package-snapshot" {
            ensure!(
                request.package.is_none(),
                "package selects a desktop app, not a library"
            );
            ensure!(
                ctx.admission
                    .state
                    .components
                    .values()
                    .any(|c| c.tooling_profile == ToolingProfile::Electron),
                "No desktop component is registered"
            );
        }
        return Ok(());
    }
    ctx.prerequisite()?;
    if service {
        run_service(&request, target.as_deref(), &mut ctx)?;
        ctx.admission.recheck()?;
        drop(lock);
        return Ok(());
    }
    let full = full.context("Missing quality admission")?;
    let fingerprint = node_fingerprint(&full)?;
    ctx.dirs(workspace_write_directories(
        &full.state,
        &full.native.inputs,
    )?)?;
    let command = request.command.as_str();
    if command == "setup-node" {
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
                    "--include=dev".into(),
                    "--strict-peer-deps".into(),
                ],
            ),
            network: "bridge",
            access: TaskAccess::Finite,
            inputs: Some(&full.native.inputs),
        })?;
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
            "package is defined for desktop applications"
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
        ensure!(
            command != "package" || component.tooling_profile == ToolingProfile::Electron,
            "package is defined for desktop applications"
        );
        if !setup {
            ctx.task(ContainerTask {
                component: Some(&target),
                profile: component.tooling_profile,
                image: &image,
                argv: &node_argv(&ctx, vec!["npm".into(), "run".into(), command.into()]),
                network: "none",
                access: if command == "fix" {
                    TaskAccess::Fix
                } else {
                    TaskAccess::Finite
                },
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
    let _lock = CheckoutLock::acquire(&request.runtime.root)?;
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
        }
    }
    let image = if component.toolchain == Toolchain::Rust {
        ctx.derived_image("rust", request.prepare)?
    } else {
        ctx.node_image(request.prepare)?;
        ctx.admission.pins.node_image.clone()
    };
    ctx.task(ContainerTask {
        component: Some(&target),
        profile: if component.toolchain == Toolchain::Rust {
            ToolingProfile::Rust
        } else {
            ToolingProfile::Node
        },
        image: &image,
        argv: &request.argv,
        network: &request.network,
        access: if request.write {
            TaskAccess::ExecWrite
        } else {
            TaskAccess::ExecReadOnly
        },
        inputs: None,
    })?;
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
) -> Result<()> {
    let inventory = capture_compose_inputs(&ctx.admission.root)?;
    let mut command = ctx.compose()?;
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
    ctx.runner.checked(&mut command, OutputMode::Inherit)?;
    assert_compose_inventory(&ctx.admission.root, &inventory)?;
    Ok(())
}
/// Normalized Compose, including implicit dependencies and selected profiles,
/// determines which registered applications need a prepared toolchain.
pub fn development_components(
    state: &ProjectState,
    model: &Value,
    selected: &str,
) -> Result<Vec<String>> {
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
    let reached = development_components(&full.state, &model, &service)?;
    ensure!(
        !reached.is_empty(),
        "Selected Compose service has no registered application"
    );
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
                .tempfile_in(ctx.admission.root.join(".devkit/state"))?;
            use std::io::Write;
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
    let mut command = ctx.compose()?;
    for f in &overlays {
        command.arg("--file").arg(f.path());
    }
    command.args(["up", "-d", "--wait", "--pull", "never", &service]);
    full.recheck()?;
    assert_compose_inventory(&ctx.admission.root, &inventory)?;
    ctx.runner.checked(&mut command, OutputMode::Inherit)?;
    assert_compose_inventory(&ctx.admission.root, &inventory)?;
    Ok(())
}
/// Lifecycle service inventory uses the same cancellable native process facade.
pub struct DockerComposeNormalizer {
    pub runner: ProcessRunner,
}
impl ComposeNormalizer for DockerComposeNormalizer {
    fn normalize(&self, snapshot: &Path, entrypoints: &[String]) -> Result<Value> {
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
