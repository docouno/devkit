use crate::{
    Result,
    catalog::*,
    compose::assert_compose_inventory,
    files::*,
    graph::*,
    model::*,
    native::*,
    processes::{OutputMode, OwnedContainer, ProcessRunner},
    projections::*,
    release::*,
    state::*,
    transaction::*,
};
use anyhow::{Context, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

const LEGACY_GITIGNORE:&[u8]=b"**/node_modules/\n**/dist/\n**/target/\n**/release/\n.devkit/cache/\n.devkit/state/\n.env\n.env.*\n!.env.example\n";
const PROJECT_GITIGNORE:&[u8]=b"**/node_modules/\n/dist/\n/target/\n/release/\n/apps/*/dist/\n/packages/*/dist/\n/apps/*/target/\n/packages/*/target/\n/apps/*/release/\n/packages/*/release/\n/.worktrees/\n.devkit/cache/\n.devkit/state/\n.env\n.env.*\n!.env.example\n";
const PROJECT_AGENTS: &[u8] = b"# Development instructions\n\n\
- Start with `make help` and `make info`. Use `make setup` to prepare pinned tools and dependencies.\n\
- Run `make check`, `make test`, and `make build` in the checkout. These gates use offline containers.\n\
- Change dependencies through `make exec` with an explicit component selector and lockfile-only flags, for example `make exec app=<name> write=1 network=bridge command='pnpm add <pkg> --lockfile-only --ignore-scripts'` (or `package=<name>`, `-w` for `pnpm add`/`pnpm remove` in the root Node workspace without a selector, `prepare=1` before the first `make setup`, and `cargo` inside `app=`/`package=` for Rust), then run `make setup` (and `make sync` after manual `package.json` or `.npmrc` edits). Do not edit managed `toolDependencies` manually or invoke host package managers.\n\
- Use `make add-app`, `make add-package`, `make register`, `make sync`, `make update`, and `make recover` to manage components and environment updates.\n\
- Set `DEVKIT_PREVIEW_URL_TEMPLATE` in the checkout's root `.env` for a preview reverse proxy. To share the template from the checkout on branch `next`, run `git config --local devkit.previewSourceBranch next` once in this clone.\n\
- In non-interactive Worktrunk sessions (including agent PTY runners), pass `--yes` (`wt switch --create --no-cd --yes <branch>`, `wt remove --foreground --yes <branch>`, or approve hooks once with `wt config approvals add --yes`) to avoid interactive hook prompts.\n\
- Keep application and shared code in `apps/` and `packages/`. The root `Makefile` and `compose.yaml` belong to this project. See [.devkit/runtime/GUIDE.md](.devkit/runtime/GUIDE.md) for command details.\n";

const PROJECT_WORKTRUNK: &[u8] =
    br#"# Worktrunk runs approved project hooks in the target checkout.
# Pass --yes in non-interactive sessions or run wt config approvals add --yes.
# Enable the optional start with DEVKIT_WT_AUTOSTART=1.
[pre-start]
devkit-reserve = "sh .devkit/runtime/devkit.sh worktree-hook reserve"

[post-start]
devkit-start = "sh .devkit/runtime/devkit.sh worktree-hook start"

[pre-remove]
devkit-remove = "sh .devkit/runtime/devkit.sh worktree-hook remove"
"#;

fn project_readme(name: &str, state: &ProjectState) -> String {
    let mut text = format!(
        "# {name}\n\nAccepted development environment: Devkit {}.\n\n\
         ```sh\nmake setup\nmake check\nmake test\nmake build\n```\n\n\
         ## Components\n\n",
        state.version
    );
    for (path, component) in &state.components {
        let language = if component.toolchain == Toolchain::Rust {
            "Rust"
        } else {
            "Node.js"
        };
        let role = if component.role == Role::App {
            "application"
        } else {
            "shared package"
        };
        text += &format!("- `{path}` — {language} {role}.\n");
    }
    let app = state
        .components
        .iter()
        .find(|(_, component)| component.tooling_profile == ToolingProfile::Web)
        .or_else(|| {
            state
                .components
                .iter()
                .find(|(_, component)| component.role == Role::App)
        });
    if let Some((path, _)) = app {
        text += &format!(
            "\nStart development with `make dev app={}`. `make info` shows readiness, leased ports \
             and preview URLs; `make down` stops the project stack and preserves data volumes.\n",
            path.rsplit('/').next().unwrap()
        );
    }
    text += &format!(
        "\nApplication source, the root Makefile and `compose.yaml` belong to this project. \
         Shared code lives in `packages/` and is consumed through its native public interfaces. \
         See [.devkit/runtime/GUIDE.md](.devkit/runtime/GUIDE.md) for daily commands, \
         ownership boundaries, dependency workflows and library packaging.\n\n\
         Use `make add-app`, `make add-package`, `make register`, and `make update to=VERSION` \
         to register components or explicitly accept an environment update (see the \
         [Devkit documentation](https://github.com/docouno/devkit/blob/v{}/README.md)). \
         A newer external CLI does not update this project's accepted environment automatically.\n",
        env!("CARGO_PKG_VERSION")
    );
    text
}

#[derive(Clone, Debug)]
pub struct ComponentSpec {
    pub name: String,
    pub template: String,
    pub uses: Vec<String>,
}
#[derive(Default)]
pub struct InitOptions<'a> {
    pub template: Option<String>,
    pub apps: Option<Vec<ComponentSpec>>,
    pub packages: Option<Vec<ComponentSpec>>,
    pub preset: Option<String>,
    pub package_mode: Option<PackageMode>,
    pub name: Option<String>,
    pub registry: Option<&'a str>,
    pub hooks: Option<&'a dyn MutationHooks>,
    pub no_agents_md: bool,
    pub no_worktrunk: bool,
}
#[derive(Default)]
pub struct UpdateOptions<'a> {
    pub api: Option<String>,
    pub package_mode: Option<PackageMode>,
    pub registry: Option<&'a str>,
    pub compose_inventory: Option<&'a ComposeInventory>,
    pub compose_normalizer: Option<&'a dyn crate::compose::ComposeNormalizer>,
    pub runner: Option<&'a crate::processes::ProcessRunner>,
    pub hooks: Option<&'a dyn MutationHooks>,
    pub preview: bool,
    pub reconcile_lock: bool,
    pub idempotent: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub before: Option<FileIdentity>,
    pub after: Option<FileIdentity>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationOutcome {
    pub state: ProjectState,
    pub files: Vec<FileChange>,
    pub applied: bool,
    pub no_op: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_reconciliation: Option<LockReconciliationReport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicableMigration {
    pub id: String,
    pub version: String,
    pub severity: MigrationSeverity,
    pub title: String,
    pub summary: String,
    pub profiles: Vec<String>,
    pub doc_path: String,
    pub anchor: String,
    pub local_path: String,
    pub url: String,
}

pub fn applicable_migrations(
    old: &ProjectState,
    release: &AdmittedRelease,
) -> Result<Vec<ApplicableMigration>> {
    let old_ver = SemVer::parse(&old.version)?;
    let target_ver = SemVer::parse(&release.manifest.version)?;
    if old_ver >= target_ver {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for m in &release.manifest.migrations {
        let m_ver = SemVer::parse(&m.version)?;
        if !(old_ver < m_ver && m_ver <= target_ver) {
            continue;
        }
        let matches_profile = m.profiles.is_empty()
            || old.components.values().any(|c| {
                m.profiles.iter().any(|p| p == c.tooling_profile.as_str())
                    && SemVer::parse(&c.origin.version).is_ok_and(|orig| orig < m_ver)
            });
        if !matches_profile {
            continue;
        }
        let local_doc = safe_path(&release.root, &m.doc_path)?;
        out.push(ApplicableMigration {
            id: m.id.clone(),
            version: m.version.clone(),
            severity: m.severity,
            title: m.title.clone(),
            summary: m.summary.clone(),
            profiles: m.profiles.clone(),
            doc_path: m.doc_path.clone(),
            anchor: m.anchor.clone(),
            local_path: format!("{}#{}", local_doc.display(), m.anchor),
            url: format!(
                "https://github.com/docouno/devkit/blob/v{}/{}#{}",
                release.manifest.version, m.doc_path, m.anchor
            ),
        });
    }
    Ok(out)
}

/// Only native npm chooses dependency versions. The project core owns staging,
/// field ownership, graph preservation and commit, never npm's resolver policy.
fn is_reconcilable_lock_conflict(err: &anyhow::Error) -> bool {
    if crate::processes::exit_code(err) >= 128 {
        return false;
    }
    let detail = format!("{err:#}");
    [
        "ERESOLVE",
        "ETARGET",
        "EBADENGINE",
        "EOVERRIDE",
        "conflicting peer dependency",
        "unable to resolve dependency tree",
        "No matching version found",
        "ERR_PNPM_PEER_DEP_ISSUES",
        "ERR_PNPM_NO_MATCHING_VERSION",
        "ERR_PNPM_UNSUPPORTED_ENGINE",
        "ERR_PNPM_UNSUPPORTED_PLATFORM",
        "Issues with peer dependencies found",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
}

pub trait LockResolver {
    fn regenerate(
        &self,
        root: &Path,
        release: &AdmittedRelease,
        registry: Option<&str>,
    ) -> Result<()>;

    fn regenerate_reconciled(
        &self,
        root: &Path,
        release: &AdmittedRelease,
        registry: Option<&str>,
    ) -> Result<ReconcileStrategy> {
        let lock_kind = NodeLockKind::for_version(&release.manifest.version)?;
        let npmrc_policy = parse_manifest_npmrc_policy(root, lock_kind)?;
        verify_required_npm_env_vars(&npmrc_policy)?;
        resolve_ca_cert_path()?;
        let pnpm = lock_kind == NodeLockKind::Pnpm;
        let lock_path = safe_path(
            root,
            if pnpm {
                "pnpm-lock.yaml"
            } else {
                "package-lock.json"
            },
        )?;
        let seeded_backup = read_input_bytes(&lock_path)?;
        match self.regenerate(root, release, registry) {
            Ok(()) => return Ok(ReconcileStrategy::Seeded),
            Err(err) if !is_reconcilable_lock_conflict(&err) => return Err(err),
            Err(_) => {}
        }
        write_atomic(&lock_path, &seeded_backup, 0o644)?;
        if pnpm {
            let mut lock = crate::pnpm::Lock::read(root)?;
            lock.prune_dev()?;
            lock.write(root)?;
        } else {
            prune_dev_lock_packages(&lock_path)?;
        }
        match self.regenerate(root, release, registry) {
            Ok(()) => return Ok(ReconcileStrategy::PruneDev),
            Err(err) if !is_reconcilable_lock_conflict(&err) => return Err(err),
            Err(_) => {}
        }
        remove_file(&lock_path)?;
        self.regenerate(root, release, registry)?;
        Ok(ReconcileStrategy::CleanFallback)
    }
}
#[derive(Default)]
pub struct DockerPnpmResolver {
    pub cache_dir: Option<PathBuf>,
    pub offline: bool,
}
impl LockResolver for DockerPnpmResolver {
    fn regenerate(
        &self,
        root: &Path,
        release: &AdmittedRelease,
        registry: Option<&str>,
    ) -> Result<()> {
        let pins = String::from_utf8(release.bytes("runtime/toolchains.env")?)?;
        let mut image = None;
        for line in pins.lines() {
            if let Some(value) = line.strip_prefix("DEVKIT_NODE_IMAGE=") {
                ensure!(image.is_none(), "Duplicate Node image pin");
                let (name, hash) = value
                    .rsplit_once("@sha256:")
                    .context("Node image must be digest-pinned")?;
                ensure!(
                    !name.is_empty() && SHA256_RE.is_match(hash),
                    "Invalid Node image pin"
                );
                image = Some(value.to_string());
            }
        }
        let image = image.context("Missing digest-pinned Node image")?;
        let root = project_root(root)?;
        let npmrc_policy = parse_manifest_npmrc_policy(
            &root,
            NodeLockKind::for_version(&release.manifest.version)?,
        )?;
        if !self.offline {
            verify_required_npm_env_vars(&npmrc_policy)?;
        }
        let ca_cert = if self.offline {
            None
        } else {
            resolve_ca_cert_path()?
                .map(|p| crate::download::additional_ca_file(&p))
                .transpose()?
        };
        let meta = fs::metadata(&root)?;
        let runner = ProcessRunner::new()?;
        let pin = crate::runtime::PnpmPin::from_release(release)?;
        let cache = crate::cache::ReleaseCache::new(self.cache_dir.as_deref(), &runner)?;
        let pnpm = cache.ensure_pnpm(&pin, Some(&npmrc_policy), !self.offline)?;
        let mut userconfig = tempfile::Builder::new()
            .prefix("devkit-npmrc-")
            .tempfile()?;
        use std::io::Write;
        userconfig.write_all(npmrc_policy.sanitized_lines.join("\n").as_bytes())?;
        userconfig.flush()?;
        let mut budget = crate::resources::HostResourceBudget::probe_with_docker(
            &runner,
            &mut Command::new("docker"),
        )?;
        // Docker's --mount value is CSV; quoting keeps spaces and commas in the
        // source path inside one field. No shell interprets any argument.
        let mount = format!(
            "type=bind,\"source={}\",target=/workspace",
            root.to_str()
                .context("Non-UTF8 checkout")?
                .replace('"', "\"\"")
        );
        let build_command =
            |budget: &crate::resources::HostResourceBudget, token: &str| -> Result<Command> {
                let spec = budget.for_npm_resolver();
                let env_plan = budget.toolchain_env_for_resolver(&spec);
                let mut command = Command::new("docker");
                command.args([
                    "run",
                    "--rm",
                    "--init",
                    "--network",
                    if self.offline { "none" } else { "host" },
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges:true",
                    "--name",
                    &format!("devkit-resolve-{token}"),
                    "--label",
                    &format!("org.docouno.devkit.operation={token}"),
                    "--user",
                    &format!("{}:{}", meta.uid(), meta.gid()),
                    "--mount",
                    &mount,
                    "--workdir",
                    "/workspace",
                ]);
                spec.write_docker_args(&mut command);
                for (source, target) in [
                    (pnpm.as_path(), "/devkit/bin/pnpm"),
                    (userconfig.path(), "/devkit/user.npmrc"),
                ] {
                    command.arg("--mount").arg(crate::runtime::bind_mount(
                        source,
                        Path::new(target),
                        true,
                    )?);
                }
                if fs::symlink_metadata(root.join(".npmrc")).is_ok_and(|m| m.is_file()) {
                    command.arg("--mount").arg(crate::runtime::bind_mount(
                        Path::new("/dev/null"),
                        Path::new("/workspace/.npmrc"),
                        true,
                    )?);
                }
                if let Some(ca_path) = &ca_cert {
                    let ca_mount = format!(
                        "type=bind,\"source={}\",target=/devkit/ca.pem,readonly",
                        ca_path
                            .path()
                            .to_str()
                            .context("Non-UTF8 DEVKIT_CA_CERT path")?
                            .replace('"', "\"\"")
                    );
                    command.args([
                        "--mount",
                        &ca_mount,
                        "--env",
                        "NODE_EXTRA_CA_CERTS=/devkit/ca.pem",
                        "--env",
                        "SSL_CERT_FILE=/devkit/ca.pem",
                    ]);
                }
                for value in [
                    "HOME=/tmp/devkit-home",
                    "npm_config_cache=/tmp/npm-cache",
                    "NPM_CONFIG_USERCONFIG=/devkit/user.npmrc",
                    "PNPM_CONFIG_NPMRC_AUTH_FILE=/devkit/user.npmrc",
                    "PNPM_CONFIG_IGNORE_PNPMFILE=true",
                    "PNPM_CONFIG_PM_ON_FAIL=ignore",
                    "PNPM_CONFIG_RUNTIME=false",
                    "PNPM_CONFIG_RUNTIME_ON_FAIL=ignore",
                    "PATH=/devkit/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                    "npm_config_globalconfig=/tmp/devkit-global.npmrc",
                    "npm_config_update_notifier=false",
                    "npm_config_fund=false",
                    "npm_config_audit=false",
                    "npm_config_install_links=false",
                ] {
                    command.args(["--env", value]);
                }
                env_plan.write_docker_args(&mut command);
                for var in &npmrc_policy.required_env_vars {
                    if self.offline {
                        command.args(["--env", &format!("{var}=devkit-offline-unused")]);
                    } else {
                        command.args(["--env", var]);
                    }
                }
                if let Some(registry) = registry {
                    command.args(["--env", &format!("PNPM_CONFIG_REGISTRY={registry}")]);
                }
                command.arg(&image).args([
                    "/devkit/bin/pnpm",
                    "install",
                    "--lockfile-only",
                    "--ignore-scripts",
                    "--ignore-pnpmfile",
                    "--prefer-offline",
                    "--trust-lockfile",
                    "--config.strict-peer-dependencies=true",
                ]);
                if self.offline {
                    command.arg("--offline");
                }
                Ok(command)
            };
        let mut last_output = None;
        for attempt in 0..2 {
            let token = uuid::Uuid::new_v4().to_string();
            let mut command = build_command(&budget, &token)?;
            let mut owned = OwnedContainer::new(format!("devkit-resolve-{token}"), token);
            owned.label = "org.docouno.devkit.operation".into();
            let output = runner.run_owned_unchecked(
                &mut command,
                OutputMode::Capture(16 * 1024 * 1024),
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
                    "devkit: warning: Docker daemon lacks cgroup resource delegation; retrying npm resolver without container cgroup limits"
                );
                budget.disable_cgroup_limits();
                continue;
            }
            last_output = Some(output);
            break;
        }
        let output = last_output.context("Missing npm resolver output")?;
        if output.code != 0 {
            let mut combined = output.stderr;
            combined.extend_from_slice(&output.stdout);
            let excerpt = if combined.len() <= 16_384 {
                String::from_utf8_lossy(&combined).into_owned()
            } else {
                let head = String::from_utf8_lossy(&combined[..8_192]);
                let tail = String::from_utf8_lossy(&combined[combined.len() - 8_192..]);
                format!("{head}\n...[truncated]...\n{tail}")
            };
            return Err(crate::processes::failure(
                output.code,
                format!(
                    "npm lockfile resolution failed (exit {}): {}",
                    output.code,
                    excerpt.trim()
                ),
            ));
        }
        Ok(())
    }
}
pub fn environment_spec(release: &Release, mode: PackageMode) -> Result<String> {
    Ok(if mode == PackageMode::Registry {
        release.environment.version.clone()
    } else {
        format!(
            "file:../../.devkit/npm/{}",
            Path::new(&release.environment.archive)
                .file_name()
                .context("Archive filename")?
                .to_str()
                .context("Archive UTF8")?
        )
    })
}
pub fn profile_tools(
    release: &AdmittedRelease,
    profile: ToolingProfile,
    mode: PackageMode,
) -> Result<BTreeMap<String, String>> {
    if profile == ToolingProfile::Rust {
        return Ok(BTreeMap::new());
    }
    let value: Value = serde_json::from_slice(&release.bytes(&format!(
        "templates/{}/package.json",
        profile_template(profile)
    ))?)?;
    let dependencies = object(
        value
            .get("devDependencies")
            .context("Template devDependencies")?,
        "template devDependencies",
    )?;
    let mut tools = BTreeMap::new();
    for (name, spec) in dependencies {
        if !["react", "react-dom"].contains(&name.as_str()) {
            tools.insert(name.clone(), text(spec, name)?.to_string());
        }
    }
    ensure!(
        tools.contains_key(&release.manifest.environment.name),
        "Template is missing environment package"
    );
    tools.insert(
        release.manifest.environment.name.clone(),
        environment_spec(&release.manifest, mode)?,
    );
    Ok(tools)
}
pub fn sync_node_workspace(root: &Path, state: &ProjectState, name: Option<&str>) -> Result<()> {
    let paths = node_components(state)
        .iter()
        .map(|(p, _)| (*p).clone())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(());
    }
    assert_package_lock_mode(root)?;
    let path = safe_path(root, "package.json")?;
    let mut manifest: Value = if exists(&path)? {
        read_json(&path)?
    } else {
        json!({"name":name.unwrap_or("devkit-project"),"private":true})
    };
    object(&manifest, "root package")?;
    let mut names = BTreeSet::new();
    for path in &paths {
        let m = native_manifest(root, path)?;
        let n = text(
            m.get("name").context("Workspace package name")?,
            "workspace package name",
        )?;
        ensure!(
            names.insert(n.to_string()),
            "Duplicate npm workspace name: {n}"
        );
    }
    if SemVer::parse(&state.version)? >= SemVer::parse("0.5.0")? {
        manifest
            .as_object_mut()
            .context("Root package must be an object")?
            .remove("workspaces");
        let mut workspace = if exists(&safe_path(root, "pnpm-workspace.yaml")?)? {
            crate::pnpm::Workspace::read(root)?
        } else {
            crate::pnpm::Workspace::new(vec![])
        };
        workspace.packages = paths;
        // Release admission requires @docouno/env to match the accepted version
        // and verifies the packaged archive's integrity before projection.
        let environment = format!("@docouno/env@{}", state.version);
        if !workspace.minimum_release_age_exclude.contains(&environment) {
            workspace.minimum_release_age_exclude.push(environment);
        }
        workspace.validate(root)?;
        crate::pnpm::write_yaml(root, "pnpm-workspace.yaml", &workspace)?;
    } else {
        manifest["workspaces"] = json!(paths);
    }
    write_json(&path, &manifest)
}
fn native_value(root: &Path, path: &str) -> Result<Value> {
    Ok(Value::Object(native_manifest(root, path)?))
}
fn write_native(root: &Path, path: &str, value: &Value) -> Result<()> {
    write_json(&input_path(root, format!("{path}/package.json"))?, value)
}
fn development(name: &str, owner: DevelopmentOwner, desktop: bool) -> Development {
    Development {
        owner,
        kind: if desktop {
            DevelopmentKind::Desktop
        } else {
            DevelopmentKind::Compose
        },
        service: Some(name.to_string()),
        api: None,
        extra: BTreeMap::new(),
    }
}
fn local_relative(from: &str, to: &str) -> String {
    let from = from.split('/').collect::<Vec<_>>();
    let to = to.split('/').collect::<Vec<_>>();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n("..", from.len() - shared)
        .chain(to[shared..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}
pub fn render_component(
    release: &AdmittedRelease,
    destination: &Path,
    project_name: &str,
    spec: &ComponentSpec,
    role: Role,
    mode: PackageMode,
    components: &BTreeMap<String, ComponentState>,
) -> Result<(String, ComponentState, Option<LockSeed>)> {
    let definition = template(&spec.template)?;
    ensure!(
        definition.role == role,
        "Template {} does not create a {role}",
        definition.id
    );
    let path = component_path(role, &spec.name)?;
    ensure!(
        !exists(&safe_path(destination, &path)?)?,
        "Component path already exists: {path}"
    );
    ensure!(
        spec.uses.is_empty() || definition.toolchain == Toolchain::Node,
        "--use is supported for Node.js components"
    );
    let mut resolved_uses = Vec::new();
    let mut seen_use_paths = BTreeSet::new();
    for use_slug in &spec.uses {
        let dep_path = component_path(Role::Library, use_slug)?;
        ensure!(dep_path != path, "Component cannot --use itself: {path}");
        if !seen_use_paths.insert(dep_path.clone()) {
            continue;
        }
        let dep_comp = components
            .get(&dep_path)
            .with_context(|| format!("Unknown workspace package for --use: {dep_path}"))?;
        ensure!(
            dep_comp.role == Role::Library && dep_comp.toolchain == Toolchain::Node,
            "Target of --use must be a registered Node.js library: {dep_path}"
        );
        let m = native_manifest(destination, &dep_path)?;
        let dep_pkg_name = text(
            m.get("name").context("Workspace package name")?,
            "workspace package name",
        )?
        .to_string();
        resolved_uses.push((dep_path, dep_comp.tooling_profile, dep_pkg_name));
    }
    let shared_clients: Vec<&str> = resolved_uses
        .iter()
        .filter(|(_, p, _)| *p == ToolingProfile::ReactLibrary)
        .map(|(_, _, n)| n.as_str())
        .collect();
    ensure!(
        shared_clients.len() <= 1,
        "At most one react-client shared client binding is supported per component"
    );
    let prefix = format!("templates/{}/", definition.id);
    for (name, id) in &release.manifest.files {
        if let Some(suffix) = name.strip_prefix(&prefix) {
            if suffix.starts_with("variants/") {
                continue;
            }
            write_atomic(
                &safe_path(destination, &format!("{path}/{suffix}"))?,
                &release.bytes(name)?,
                id.mode,
            )?;
        }
    }
    let variant_spec = if let Some(&client_pkg) = shared_clients.first()
        && ["react-web", "electron-react"].contains(&definition.id)
    {
        Some(("shared", "__DEVKIT_CLIENT_PACKAGE__", client_pkg))
    } else if definition.id == "react-web"
        && let Some((_, _, ui_pkg)) = resolved_uses
            .iter()
            .find(|(_, p, _)| *p == ToolingProfile::ReactUi)
    {
        Some(("design-system", "__DEVKIT_UI_PACKAGE__", ui_pkg.as_str()))
    } else {
        None
    };
    if let Some((variant_name, token, replacement)) = variant_spec {
        let variant = format!("{prefix}variants/{variant_name}/");
        if release
            .manifest
            .files
            .contains_key(&format!("{variant}variant.json"))
        {
            let descriptor: Value =
                serde_json::from_slice(&release.bytes(&format!("{variant}variant.json"))?)?;
            for remove in descriptor["remove"]
                .as_array()
                .context("Variant requires explicit remove list")?
            {
                let p = safe_path(
                    destination,
                    &format!("{path}/{}", text(remove, "removed path")?),
                )?;
                if exists(&p)? {
                    if p.is_dir() {
                        fs::remove_dir_all(p)?;
                    } else {
                        remove_file(&p)?;
                    }
                }
            }
            let files_prefix = format!("{variant}files/");
            for (name, id) in &release.manifest.files {
                if let Some(suffix) = name.strip_prefix(&files_prefix) {
                    let bytes =
                        String::from_utf8(release.bytes(name)?)?.replace(token, replacement);
                    write_atomic(
                        &safe_path(destination, &format!("{path}/{suffix}"))?,
                        bytes.as_bytes(),
                        id.mode,
                    )?;
                }
            }
        }
    }
    let state = ComponentState {
        role,
        toolchain: definition.toolchain,
        tooling_profile: definition.profile,
        origin: Origin {
            template: definition.id.to_string(),
            version: release.manifest.version.clone(),
            extra: BTreeMap::new(),
        },
        tool_dependencies: profile_tools(release, definition.profile, mode)?,
        dependencies: vec![],
        development: (role == Role::App).then(|| {
            development(
                &spec.name,
                DevelopmentOwner::Managed,
                definition.id == "electron-react",
            )
        }),
        extra: BTreeMap::new(),
    };
    if definition.toolchain == Toolchain::Rust {
        return Ok((path, state, None));
    }
    let mut manifest = native_value(destination, &path)?;
    manifest["name"] = json!(component_package_name(project_name, role, &spec.name)?);
    let deps = manifest
        .get_mut("devDependencies")
        .and_then(Value::as_object_mut)
        .context("Template devDependencies must be an object")?;
    for (name, value) in &state.tool_dependencies {
        deps.insert(name.clone(), json!(value));
    }
    if !resolved_uses.is_empty() {
        if !manifest.get("dependencies").is_some_and(Value::is_object) {
            manifest["dependencies"] = json!({});
        }
        let prod_deps = manifest
            .get_mut("dependencies")
            .and_then(Value::as_object_mut)
            .context("Template dependencies must be an object")?;
        for (dep_path, _, dep_pkg_name) in &resolved_uses {
            prod_deps.insert(
                dep_pkg_name.clone(),
                if SemVer::parse(&release.manifest.version)? >= SemVer::parse("0.5.0")? {
                    json!("workspace:*")
                } else {
                    json!(format!("file:{}", local_relative(&path, dep_path)))
                },
            );
        }
    }
    write_native(destination, &path, &manifest)?;
    let lock = if exists(&safe_path(destination, &format!("{path}/pnpm-lock.yaml"))?)? {
        let lock: crate::pnpm::Lock =
            crate::pnpm::read_yaml(destination, &format!("{path}/pnpm-lock.yaml"))?;
        serde_json::to_value(lock)?
    } else {
        let mut lock: Value = read_json(&safe_path(
            destination,
            &format!("{path}/package-lock.json"),
        )?)
        .context("Template lacks canonical native lock")?;
        lock["packages"][""] = manifest.clone();
        lock["name"] = manifest["name"].clone();
        lock
    };
    let seed = LockSeed {
        prefix: path.clone(),
        lock,
    };
    Ok((path, state, Some(seed)))
}
pub fn write_managed(
    release: &AdmittedRelease,
    destination: &Path,
    state: &ProjectState,
) -> Result<BTreeMap<String, FileIdentity>> {
    let mut managed = BTreeMap::new();
    for (name, id) in &release.manifest.files {
        if name.starts_with("runtime/") {
            let target = format!(".devkit/{name}");
            write_atomic(
                &safe_path(destination, &target)?,
                &release.bytes(name)?,
                id.mode,
            )?;
            managed.insert(target, id.clone());
        }
    }
    for required in [
        "devkit.mk",
        "devkit.sh",
        "toolchains.env",
        "compose.tools.yaml",
    ] {
        ensure!(
            managed.contains_key(&format!(".devkit/runtime/{required}")),
            "Missing release runtime/{required}"
        );
    }
    if state.package_mode == PackageMode::Local {
        let target = format!(
            ".devkit/npm/{}",
            Path::new(&release.manifest.environment.archive)
                .file_name()
                .context("Archive filename")?
                .to_str()
                .context("Archive UTF8")?
        );
        write_atomic(
            &safe_path(destination, &target)?,
            &release.bytes(&release.manifest.environment.archive)?,
            0o644,
        )?;
        managed.insert(
            target.clone(),
            identity(&safe_path(destination, &target)?)?.unwrap(),
        );
    }
    for (path, c) in &state.components {
        if c.toolchain == Toolchain::Rust {
            let target = format!("{path}/rust-toolchain.toml");
            write_atomic(
                &safe_path(destination, &target)?,
                &release.bytes("templates/rust-api/rust-toolchain.toml")?,
                0o644,
            )?;
            managed.insert(
                target.clone(),
                identity(&safe_path(destination, &target)?)?.unwrap(),
            );
        }
    }
    let profiles = state
        .components
        .values()
        .map(|c| c.tooling_profile)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|p| {
            Ok((
                p.as_str().to_string(),
                profile_tools(release, p, state.package_mode)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let inputs = if exists(&safe_path(destination, "package-lock.json")?)?
        || exists(&safe_path(destination, "pnpm-lock.yaml")?)?
    {
        collect_native_input_closure(destination, state)?
    } else {
        InputIdentitySet {
            schema_version: 1,
            files: BTreeMap::new(),
            directories: if node_components(state).is_empty() {
                vec![]
            } else {
                std::iter::once(String::new())
                    .chain(node_components(state).iter().map(|(p, _)| (*p).clone()))
                    .collect()
            },
            digest: String::new(),
        }
    };
    let outputs = vec![
        (
            ".devkit/tooling-profiles.json",
            json_bytes(
                &json!({"schemaVersion":1,"version":release.manifest.version,"packageMode":state.package_mode,"profiles":profiles}),
            )?,
        ),
        (
            ".devkit/project.mk",
            render_project_make(state).into_bytes(),
        ),
        (".devkit/compose.apps.yaml", render_app_compose(state)?),
        (
            ".devkit/compose.workspace.json",
            render_workspace_compose(state, &inputs)?,
        ),
    ];
    for (name, bytes) in outputs {
        write_atomic(&safe_path(destination, name)?, &bytes, 0o644)?;
        managed.insert(
            name.to_string(),
            identity(&safe_path(destination, name)?)?.unwrap(),
        );
    }
    let checksums = managed
        .iter()
        .map(|(name, id)| format!("{}  {name}\n", id.sha256))
        .collect::<String>();
    write_atomic(
        &safe_path(destination, CHECKSUM_PATH)?,
        checksums.as_bytes(),
        0o644,
    )?;
    managed.insert(
        CHECKSUM_PATH.to_string(),
        identity(&safe_path(destination, CHECKSUM_PATH)?)?.unwrap(),
    );
    Ok(managed)
}
fn empty_target(target: &Path) -> Result<()> {
    ensure!(
        !exists(target)? || {
            ensure!(target.is_dir(), "Target must be a directory");
            sorted_entries(target)?
                .iter()
                .all(|e| e.file_name() == ".git")
        },
        "Target must be new or empty (existing .git metadata is preserved)"
    );
    Ok(())
}
fn derived_name(target: &Path) -> Result<String> {
    let raw = target
        .file_name()
        .context("Target name")?
        .to_str()
        .context("Target name UTF8")?
        .to_lowercase();
    let mut result = String::new();
    for c in raw.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            result.push(c);
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    Ok(slug(result.trim_matches('-'))?.to_string())
}
fn component_package_name(project_name: &str, role: Role, name: &str) -> Result<String> {
    let kind = if role == Role::App { "app" } else { "package" };
    if let Some(scoped) = project_name.strip_prefix('@') {
        let (scope, project) = scoped
            .split_once('/')
            .context("Invalid scoped root package identity")?;
        ensure!(
            !scope.is_empty() && !project.is_empty() && !project.contains('/'),
            "Invalid scoped root package identity"
        );
        Ok(format!("@{scope}/{project}-{kind}-{name}"))
    } else {
        Ok(format!("@{project_name}/{kind}-{name}"))
    }
}
fn existing_project_namespace(root: &Path) -> Result<String> {
    if identity(&safe_path(root, "package.json")?)?.is_some() {
        let manifest = native_manifest(root, "")?;
        if let Some(name) = manifest.get("name") {
            return Ok(text(name, "root package name")?.to_string());
        }
    }
    // Published Rust-only schema2 projects do not retain an npm namespace.
    // Their first Node member establishes a stable private fallback.
    Ok("devkit-project".to_string())
}
pub fn init_project(
    release_root: &Path,
    target: &Path,
    options: &InitOptions<'_>,
    resolver: &dyn LockResolver,
) -> Result<ProjectState> {
    let has_components = options.apps.is_some() || options.packages.is_some();
    ensure!(
        [
            options.template.is_some(),
            has_components,
            options.preset.is_some()
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
            == 1,
        "Choose exactly one of --app/--package, --preset or --template"
    );
    let release = admit_release(release_root)?;
    let target = project_root(target)?;
    let parent = target.parent().context("Target parent")?;
    ensure!(
        parent.is_dir(),
        "Target parent directory must already exist"
    );
    empty_target(&target)?;
    let name = options
        .name
        .clone()
        .map(Ok)
        .unwrap_or_else(|| derived_name(&target))?;
    slug(&name)?;
    let mode = options.package_mode.unwrap_or(PackageMode::Registry);
    let mut state = ProjectState {
        schema_version: 2,
        version: release.manifest.version.clone(),
        release_identity: release.identity.clone(),
        generation: uuid::Uuid::new_v4().to_string(),
        package_mode: mode,
        managed: BTreeMap::new(),
        components: BTreeMap::new(),
        extra: BTreeMap::new(),
    };
    let app = |name: &str, template: &str| ComponentSpec {
        name: name.to_string(),
        template: template.to_string(),
        uses: vec![],
    };
    let specs = if let Some(preset) = &options.preset {
        match preset.as_str() {
            "fullstack-web" => vec![
                (Role::App, app("api", "rust-api")),
                (Role::App, app("web", "react-web")),
            ],
            "fullstack-desktop" => vec![
                (Role::Library, app("client", "react-client")),
                (Role::App, app("api", "rust-api")),
                (
                    Role::App,
                    ComponentSpec {
                        uses: vec!["client".to_string()],
                        ..app("web", "react-web")
                    },
                ),
                (
                    Role::App,
                    ComponentSpec {
                        uses: vec!["client".to_string()],
                        ..app("desktop", "electron-react")
                    },
                ),
            ],
            "design-system" => vec![
                (Role::Library, app("ui", "react-ui")),
                (
                    Role::App,
                    ComponentSpec {
                        uses: vec!["ui".to_string()],
                        ..app("playground", "react-web")
                    },
                ),
            ],
            _ => anyhow::bail!("Unknown preset: {preset}"),
        }
    } else if has_components {
        let mut v = Vec::new();
        if let Some(pkgs) = &options.packages {
            v.extend(pkgs.iter().cloned().map(|p| (Role::Library, p)));
        }
        if let Some(apps) = &options.apps {
            v.extend(apps.iter().cloned().map(|a| (Role::App, a)));
        }
        v
    } else if options.template.as_deref() == Some("mixed") {
        vec![
            (Role::App, app("api", "rust-api")),
            (Role::App, app("web", "react-web")),
        ]
    } else {
        let t = template(options.template.as_deref().unwrap())?;
        let default_slug = match t.id {
            "node-api" => "main",
            "rust-api" => "api",
            "ts-library" => "lib",
            "react-ui" => "ui",
            "react-client" => "client",
            _ => "web",
        };
        vec![(t.role, app(default_slug, t.id))]
    };
    ensure!(!specs.is_empty(), "Init requires at least one component");
    let stage = tempfile::Builder::new()
        .prefix(".devkit-init-")
        .tempdir_in(parent)?;
    let mut seeds = vec![];
    for (role, spec) in specs {
        let (path, c, seed) = render_component(
            &release,
            stage.path(),
            &name,
            &spec,
            role,
            mode,
            &state.components,
        )?;
        state.components.insert(path, c);
        if let Some(seed) = seed {
            seeds.push(seed);
        }
    }
    if [Some("fullstack-web"), Some("fullstack-desktop")].contains(&options.preset.as_deref())
        || options.template.as_deref() == Some("mixed")
    {
        for path in ["apps/web", "apps/desktop"] {
            if let Some(c) = state.components.get_mut(path)
                && let Some(d) = &mut c.development
            {
                d.api = Some("apps/api".to_string());
            }
        }
    }
    sync_node_workspace(stage.path(), &state, Some(&name))?;
    state.managed = write_managed(&release, stage.path(), &state)?;
    if !node_components(&state).is_empty() {
        seed_workspace_lock(stage.path(), &state, &seeds)?;
        let product = capture_product_graph(stage.path(), &state)?;
        resolver.regenerate(stage.path(), &release, options.registry)?;
        assert_product_graphs_preserved(
            &product,
            &read_native_graph(stage.path(), NodeLockKind::for_state(&state)?)?,
        )?;
    }
    let deps = derive_component_dependencies(stage.path(), &state)?;
    for (path, c) in &mut state.components {
        c.dependencies = deps[path].clone();
        if c.toolchain == Toolchain::Node {
            remove_file(&safe_path(stage.path(), &format!("{path}/pnpm-lock.yaml"))?)?;
            remove_file(&safe_path(
                stage.path(),
                &format!("{path}/package-lock.json"),
            )?)?;
        }
    }
    state.managed = write_managed(&release, stage.path(), &state)?;
    write_atomic(
        &stage.path().join("Makefile"),
        b"include .devkit/runtime/devkit.mk\n",
        0o644,
    )?;
    write_atomic(
        &stage.path().join("compose.yaml"),
        b"# Product-owned development overrides.\nservices: {}\n",
        0o644,
    )?;
    write_atomic(&stage.path().join(".gitignore"), PROJECT_GITIGNORE, 0o644)?;
    if SemVer::parse(&state.version)? >= SemVer::parse("0.5.0")? {
        if !options.no_agents_md {
            write_atomic(&stage.path().join("AGENTS.md"), PROJECT_AGENTS, 0o644)?;
        }
        if !options.no_worktrunk {
            write_atomic(
                &stage.path().join(".config/wt.toml"),
                PROJECT_WORKTRUNK,
                0o644,
            )?;
        }
    }
    let readme = project_readme(&name, &state);
    write_atomic(&stage.path().join("README.md"), readme.as_bytes(), 0o644)?;
    write_json(&safe_path(stage.path(), MANIFEST_PATH)?, &state)?;
    assert_native_components(stage.path(), &state)?;
    validate_project_state(&state)?;
    release.recheck()?;
    if let Some(h) = options.hooks {
        h.after_stage()?;
    }
    empty_target(&target)?;
    if !exists(&target)? {
        crate::archive::publish_new_directory(stage.path(), &target)?;
        let _ = stage.keep();
    } else {
        for entry in sorted_entries(stage.path())? {
            let dest = target.join(entry.file_name());
            crate::archive::rename_noreplace(&entry.path(), &dest)
                .with_context(|| format!("Concurrent init conflict: {}", dest.display()))?;
        }
        sync_directory(&target)?;
        sync_directory(parent)?;
    }
    Ok(state)
}

#[derive(Clone)]
enum Change {
    Add {
        role: Role,
        spec: ComponentSpec,
    },
    Register {
        role: Role,
        name: String,
        profile: ToolingProfile,
    },
}
pub struct AdmissionSnapshot {
    pub root: PathBuf,
    pub state: ProjectState,
    pub inputs: InputIdentitySet,
    pub source_signature: String,
    pub manifest_identity: FileIdentity,
}
impl AdmissionSnapshot {
    pub fn capture(root: &Path) -> Result<Self> {
        let root = project_root(root)?;
        assert_no_pending(&root)?;
        let manifest_identity =
            identity(&safe_path(&root, MANIFEST_PATH)?)?.context("Missing project manifest")?;
        let state = read_project_state(&root)?;
        assert_managed(&root, &state)?;
        assert_tool_profiles(&root, &state)?;
        let inputs = collect_native_input_closure(&root, &state)?;
        let source_signature = source_signature(&root, Some(&state))?;
        ensure!(
            identity(&safe_path(&root, MANIFEST_PATH)?)?.as_ref() == Some(&manifest_identity),
            "Project manifest changed during admission"
        );
        Ok(Self {
            root,
            state,
            inputs,
            source_signature,
            manifest_identity,
        })
    }
    pub fn recheck(&self) -> Result<()> {
        ensure!(
            identity(&safe_path(&self.root, MANIFEST_PATH)?)?.as_ref()
                == Some(&self.manifest_identity),
            "Project manifest changed while staging"
        );
        ensure!(
            source_signature(&self.root, Some(&self.state))? == self.source_signature,
            "Project changed while staging the update; retry after reviewing concurrent edits"
        );
        ensure!(
            collect_native_input_closure(&self.root, &self.state)? == self.inputs,
            "Native input closure changed while staging"
        );
        Ok(())
    }
}
fn verify_old_tools(root: &Path, state: &ProjectState) -> Result<()> {
    assert_component_manifests(root, state)?;
    for (path, c) in &state.components {
        if c.toolchain == Toolchain::Rust {
            for name in ["Cargo.toml", "Cargo.lock"] {
                ensure!(
                    identity(&safe_path(root, &format!("{path}/{name}"))?)?.is_some(),
                    "Missing native {path}/{name}"
                );
            }
        } else {
            let m = native_manifest(root, path)?;
            let deps = object(
                m.get("devDependencies").context("Native devDependencies")?,
                "native devDependencies",
            )?;
            for (name, spec) in &c.tool_dependencies {
                ensure!(
                    deps.get(name) == Some(&json!(spec)),
                    "Tool dependency conflict: {path}/package.json devDependencies.{name} (expected {spec}; restore it or run 'devkit update')"
                );
            }
        }
    }
    Ok(())
}
fn update_tools(
    root: &Path,
    path: &str,
    old: &ComponentState,
    next: &ComponentState,
) -> Result<()> {
    let mut m = native_value(root, path)?;
    let product_deps = object(m.get("dependencies").unwrap_or(&json!({})), "dependencies")?.clone();
    let deps = m
        .get_mut("devDependencies")
        .and_then(Value::as_object_mut)
        .context("Native devDependencies")?;
    for name in old.tool_dependencies.keys() {
        deps.remove(name);
    }
    for (name, spec) in &next.tool_dependencies {
        ensure!(
            !deps.get(name).is_some_and(|v| v != &json!(spec))
                && !product_deps.get(name).is_some_and(|v| v != &json!(spec)),
            "New tool dependency conflicts with project dependency: {path}/{name}"
        );
        deps.insert(name.clone(), json!(spec));
    }
    write_native(root, path, &m)
}
fn assert_legacy_ignore_allows_component(root: &Path, path: &str, name: &str) -> Result<()> {
    if !["target", "dist", "release"].contains(&name) {
        return Ok(());
    }
    let ignore = root.join(".gitignore");
    match fs::symlink_metadata(&ignore) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            ensure!(
                fs::read(&ignore)? != LEGACY_GITIGNORE,
                "The accepted legacy .gitignore default would ignore {path} source files; adjust your project-owned .gitignore to namespace-scoped output rules before adding this component"
            );
        }
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn registration_intent(change: &Change, options: &UpdateOptions<'_>) -> Result<(String, Value)> {
    let reconcile = options.reconcile_lock;
    Ok(match change {
        Change::Add { role, spec } => (
            component_path(*role, &spec.name)?,
            json!({"template": template(&spec.template)?.id, "api": options.api, "use": spec.uses, "reconcile": reconcile}),
        ),
        Change::Register {
            role,
            name,
            profile,
        } => (
            component_path(*role, name)?,
            json!({"profile": profile, "reconcile": reconcile}),
        ),
    })
}

fn mutate(
    release_root: &Path,
    target: &Path,
    options: &UpdateOptions<'_>,
    change: Option<Change>,
    resolver: &dyn LockResolver,
) -> Result<MutationOutcome> {
    let release = admit_release(release_root)?;
    let root_candidate = project_root(target)?;
    assert_no_pending(&root_candidate)?;
    let min_from = release
        .manifest
        .min_supported_from_version
        .as_deref()
        .unwrap_or(MIN_SUPPORTED_PROJECT_VERSION);
    ensure_supported_checkout_floor(&root_candidate, change.is_none(), min_from)?;
    let op_label = match &change {
        Some(Change::Add {
            role: Role::App, ..
        }) => "add-app",
        Some(Change::Add {
            role: Role::Library,
            ..
        }) => "add-package",
        Some(Change::Register { .. }) => "register",
        None if options.preview => "preview",
        None => "sync/update",
    };
    let lock = CheckoutLock::acquire_exclusive(target, op_label, options.runner)?;
    assert_no_pending(&lock.root)?;
    let candidate_state = read_project_state(&lock.root)?;
    if !node_components(&candidate_state).is_empty()
        && SemVer::parse(&candidate_state.version)? < SemVer::parse("0.5.0")?
        && SemVer::parse(&release.manifest.version)? >= SemVer::parse("0.5.0")?
    {
        for destination in ["pnpm-lock.yaml", "pnpm-workspace.yaml"] {
            ensure!(
                !exists(&safe_path(&lock.root, destination)?)?,
                "npm migration destination already exists: {destination}; move it aside before updating"
            );
        }
    }
    let snapshot = AdmissionSnapshot::capture(&lock.root)?;
    let root = &snapshot.root;
    let old = &snapshot.state;
    let mut next = old.clone();
    verify_old_tools(root, &next)?;
    let intent = change
        .as_ref()
        .map(|change| registration_intent(change, options))
        .transpose()?;
    if options.idempotent
        && let Some((path, requested)) = &intent
        && let Some(component) = old.components.get(path)
    {
        ensure!(
            release.identity == old.release_identity,
            "Add/register requires the exact accepted release"
        );
        let matched = match component.origin.extra.get("registrationIntent") {
            Some(recorded) => recorded == requested,
            None => matches!(&change, Some(Change::Register { profile, .. })
                if *profile == component.tooling_profile && !options.reconcile_lock),
        };
        ensure!(
            matched,
            "Component {path} already exists with different or unverified registration options; use make sync reconcile=1 to reconcile its lock"
        );
        release.recheck()?;
        snapshot.recheck()?;
        return Ok(MutationOutcome {
            state: old.clone(),
            files: vec![],
            applied: false,
            no_op: true,
            lock_reconciliation: None,
        });
    }
    let owned_inventory = match (options.compose_inventory, options.compose_normalizer) {
        (None, Some(normalizer))
            if matches!(
                change,
                Some(Change::Add {
                    role: Role::App,
                    ..
                })
            ) =>
        {
            Some(crate::compose::admit_compose_inventory(root, normalizer)?)
        }
        _ => None,
    };
    let compose_inventory = options.compose_inventory.or(owned_inventory.as_ref());
    if let Some(inventory) = compose_inventory {
        assert_compose_inventory(root, inventory)?;
    }
    let adding_node = change.as_ref().is_some_and(|c| match c {
        Change::Add { spec, .. } => {
            template(&spec.template).is_ok_and(|t| t.toolchain == Toolchain::Node)
        }
        Change::Register { profile, .. } => *profile != ToolingProfile::Rust,
    });
    if !node_components(old).is_empty() || adding_node {
        assert_package_lock_mode(root)?;
    }
    if !node_components(old).is_empty() {
        let manifest = native_manifest(root, "")?;
        let expected = node_components(old)
            .iter()
            .map(|(p, _)| (*p).clone())
            .collect::<Vec<_>>();
        let mut adopting = expected.clone();
        if let Some(Change::Register { role, name, .. }) = &change {
            adopting.push(component_path(*role, name)?);
            adopting.sort();
        }
        ensure!(
            if SemVer::parse(&old.version)? >= SemVer::parse("0.5.0")? {
                let workspace = crate::pnpm::Workspace::read(root)?;
                workspace.packages == expected || workspace.packages == adopting
            } else {
                manifest.get("workspaces") == Some(&json!(expected))
                    || manifest.get("workspaces") == Some(&json!(adopting))
            },
            "Root npm workspace membership conflicts with accepted registration"
        );
    }
    if change.is_some() {
        ensure!(
            release.identity == old.release_identity,
            "Add/register requires the exact accepted release"
        );
    }
    ensure!(
        old.version != release.manifest.version || old.release_identity == release.identity,
        "A different release has the same version; use a distinctly versioned release"
    );
    let mode = options.package_mode.unwrap_or(old.package_mode);
    if let Some(Change::Add { role, spec }) = &change {
        assert_legacy_ignore_allows_component(
            root,
            &component_path(*role, &spec.name)?,
            &spec.name,
        )?;
    }
    if change.is_none()
        && !options.reconcile_lock
        && old.release_identity == release.identity
        && mode == old.package_mode
    {
        match assert_native_components(root, old)
            .and_then(|()| assert_projections(root, old, &snapshot.inputs))
        {
            Ok(()) => {
                release.recheck()?;
                snapshot.recheck()?;
                return Ok(MutationOutcome {
                    state: old.clone(),
                    files: vec![],
                    applied: false,
                    no_op: true,
                    lock_reconciliation: None,
                });
            }
            Err(e) if e.to_string().contains("projection is stale") => {}
            Err(e) => return Err(e),
        }
    }
    let stage_name = format!(".devkit/state/update-{}", uuid::Uuid::new_v4());
    let stage = safe_path(root, &stage_name)?;
    create_dir_all_durable(&stage)?;
    let staged_root = stage.join("project");
    let result = (|| {
        copy_tree(root, &staged_root, &|n| snapshot_excluded(old, n), true)?;
        let mut seeds = vec![];
        if !node_components(old).is_empty() {
            seeds.push(LockSeed {
                prefix: String::new(),
                lock: read_workspace_lock(&staged_root, NodeLockKind::for_state(old)?)?,
            });
        }
        let mut created = vec![];
        if let Some(Change::Add { role, spec }) = &change {
            let path = component_path(*role, &spec.name)?;
            ensure!(
                !next.components.contains_key(&path) && !exists(&safe_path(root, &path)?)?,
                "Component path already exists: {path}"
            );
            if *role == Role::App {
                let inventory = compose_inventory
                    .context("Adding a managed service requires admitted Compose inventory")?;
                ensure!(
                    !inventory.services.contains(&spec.name),
                    "Compose service conflict: {}",
                    spec.name
                );
            }
            let name = if template(&spec.template)?.toolchain == Toolchain::Node {
                existing_project_namespace(&staged_root)?
            } else {
                String::new()
            };
            let (path, mut c, seed) = render_component(
                &release,
                &staged_root,
                &name,
                spec,
                *role,
                mode,
                &next.components,
            )?;
            if let Some(api) = &options.api {
                slug(api)?;
                ensure!(
                    *role == Role::App
                        && ["react-web", "electron-react"].contains(&spec.template.as_str())
                        && SemVer::parse(&next.version)? >= SemVer::parse("0.5.0")?,
                    "--api requires a React web or Electron app on Devkit 0.5 or newer"
                );
                let backend_path = format!("apps/{api}");
                let backend = next
                    .components
                    .get(&backend_path)
                    .context("--api requires a registered API application")?;
                ensure!(
                    backend_path != path
                        && backend.role == Role::App
                        && matches!(
                            backend.tooling_profile,
                            ToolingProfile::Node | ToolingProfile::Rust
                        ),
                    "--api target must be a Node or Rust API application"
                );
                let integration = backend
                    .development
                    .as_ref()
                    .context("API has no development integration")?;
                ensure!(
                    integration.kind == DevelopmentKind::Compose,
                    "API must be a Compose service"
                );
                ensure!(
                    compose_inventory
                        .context("Missing Compose inventory")?
                        .services
                        .iter()
                        .any(|s| s == integration.service.as_deref().unwrap_or(api)),
                    "API Compose service is missing"
                );
                c.development
                    .as_mut()
                    .context("Frontend has no development integration")?
                    .api = Some(backend_path);
            }
            next.components.insert(path.clone(), c);
            created.push(path);
            if let Some(seed) = seed {
                seeds.push(seed);
            }
        } else if let Some(Change::Register {
            role,
            name,
            profile,
        }) = &change
        {
            let path = component_path(*role, name)?;
            ensure!(
                !next.components.contains_key(&path),
                "Component is already registered: {path}"
            );
            ensure!(
                profile_allows(*profile, *role),
                "Tooling profile {profile} cannot register a {role}"
            );
            let toolchain = profile_toolchain(*profile);
            next.components.insert(
                path.clone(),
                ComponentState {
                    role: *role,
                    toolchain,
                    tooling_profile: *profile,
                    origin: Origin {
                        template: "adopted".to_string(),
                        version: release.manifest.version.clone(),
                        extra: BTreeMap::new(),
                    },
                    tool_dependencies: profile_tools(&release, *profile, mode)?,
                    dependencies: vec![],
                    development: (*role == Role::App)
                        .then(|| development(name, DevelopmentOwner::Project, false)),
                    extra: BTreeMap::new(),
                },
            );
            verify_old_tools(&staged_root, &next)?;
            if toolchain == Toolchain::Node {
                let member_lock = safe_path(&staged_root, &format!("{path}/package-lock.json"))?;
                let pnpm_member_lock = safe_path(&staged_root, &format!("{path}/pnpm-lock.yaml"))?;
                if exists(&pnpm_member_lock)? {
                    let lock: crate::pnpm::Lock =
                        crate::pnpm::read_yaml(&staged_root, &format!("{path}/pnpm-lock.yaml"))?;
                    seeds.push(LockSeed {
                        prefix: path.clone(),
                        lock: serde_json::to_value(lock)?,
                    });
                } else if exists(&member_lock)? {
                    seeds.push(LockSeed {
                        prefix: path.clone(),
                        lock: read_json(&member_lock)?,
                    });
                } else {
                    ensure!(
                        if NodeLockKind::for_state(old)? == NodeLockKind::Pnpm {
                            crate::pnpm::Lock::read(&staged_root)?
                                .importers
                                .contains_key(&path)
                        } else {
                            package_lock(&staged_root)?.contains_key(&path)
                        },
                        "Adopted Node component requires admitted native lock membership"
                    );
                }
            }
        }
        if SemVer::parse(&next.version)? >= SemVer::parse("0.5.0")?
            && let Some((path, record)) = &intent
        {
            next.components
                .get_mut(path)
                .context("Missing new registration")?
                .origin
                .extra
                .insert("registrationIntent".into(), record.clone());
        }
        let migrating_product = if !node_components(old).is_empty()
            && SemVer::parse(&old.version)? < SemVer::parse("0.5.0")?
            && SemVer::parse(&release.manifest.version)? >= SemVer::parse("0.5.0")?
        {
            Some(crate::graph::read_legacy_npm_product_graph(
                &staged_root,
                old,
            )?)
        } else {
            None
        };
        if !node_components(old).is_empty()
            && SemVer::parse(&old.version)? < SemVer::parse("0.5.0")?
            && SemVer::parse(&release.manifest.version)? >= SemVer::parse("0.5.0")?
        {
            migrate_npm_manifests(&staged_root, &next)?;
        }
        next.version = release.manifest.version.clone();
        sync_node_workspace(&staged_root, &next, None)?;
        if !node_components(&next).is_empty() {
            seed_workspace_lock_with_reconciliation(
                &staged_root,
                &next,
                &seeds,
                options.reconcile_lock,
            )?;
        }
        let product = if node_components(&next).is_empty() {
            None
        } else {
            Some(match migrating_product {
                Some(product) => product,
                None => capture_product_graph(&staged_root, &next)?,
            })
        };
        let old_components = next.components.clone();
        for (path, c) in &mut next.components {
            c.tool_dependencies = profile_tools(&release, c.tooling_profile, mode)?;
            if c.toolchain == Toolchain::Node {
                update_tools(&staged_root, path, &old_components[path], c)?;
            }
        }
        next.package_mode = mode;
        next.version = release.manifest.version.clone();
        next.release_identity = release.identity.clone();
        next.generation = uuid::Uuid::new_v4().to_string();
        for (path, _) in node_components(&next) {
            for file in ["package-lock.json", "pnpm-lock.yaml"] {
                remove_file(&safe_path(&staged_root, &format!("{path}/{file}"))?)?;
            }
        }
        next.managed = write_managed(&release, &staged_root, &next)?;
        for name in old.managed.keys() {
            if !next.managed.contains_key(name) {
                remove_file(&safe_path(&staged_root, name)?)?;
            }
        }
        let mut lock_reconciliation = None;
        if let Some(product) = product {
            if options.reconcile_lock {
                let strategy =
                    resolver.regenerate_reconciled(&staged_root, &release, options.registry)?;
                let after_graph = read_native_graph(&staged_root, NodeLockKind::for_state(&next)?)?;
                assert_direct_product_roots_preserved(&product, &after_graph)?;
                lock_reconciliation = Some(compute_reconciliation_report(
                    strategy,
                    &product.graph,
                    &after_graph,
                ));
            } else {
                resolver.regenerate(&staged_root, &release, options.registry)?;
                let after_graph = read_native_graph(&staged_root, NodeLockKind::for_state(&next)?)?;
                assert_product_graphs_preserved(&product, &after_graph).map_err(|err| {
                    crate::cli::error(
                        "PRODUCT_GRAPH_DRIFT",
                        format_product_graph_drift(&product, &after_graph, &err),
                    )
                })?;
            }
        }
        for (path, _) in node_components(&next) {
            remove_file(&safe_path(
                &staged_root,
                &format!("{path}/package-lock.json"),
            )?)?;
        }
        for (path, _) in node_components(&next) {
            remove_file(&safe_path(&staged_root, &format!("{path}/pnpm-lock.yaml"))?)?;
        }
        let deps = derive_component_dependencies(&staged_root, &next)?;
        for (path, c) in &mut next.components {
            c.dependencies = deps[path].clone();
        }
        next.managed = write_managed(&release, &staged_root, &next)?;
        for (name, id) in &next.managed {
            if !old.managed.contains_key(name) && identity(&safe_path(root, name)?)?.is_some() {
                let adopting = matches!(&change,Some(Change::Register{role,name:component,..}) if name==&format!("{}/rust-toolchain.toml",component_path(*role,component)?));
                ensure!(
                    adopting
                        && identity(&safe_path(root, name)?)?
                            .as_ref()
                            .is_some_and(|actual| actual.matches_git_tracked(id)),
                    "New managed file conflicts with project content: {name}"
                );
            }
        }
        validate_project_state(&next)?;
        assert_native_components(&staged_root, &next)?;
        write_json(&safe_path(&staged_root, MANIFEST_PATH)?, &next)?;
        let mut paths = old
            .managed
            .keys()
            .chain(next.managed.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        paths.extend([
            MANIFEST_PATH.to_string(),
            "package.json".to_string(),
            "package-lock.json".to_string(),
            "pnpm-workspace.yaml".to_string(),
            "pnpm-lock.yaml".to_string(),
            ".npmrc".to_string(),
        ]);
        for (path, _) in node_components(&next) {
            paths.insert(format!("{path}/package.json"));
            paths.insert(format!("{path}/package-lock.json"));
            paths.insert(format!("{path}/pnpm-lock.yaml"));
        }
        for dir in &created {
            for (name, _) in file_map(&safe_path(&staged_root, dir)?)? {
                paths.insert(format!("{dir}/{name}"));
            }
        }
        let tx = PreparedTransaction::prepare(root, &stage_name, &staged_root, &paths, created)?;
        let files = tx
            .journal
            .files
            .iter()
            .map(|f| FileChange {
                path: f.path.clone(),
                before: f.before.clone(),
                after: f.after.clone(),
            })
            .collect();
        let preflight = || {
            release.recheck()?;
            snapshot.recheck()?;
            if let Some(inventory) = compose_inventory {
                assert_compose_inventory(root, inventory)?;
            }
            Ok(())
        };
        if options.preview {
            if let Some(h) = options.hooks {
                h.after_stage()?;
            }
            preflight()?;
        } else {
            tx.commit(root, options.hooks, &preflight)?;
            if old.version != next.version {
                remove_file(&safe_path(root, ".devkit/state/node-workspace.lock")?)?;
            }
        }
        Ok(MutationOutcome {
            state: next.clone(),
            files,
            applied: !options.preview,
            no_op: false,
            lock_reconciliation,
        })
    })();
    if !exists(&safe_path(root, PENDING_PATH)?)?
        && let Err(cleanup) = cleanup_unpublished_stage(root, &stage)
    {
        return match result {
            Ok(_) => {
                Err(cleanup.context("Operation completed but recovery stage cleanup was blocked"))
            }
            Err(error) => Err(error.context(format!("Recovery stage retained: {cleanup}"))),
        };
    }
    result
}

fn read_workspace_lock(root: &Path, kind: NodeLockKind) -> Result<Value> {
    if kind == NodeLockKind::Pnpm {
        let lock: crate::pnpm::Lock = crate::pnpm::read_yaml(root, "pnpm-lock.yaml")?;
        Ok(serde_json::to_value(lock)?)
    } else {
        read_json(&safe_path(root, "package-lock.json")?)
    }
}

fn migrate_npm_manifests(root: &Path, state: &ProjectState) -> Result<()> {
    let packages = package_lock(root)?;
    for (path, _) in node_components(state) {
        let mut manifest = native_value(root, path)?;
        let public = manifest.get("private") == Some(&Value::Bool(false));
        for field in ["dependencies", "optionalDependencies", "devDependencies"] {
            let Some(dependencies) = manifest.get_mut(field).and_then(Value::as_object_mut) else {
                continue;
            };
            for (name, value) in dependencies {
                if matches!(
                    parse_npm_dependency_spec(text(value, "dependency specifier")?)?,
                    NpmDepSpec::Alias { .. }
                ) {
                    continue;
                }
                if let Some(target) = locked_dependency(&packages, path, name)?
                    && let Some(component) = state.components.get(&target)
                {
                    let target_manifest = native_manifest(root, &target)?;
                    let publishable = component.role == Role::Library
                        && target_manifest.get("private") == Some(&Value::Bool(false));
                    ensure!(
                        !public || field == "devDependencies" || publishable,
                        "Publishable library {path} cannot depend on private workspace package {name}"
                    );
                    *value = json!(if public && publishable && field != "devDependencies" {
                        "workspace:^"
                    } else {
                        "workspace:*"
                    });
                }
            }
        }
        preserve_local_peers(state, &packages, path, &mut manifest)?;
        write_native(root, path, &manifest)?;
    }
    let root_path = input_path(root, "package.json")?;
    let mut root_manifest = read_json(&root_path)?;
    preserve_local_peers(state, &packages, "", &mut root_manifest)?;
    write_json(&root_path, &root_manifest)?;
    let npmrc = safe_path(root, ".npmrc")?;
    if exists(&npmrc)? {
        let original = fs::read_to_string(&npmrc)?;
        let mut content = String::new();
        for line in original.split_inclusive('\n') {
            let key_value = line
                .trim()
                .split_once('=')
                .map(|(k, v)| (k.trim(), v.trim()));
            if !matches!(
                key_value,
                Some(("package-lock", "true") | ("install-strategy", "hoisted"))
            ) {
                content.push_str(line);
            }
        }
        if content != original {
            write_atomic(&npmrc, content.as_bytes(), 0o644)?;
        }
    }
    Ok(())
}

fn preserve_local_peers(
    state: &ProjectState,
    packages: &Packages,
    path: &str,
    manifest: &mut Value,
) -> Result<()> {
    // npm can select a local peer without a physical declaration. Preserve the
    // source selection for pnpm while retaining its publishable peer range.
    for name in dependency_table(object(manifest, "manifest")?, "peerDependencies")?.keys() {
        let Some(target) = locked_dependency(packages, path, name)? else {
            continue;
        };
        if !state.components.contains_key(&target) {
            continue;
        }
        let own = crate::pnpm::PHYSICAL_FIELDS
            .iter()
            .any(|field| manifest.get(field).and_then(|v| v.get(name)).is_some());
        let shared = crate::pnpm::PHYSICAL_FIELDS.iter().any(|field| {
            packages
                .get("")
                .and_then(|root| root.get(*field))
                .and_then(|v| v.get(name))
                .is_some()
        }) && locked_dependency(packages, "", name)?.as_ref() == Some(&target);
        if !own && !shared {
            manifest
                .as_object_mut()
                .context("Manifest object")?
                .entry("devDependencies")
                .or_insert(json!({}))
                .as_object_mut()
                .context("Development dependencies")?
                .insert(name.clone(), json!("workspace:*"));
        }
    }
    Ok(())
}

fn collect_external_deps(
    manifest: &Fields,
    packages: &Packages,
    state: &ProjectState,
    location: &str,
) -> Result<BTreeMap<(String, String), String>> {
    let mut external = BTreeMap::new();
    for field in DEPENDENCY_FIELDS {
        for (name, value) in dependency_table(manifest, field)? {
            let spec = text(&value, &format!("{location}:{name}"))?;
            let parsed = parse_npm_dependency_spec(spec)?;
            if matches!(parsed, NpmDepSpec::LocalFile(_) | NpmDepSpec::Workspace(_)) {
                continue;
            }
            if matches!(parsed, NpmDepSpec::RegistryRange(_))
                && let Ok(Some(target)) = locked_dependency(packages, location, &name)
                && state.components.contains_key(&target)
            {
                continue;
            }
            external.insert((field.to_owned(), name), spec.to_owned());
        }
    }
    Ok(external)
}

fn external_deps_changed(
    staged_root: &Path,
    state: &ProjectState,
    old_lock: &Value,
    seeded_packages: &Packages,
) -> Result<bool> {
    if old_lock.get("lockfileVersion") == Some(&json!("9.0")) {
        let old = crate::pnpm::Lock::from_value(old_lock)?;
        let seeded = crate::pnpm::Lock::read(staged_root)?;
        for path in std::iter::once(String::new())
            .chain(node_components(state).iter().map(|(p, _)| (*p).clone()))
        {
            let manifest = native_manifest(staged_root, &path)?;
            let Some(importer) = old.importers.get(crate::pnpm::importer_id(&path)) else {
                return Ok(true);
            };
            for field in crate::pnpm::PHYSICAL_FIELDS {
                let mut current = dependency_table(&manifest, field)?;
                if field == "dependencies" {
                    for key in dependency_table(&manifest, "optionalDependencies")?.keys() {
                        current.remove(key);
                    }
                }
                let external = |name: &str, spec: &str| -> Result<bool> {
                    Ok(!matches!(
                        parse_npm_dependency_spec(spec)?,
                        NpmDepSpec::Workspace(_) | NpmDepSpec::LocalFile(_)
                    ) && !name.is_empty())
                };
                let before = importer
                    .field(field)
                    .iter()
                    .filter_map(|(name, dep)| match external(name, &dep.specifier) {
                        Ok(true) => Some(Ok((name.clone(), dep.specifier.clone()))),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                let after = current
                    .iter()
                    .filter_map(|(name, spec)| {
                        match text(spec, "dependency specifier")
                            .and_then(|spec| external(name, spec).map(|yes| (yes, spec)))
                        {
                            Ok((true, spec)) => Some(Ok((name.clone(), spec.to_owned()))),
                            Ok((false, _)) => None,
                            Err(e) => Some(Err(e)),
                        }
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                if before != after {
                    return Ok(true);
                }
            }
            for (name, value) in dependency_table(&manifest, "peerDependencies")? {
                if seeded
                    .target(
                        staged_root,
                        &path,
                        &manifest,
                        "peerDependencies",
                        (&name, text(&value, "peer specifier")?),
                        false,
                    )
                    .is_err()
                {
                    return Ok(true);
                }
            }
        }
        return Ok(false);
    }
    let raw_old = object(
        old_lock
            .get("packages")
            .context("Missing native lock packages")?,
        "native lock packages",
    )?;
    let mut old_packages: Packages = BTreeMap::new();
    for (loc, val) in raw_old {
        old_packages.insert(loc.clone(), object(val, loc)?.clone());
    }
    let mut locations = vec![String::new()];
    for (path, _) in node_components(state) {
        locations.push(path.clone());
    }
    for location in locations {
        let Some(old_record) = old_packages.get(&location) else {
            return Ok(true);
        };
        let current_manifest = native_manifest(staged_root, &location)?;
        let before = collect_external_deps(old_record, &old_packages, state, &location)?;
        let after = collect_external_deps(&current_manifest, seeded_packages, state, &location)?;
        if before != after {
            return Ok(true);
        }
    }
    Ok(false)
}

fn pnpm_registries_drifted(policy: &NpmrcPolicy, lock: &crate::pnpm::Lock) -> bool {
    lock.packages.iter().any(|(key, record)| {
        let Ok((name, _)) = crate::pnpm::package_parts(key) else {
            return true;
        };
        let scoped = name
            .strip_prefix('@')
            .and_then(|n| n.split_once('/'))
            .and_then(|(scope, _)| policy.scoped_registries.get(scope));
        let Some(registry) = scoped.or(policy.default_registry.as_ref()) else {
            return false;
        };
        let tarball = record
            .get("resolution")
            .and_then(|r| r.get("tarball"))
            .and_then(Value::as_str)
            .unwrap_or("https://registry.npmjs.org/");
        !tarball.starts_with("file:") && !tarball.starts_with(registry.trim_end_matches('/'))
    })
}

fn npmrc_registries_drifted(policy: &NpmrcPolicy, packages: &Packages) -> bool {
    if policy.default_registry.is_none() && policy.scoped_registries.is_empty() {
        return false;
    }
    for (location, record) in packages {
        if !location.contains("node_modules/") || record.get("link") == Some(&Value::Bool(true)) {
            continue;
        }
        let Some(resolved) = record.get("resolved").and_then(Value::as_str) else {
            continue;
        };
        if resolved.starts_with("file:") {
            continue;
        }
        let pkg_name = record
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| location.rsplit("node_modules/").next().unwrap_or(""));
        let expected_registry = if let Some(rest) = pkg_name.strip_prefix('@')
            && let Some((scope, _)) = rest.split_once('/')
            && let Some(scoped) = policy.scoped_registries.get(scope)
        {
            Some(scoped.as_str())
        } else {
            policy.default_registry.as_deref()
        };
        if let Some(prefix) = expected_registry {
            let normalized = prefix.trim_end_matches('/');
            if !resolved.starts_with(normalized) {
                return true;
            }
        }
    }
    false
}

pub fn sync_project(
    release_root: &Path,
    target: &Path,
    options: &UpdateOptions<'_>,
    resolver: &dyn LockResolver,
) -> Result<MutationOutcome> {
    let release = admit_release(release_root)?;
    let root_candidate = project_root(target)?;
    let min_from = release
        .manifest
        .min_supported_from_version
        .as_deref()
        .unwrap_or(MIN_SUPPORTED_PROJECT_VERSION);
    ensure_supported_checkout_floor(&root_candidate, false, min_from)?;
    let lock = CheckoutLock::acquire(target)?;
    let snapshot = AdmissionSnapshot::capture(&lock.root)?;
    let root = &snapshot.root;
    let old = &snapshot.state;
    let mut next = old.clone();
    ensure!(
        release.identity == old.release_identity && release.manifest.version == old.version,
        "Sync requires the exact accepted release"
    );
    ensure!(
        options.package_mode.is_none() || options.package_mode == Some(old.package_mode),
        "Sync does not change package mode"
    );
    verify_old_tools(root, &next)?;
    if !node_components(old).is_empty() {
        assert_package_lock_mode(root)?;
        let manifest = native_manifest(root, "")?;
        let expected = node_components(old)
            .iter()
            .map(|(p, _)| (*p).clone())
            .collect::<Vec<_>>();
        ensure!(
            if SemVer::parse(&old.version)? >= SemVer::parse("0.5.0")? {
                crate::pnpm::Workspace::read(root)?.packages == expected
            } else {
                manifest.get("workspaces") == Some(&json!(expected))
            },
            "Root npm workspace membership conflicts with accepted registration"
        );
    }
    let stage_name = format!(".devkit/state/update-{}", uuid::Uuid::new_v4());
    let stage = safe_path(root, &stage_name)?;
    create_dir_all_durable(&stage)?;
    let staged_root = stage.join("project");
    let result: Result<MutationOutcome> = (|| {
        copy_tree(root, &staged_root, &|n| snapshot_excluded(old, n), true)?;
        let staged_closure = collect_native_input_closure(&staged_root, &next)?;
        let npmrc_policy = parse_and_admit_npmrc(
            &staged_root,
            &staged_closure.directories,
            NodeLockKind::for_state(&next)?,
        )?;
        sync_node_workspace(&staged_root, &next, None)?;
        let mut lock_reconciliation = None;
        if !node_components(&next).is_empty() {
            let old_lock = read_workspace_lock(&staged_root, NodeLockKind::for_state(old)?)?;
            let (before_graph, retained_product) =
                capture_retained_product_graph(&staged_root, &next, &old_lock)?;
            seed_workspace_lock_with_reconciliation(
                &staged_root,
                &next,
                &[LockSeed {
                    prefix: String::new(),
                    lock: old_lock.clone(),
                }],
                options.reconcile_lock,
            )?;
            let seeded_packages = if NodeLockKind::for_state(&next)? == NodeLockKind::Pnpm {
                Packages::new()
            } else {
                package_lock(&staged_root)?
            };
            let mut needs_resolver = options.reconcile_lock
                || external_deps_changed(&staged_root, &next, &old_lock, &seeded_packages)?
                || if NodeLockKind::for_state(&next)? == NodeLockKind::Pnpm {
                    pnpm_registries_drifted(&npmrc_policy, &crate::pnpm::Lock::read(&staged_root)?)
                } else {
                    npmrc_registries_drifted(&npmrc_policy, &seeded_packages)
                };
            if !needs_resolver {
                let offline_ok = read_native_graph(&staged_root, NodeLockKind::for_state(&next)?)
                    .and_then(|_| derive_component_dependencies(&staged_root, &next))
                    .is_ok();
                if !offline_ok {
                    needs_resolver = true;
                }
            }
            if needs_resolver {
                if options.reconcile_lock {
                    let strategy =
                        resolver.regenerate_reconciled(&staged_root, &release, options.registry)?;
                    let after_graph =
                        read_native_graph(&staged_root, NodeLockKind::for_state(&next)?)?;
                    let expected_current = capture_product_graph(&staged_root, &next)?;
                    assert_direct_product_roots_preserved(&expected_current, &after_graph)?;
                    lock_reconciliation = Some(compute_reconciliation_report(
                        strategy,
                        &before_graph,
                        &after_graph,
                    ));
                } else {
                    resolver.regenerate(&staged_root, &release, options.registry)?;
                    let after_graph =
                        read_native_graph(&staged_root, NodeLockKind::for_state(&next)?)?;
                    assert_retained_product_graph_preserved(&retained_product, &after_graph)
                        .map_err(|err| {
                            crate::cli::error(
                                "PRODUCT_GRAPH_DRIFT",
                                format_product_graph_drift(&retained_product, &after_graph, &err),
                            )
                        })?;
                }
            } else {
                let after_graph = read_native_graph(&staged_root, NodeLockKind::for_state(&next)?)?;
                assert_retained_product_graph_preserved(&retained_product, &after_graph).map_err(
                    |err| {
                        crate::cli::error(
                            "PRODUCT_GRAPH_DRIFT",
                            format_product_graph_drift(&retained_product, &after_graph, &err),
                        )
                    },
                )?;
            }
            for (path, _) in node_components(&next) {
                remove_file(&safe_path(
                    &staged_root,
                    &format!("{path}/package-lock.json"),
                )?)?;
            }
        }
        for (path, _) in node_components(&next) {
            remove_file(&safe_path(&staged_root, &format!("{path}/pnpm-lock.yaml"))?)?;
        }
        let deps = derive_component_dependencies(&staged_root, &next)?;
        for (path, c) in &mut next.components {
            c.dependencies = deps[path].clone();
        }
        next.managed = write_managed(&release, &staged_root, &next)?;
        for name in old.managed.keys() {
            if !next.managed.contains_key(name) {
                remove_file(&safe_path(&staged_root, name)?)?;
            }
        }
        validate_project_state(&next)?;
        assert_native_components(&staged_root, &next)?;
        let mut paths = old
            .managed
            .keys()
            .chain(next.managed.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        paths.extend([
            MANIFEST_PATH.to_string(),
            "package.json".to_string(),
            "package-lock.json".to_string(),
            "pnpm-workspace.yaml".to_string(),
            "pnpm-lock.yaml".to_string(),
            ".npmrc".to_string(),
        ]);
        for (path, _) in node_components(&next) {
            paths.insert(format!("{path}/package.json"));
            paths.insert(format!("{path}/package-lock.json"));
            paths.insert(format!("{path}/pnpm-lock.yaml"));
        }
        let mut any_changed = next != *old;
        if !any_changed {
            for name in &paths {
                if name == MANIFEST_PATH {
                    continue;
                }
                if !FileIdentity::matches_git_tracked_opt(
                    identity(&safe_path(root, name)?)?.as_ref(),
                    identity(&safe_path(&staged_root, name)?)?.as_ref(),
                ) {
                    any_changed = true;
                    break;
                }
            }
        }
        if !any_changed {
            if let Some(h) = options.hooks {
                h.after_stage()?;
            }
            release.recheck()?;
            snapshot.recheck()?;
            return Ok(MutationOutcome {
                state: old.clone(),
                files: vec![],
                applied: false,
                no_op: true,
                lock_reconciliation,
            });
        }
        next.generation = uuid::Uuid::new_v4().to_string();
        write_json(&safe_path(&staged_root, MANIFEST_PATH)?, &next)?;
        let tx = PreparedTransaction::prepare(root, &stage_name, &staged_root, &paths, vec![])?;
        let files = tx
            .journal
            .files
            .iter()
            .map(|f| FileChange {
                path: f.path.clone(),
                before: f.before.clone(),
                after: f.after.clone(),
            })
            .collect();
        let preflight = || {
            release.recheck()?;
            snapshot.recheck()?;
            Ok(())
        };
        if options.preview {
            if let Some(h) = options.hooks {
                h.after_stage()?;
            }
            preflight()?;
        } else {
            tx.commit(root, options.hooks, &preflight)?;
        }
        Ok(MutationOutcome {
            state: next.clone(),
            files,
            applied: !options.preview,
            no_op: false,
            lock_reconciliation,
        })
    })();
    if !exists(&safe_path(root, PENDING_PATH)?)?
        && let Err(cleanup) = cleanup_unpublished_stage(root, &stage)
    {
        return match result {
            Ok(_) => {
                Err(cleanup.context("Operation completed but recovery stage cleanup was blocked"))
            }
            Err(error) => Err(error.context(format!("Recovery stage retained: {cleanup}"))),
        };
    }
    result
}
pub fn update_project(
    release_root: &Path,
    target: &Path,
    options: &UpdateOptions<'_>,
    resolver: &dyn LockResolver,
) -> Result<MutationOutcome> {
    mutate(release_root, target, options, None, resolver)
}
pub fn add_project_component(
    release_root: &Path,
    target: &Path,
    role: Role,
    spec: ComponentSpec,
    options: &UpdateOptions<'_>,
    resolver: &dyn LockResolver,
) -> Result<MutationOutcome> {
    mutate(
        release_root,
        target,
        options,
        Some(Change::Add { role, spec }),
        resolver,
    )
}
pub fn register_project_component(
    release_root: &Path,
    target: &Path,
    role: Role,
    name: String,
    profile: ToolingProfile,
    options: &UpdateOptions<'_>,
    resolver: &dyn LockResolver,
) -> Result<MutationOutcome> {
    ensure!(
        profile_allows(profile, role),
        "Tooling profile {profile} cannot register a {role}"
    );
    mutate(
        release_root,
        target,
        options,
        Some(Change::Register {
            role,
            name,
            profile,
        }),
        resolver,
    )
}
pub use crate::transaction::recover_project;
