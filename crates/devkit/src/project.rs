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
const PROJECT_GITIGNORE:&[u8]=b"**/node_modules/\n/dist/\n/target/\n/release/\n/apps/*/dist/\n/packages/*/dist/\n/apps/*/target/\n/packages/*/target/\n/apps/*/release/\n/packages/*/release/\n.devkit/cache/\n.devkit/state/\n.env\n.env.*\n!.env.example\n";

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
            "\nStart development with `make dev app={}`. `make status` shows local ports; \
             `make down` stops the project stack and preserves data volumes.\n",
            path.rsplit('/').next().unwrap()
        );
    }
    text += &format!(
        "\nApplication source, the root Makefile and `compose.yaml` belong to this project. \
         Shared code lives in `packages/` and is consumed through its native public interfaces.\n\n\
         Use the [Devkit CLI](https://github.com/docouno/devkit/blob/v{}/README.md) to add \
         components or explicitly accept an environment update. A newer CLI does not update \
         this project's accepted environment automatically.\n",
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
    pub preset: Option<String>,
    pub package_mode: Option<PackageMode>,
    pub name: Option<String>,
    pub registry: Option<&'a str>,
    pub hooks: Option<&'a dyn MutationHooks>,
}
#[derive(Default)]
pub struct UpdateOptions<'a> {
    pub package_mode: Option<PackageMode>,
    pub registry: Option<&'a str>,
    pub compose_inventory: Option<&'a ComposeInventory>,
    pub hooks: Option<&'a dyn MutationHooks>,
    pub preview: bool,
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
}

/// Only native npm chooses dependency versions. The project core owns staging,
/// field ownership, graph preservation and commit, never npm's resolver policy.
pub trait LockResolver {
    fn regenerate(
        &self,
        root: &Path,
        release: &AdmittedRelease,
        registry: Option<&str>,
    ) -> Result<()>;
}
pub struct DockerNpmResolver;
impl LockResolver for DockerNpmResolver {
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
        let meta = fs::metadata(&root)?;
        let token = uuid::Uuid::new_v4().to_string();
        // Docker's --mount value is CSV; quoting keeps spaces and commas in the
        // source path inside one field. No shell interprets any argument.
        let mount = format!(
            "type=bind,\"source={}\",target=/workspace",
            root.to_str()
                .context("Non-UTF8 checkout")?
                .replace('"', "\"\"")
        );
        let mut command = Command::new("docker");
        command.args([
            "run",
            "--rm",
            "--init",
            "--network",
            "host",
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
            "--tmpfs",
            "/tmp:mode=1777",
        ]);
        for value in [
            "HOME=/tmp/devkit-home",
            "npm_config_cache=/tmp/npm-cache",
            "npm_config_userconfig=/tmp/devkit-user.npmrc",
            "npm_config_globalconfig=/tmp/devkit-global.npmrc",
            "npm_config_update_notifier=false",
            "npm_config_fund=false",
            "npm_config_audit=false",
            "npm_config_install_links=false",
        ] {
            command.args(["--env", value]);
        }
        if let Some(registry) = registry {
            command.args(["--env", &format!("npm_config_registry={registry}")]);
        }
        command.arg(image).args([
            "npm",
            "install",
            "--package-lock-only",
            "--package-lock=true",
            "--ignore-scripts",
            "--include=dev",
            "--strict-peer-deps",
            "--legacy-peer-deps=false",
            "--force=false",
        ]);
        let runner = ProcessRunner::new()?;
        let mut owned = OwnedContainer::new(format!("devkit-resolve-{token}"), token.clone());
        owned.label = "org.docouno.devkit.operation".into();
        runner.run_owned(&mut command, OutputMode::Stderr, &owned)?;
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
    manifest["workspaces"] = json!(paths);
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
        spec.uses.len() <= 1
            && (spec.uses.is_empty() || ["react-web", "electron-react"].contains(&definition.id)),
        "This template supports one shared client binding"
    );
    let client = spec
        .uses
        .first()
        .map(|n| component_path(Role::Library, n))
        .transpose()?;
    let client_name = if let Some(client) = &client {
        ensure!(
            components
                .get(client)
                .is_some_and(|c| c.tooling_profile == ToolingProfile::ReactLibrary),
            "Shared client is not a registered React library: {client}"
        );
        let m = native_manifest(destination, client)?;
        text(m.get("name").context("Client name")?, "client package name")?.to_string()
    } else {
        String::new()
    };
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
    if client.is_some() {
        let variant = format!("{prefix}variants/shared/");
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
                let bytes = String::from_utf8(release.bytes(name)?)?
                    .replace("__DEVKIT_CLIENT_PACKAGE__", &client_name);
                write_atomic(
                    &safe_path(destination, &format!("{path}/{suffix}"))?,
                    bytes.as_bytes(),
                    id.mode,
                )?;
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
    if let Some(client) = &client {
        deps.insert(
            client_name,
            json!(format!("file:{}", local_relative(&path, client))),
        );
    }
    write_native(destination, &path, &manifest)?;
    let mut lock: Value = read_json(&safe_path(
        destination,
        &format!("{path}/package-lock.json"),
    )?)
    .context("Template lacks canonical native lock")?;
    lock["packages"][""] = manifest.clone();
    lock["name"] = manifest["name"].clone();
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
    let inputs = if exists(&safe_path(destination, "package-lock.json")?)? {
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
    ensure!(
        [
            options.template.is_some(),
            options.apps.is_some(),
            options.preset.is_some()
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
            == 1,
        "Choose exactly one of --app, --preset or --template"
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
        ensure!(preset == "fullstack-desktop", "Unknown preset: {preset}");
        vec![
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
        ]
    } else if let Some(apps) = &options.apps {
        apps.iter().cloned().map(|a| (Role::App, a)).collect()
    } else if options.template.as_deref() == Some("mixed") {
        vec![
            (Role::App, app("api", "rust-api")),
            (Role::App, app("web", "react-web")),
        ]
    } else {
        let t = template(options.template.as_deref().unwrap())?;
        vec![(
            Role::App,
            app(
                if t.id == "node-api" {
                    "main"
                } else if t.id == "rust-api" {
                    "api"
                } else {
                    "web"
                },
                t.id,
            ),
        )]
    };
    ensure!(
        specs.iter().any(|(r, _)| *r == Role::App),
        "Init requires an application"
    );
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
    if options.preset.is_some() || options.template.as_deref() == Some("mixed") {
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
        assert_product_graphs_preserved(&product, &read_native_graph(stage.path())?)?;
    }
    let deps = derive_component_dependencies(stage.path(), &state)?;
    for (path, c) in &mut state.components {
        c.dependencies = deps[path].clone();
        if c.toolchain == Toolchain::Node {
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
        fs::rename(stage.path(), &target)?;
    } else {
        for entry in sorted_entries(stage.path())? {
            let dest = target.join(entry.file_name());
            ensure!(
                !exists(&dest)?,
                "Concurrent init conflict: {}",
                dest.display()
            );
            fs::rename(entry.path(), dest)?;
        }
        sync_directory(&target)?;
    }
    sync_directory(parent)?;
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
                    "Tool dependency conflict: {path}/package.json devDependencies.{name}"
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
fn mutate(
    release_root: &Path,
    target: &Path,
    options: &UpdateOptions<'_>,
    change: Option<Change>,
    resolver: &dyn LockResolver,
) -> Result<MutationOutcome> {
    let release = admit_release(release_root)?;
    let lock = CheckoutLock::acquire(target)?;
    let snapshot = AdmissionSnapshot::capture(&lock.root)?;
    let root = &snapshot.root;
    let old = &snapshot.state;
    let mut next = old.clone();
    verify_old_tools(root, &next)?;
    if let Some(inventory) = options.compose_inventory {
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
            manifest.get("workspaces") == Some(&json!(expected))
                || manifest.get("workspaces") == Some(&json!(adopting)),
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
    if change.is_none() && old.release_identity == release.identity && mode == old.package_mode {
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
                lock: read_json(&safe_path(&staged_root, "package-lock.json")?)?,
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
                let inventory = options
                    .compose_inventory
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
            let (path, c, seed) = render_component(
                &release,
                &staged_root,
                &name,
                spec,
                *role,
                mode,
                &next.components,
            )?;
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
                if exists(&member_lock)? {
                    seeds.push(LockSeed {
                        prefix: path.clone(),
                        lock: read_json(&member_lock)?,
                    });
                } else {
                    ensure!(
                        package_lock(&staged_root)?.contains_key(&path),
                        "Adopted Node component requires admitted native lock membership"
                    );
                }
            }
        }
        sync_node_workspace(&staged_root, &next, None)?;
        if !node_components(&next).is_empty() {
            seed_workspace_lock(&staged_root, &next, &seeds)?;
        }
        let product = if node_components(&next).is_empty() {
            None
        } else {
            Some(capture_product_graph(&staged_root, &next)?)
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
        next.managed = write_managed(&release, &staged_root, &next)?;
        for name in old.managed.keys() {
            if !next.managed.contains_key(name) {
                remove_file(&safe_path(&staged_root, name)?)?;
            }
        }
        if let Some(product) = product {
            resolver.regenerate(&staged_root, &release, options.registry)?;
            assert_product_graphs_preserved(&product, &read_native_graph(&staged_root)?)?;
        }
        for (path, _) in node_components(&next) {
            remove_file(&safe_path(
                &staged_root,
                &format!("{path}/package-lock.json"),
            )?)?;
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
                    adopting && identity(&safe_path(root, name)?)?.as_ref() == Some(id),
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
        ]);
        for (path, _) in node_components(&next) {
            paths.insert(format!("{path}/package.json"));
            paths.insert(format!("{path}/package-lock.json"));
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
            if let Some(inventory) = options.compose_inventory {
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
        }
        Ok(MutationOutcome {
            state: next.clone(),
            files,
            applied: !options.preview,
            no_op: false,
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
