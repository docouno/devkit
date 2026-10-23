use crate::{Result, catalog::*, files::*, model::*};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub const MANIFEST_PATH: &str = ".devkit/manifest.json";
pub const PENDING_PATH: &str = ".devkit/update-pending.json";
pub const CHECKSUM_PATH: &str = ".devkit/runtime.sha256";
pub fn managed_path(name: &str) -> Result<&str> {
    relative_path(name)?;
    ensure!(
        name.starts_with(".devkit/runtime/")
            || [
                ".devkit/project.mk",
                ".devkit/compose.apps.yaml",
                ".devkit/compose.workspace.json",
                ".devkit/tooling-profiles.json",
                CHECKSUM_PATH
            ]
            .contains(&name)
            || MANAGED_NPM_RE.is_match(name)
            || MANAGED_RUST_RE.is_match(name),
        "Unsupported managed path: {name}"
    );
    Ok(name)
}
pub fn validate_project_state(s: &ProjectState) -> Result<()> {
    ensure!(
        s.schema_version == 2,
        "Unsupported project schemaVersion; only published schema2 is supported"
    );
    version(&s.version)?;
    ensure!(
        SHA256_RE.is_match(&s.release_identity) && GENERATION_RE.is_match(&s.generation),
        "Invalid project release identity/generation"
    );
    for (name, id) in &s.managed {
        managed_path(name)?;
        validate_identity(id)?;
    }
    for required in [
        CHECKSUM_PATH,
        ".devkit/runtime/toolchains.env",
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/tooling-profiles.json",
    ] {
        ensure!(
            s.managed.contains_key(required),
            "Missing managed identity: {required}"
        );
    }
    ensure!(
        s.components.keys().any(|p| p.starts_with("apps/")),
        "Project has no registered apps"
    );
    for (path, c) in &s.components {
        valid_component_path(path)?;
        let expected = if path.starts_with("apps/") {
            Role::App
        } else {
            Role::Library
        };
        ensure!(
            c.role == expected
                && profile_allows(c.tooling_profile, c.role)
                && c.toolchain == profile_toolchain(c.tooling_profile),
            "Invalid role/toolchain/profile: {path}"
        );
        ensure!(!c.origin.template.is_empty(), "Empty origin template");
        version(&c.origin.version)?;
        for (name, spec) in &c.tool_dependencies {
            ensure!(
                TOOL_DEPENDENCY_RE.is_match(name) && !spec.is_empty(),
                "Invalid tool dependency: {name}"
            );
        }
        let mut seen = BTreeSet::new();
        for dep in &c.dependencies {
            valid_component_path(dep)?;
            ensure!(seen.insert(dep), "Duplicate dependency: {path}");
            ensure!(
                s.components
                    .get(dep)
                    .is_some_and(|c| c.role == Role::Library),
                "Invalid library dependency: {path} -> {dep}"
            );
        }
        if let Some(d) = &c.development {
            ensure!(
                c.role == Role::App,
                "Library has development integration: {path}"
            );
            if let Some(service) = &d.service {
                slug(service)?;
            }
            if let Some(api) = &d.api {
                valid_component_path(api)?;
                ensure!(
                    s.components.get(api).is_some_and(|c| c.role == Role::App),
                    "Development API must reference a registered app"
                );
            }
        }
    }
    fn visit<'a>(
        s: &'a ProjectState,
        path: &'a str,
        active: &mut BTreeSet<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> Result<()> {
        if done.contains(path) {
            return Ok(());
        }
        ensure!(active.insert(path), "Component dependency cycle: {path}");
        for dep in &s.components[path].dependencies {
            visit(s, dep, active, done)?;
        }
        active.remove(path);
        done.insert(path);
        Ok(())
    }
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    for path in s.components.keys() {
        visit(s, path, &mut active, &mut done)?;
    }
    Ok(())
}
pub fn read_project_state(root: &Path) -> Result<ProjectState> {
    let path = safe_path(root, MANIFEST_PATH)?;
    let before = identity(&path)?.context("Missing project manifest")?;
    let bytes = fs::read(&path)?;
    ensure!(
        digest(&bytes) == before.sha256 && identity(&path)?.as_ref() == Some(&before),
        "Project manifest changed during admission"
    );
    let s: ProjectState = serde_json::from_slice(&bytes)?;
    validate_project_state(&s)?;
    Ok(s)
}
pub fn assert_managed(root: &Path, state: &ProjectState) -> Result<()> {
    for (path, id) in &state.managed {
        ensure!(
            identity(&safe_path(root, path)?)?.as_ref() == Some(id),
            "Managed file conflict: {path}"
        );
    }
    Ok(())
}
pub fn assert_no_pending(root: &Path) -> Result<()> {
    ensure!(
        identity(&safe_path(root, PENDING_PATH)?)?.is_none(),
        "An update is incomplete; run devkit recover first"
    );
    Ok(())
}
pub fn excluded_snapshot_path(name: &str) -> bool {
    name.split('/')
        .any(|p| [".git", "node_modules", ".artifacts"].contains(&p))
        || name == "target"
        || name.starts_with("target/")
        || [".devkit/state", ".devkit/cache"]
            .iter()
            .any(|p| name == *p || name.starts_with(&format!("{p}/")))
}
pub fn snapshot_excluded(state: &ProjectState, name: &str) -> bool {
    excluded_snapshot_path(name)
        || state.components.iter().any(|(p, c)| {
            profile_outputs(c.tooling_profile)
                .iter()
                .any(|o| name == format!("{p}/{o}") || name.starts_with(&format!("{p}/{o}/")))
        })
}
pub fn source_signature(root: &Path, state: Option<&ProjectState>) -> Result<String> {
    fn visit(
        root: &Path,
        dir: &Path,
        state: Option<&ProjectState>,
        rows: &mut Vec<Value>,
    ) -> Result<()> {
        for e in sorted_entries(dir)? {
            let p = e.path();
            let n = relative_name(root, &p)?;
            if state.map_or_else(|| excluded_snapshot_path(&n), |s| snapshot_excluded(s, &n)) {
                continue;
            }
            let m = fs::symlink_metadata(&p)?;
            if m.file_type().is_symlink() {
                rows.push(json!([
                    n,
                    "link",
                    fs::read_link(&p)?.to_str().context("Non-UTF8 symlink")?
                ]));
            } else if m.is_dir() {
                rows.push(json!([n, "directory"]));
                visit(root, &p, state, rows)?;
            } else if m.is_file() {
                rows.push(json!([
                    n,
                    m.permissions().mode() & 0o777,
                    digest(fs::read(&p)?)
                ]));
            } else {
                anyhow::bail!("Unsupported project input: {n}");
            }
        }
        Ok(())
    }
    let mut rows = vec![];
    visit(root, root, state, &mut rows)?;
    Ok(digest(serde_json::to_vec(&json!([
        "devkit-source-v1",
        rows
    ]))?))
}
pub fn verify_managed_set(root: &Path, state: &ProjectState) -> Result<()> {
    for required in [
        CHECKSUM_PATH,
        ".devkit/tooling-profiles.json",
        ".devkit/project.mk",
        ".devkit/compose.apps.yaml",
        ".devkit/compose.workspace.json",
        ".devkit/runtime/devkit.mk",
        ".devkit/runtime/devkit.sh",
        ".devkit/runtime/compose.tools.yaml",
        ".devkit/runtime/toolchains.env",
    ] {
        ensure!(
            state.managed.contains_key(required),
            "Missing managed runtime identity: {required}"
        );
    }
    let runtime = safe_path(root, ".devkit/runtime")?;
    for (name, _) in file_map(&runtime)? {
        let path = format!(".devkit/runtime/{name}");
        ensure!(
            state.managed.contains_key(&path),
            "Unlisted runtime file: {path}"
        );
    }
    let mut seen = BTreeSet::new();
    for line in fs::read_to_string(safe_path(root, CHECKSUM_PATH)?)?.lines() {
        if line.is_empty() {
            continue;
        }
        let (hash, path) = line.split_once("  ").context("Invalid checksum table")?;
        relative_path(path)?;
        ensure!(
            path != CHECKSUM_PATH
                && seen.insert(path.to_string())
                && state.managed.get(path).is_some_and(|v| v.sha256 == hash),
            "Checksum table differs from accepted manifest"
        );
    }
    ensure!(
        state
            .managed
            .keys()
            .all(|p| p == CHECKSUM_PATH || seen.contains(p)),
        "Checksum table omits a managed file"
    );
    Ok(())
}
pub struct RuntimeAdmission {
    pub root: PathBuf,
    pub state: ProjectState,
    pub native: crate::native::NativeAdmission,
    pub manifest_identity: FileIdentity,
}
impl RuntimeAdmission {
    pub fn recheck(&self) -> Result<()> {
        ensure!(
            identity(&safe_path(&self.root, MANIFEST_PATH)?)?.as_ref()
                == Some(&self.manifest_identity),
            "Accepted project generation changed during operation"
        );
        self.native.recheck(&self.root)
    }
}
/// Retain this snapshot for an entire native invocation instead of reparsing
/// native inputs separately for describe, output mounts and setup receipts.
pub fn admit_runtime_project(root: &Path, expected_version: &str) -> Result<RuntimeAdmission> {
    let root = project_root(root)?;
    let root = root.as_path();
    assert_no_pending(root)?;
    let manifest_identity =
        identity(&safe_path(root, MANIFEST_PATH)?)?.context("Missing project manifest")?;
    let s = read_project_state(root)?;
    ensure!(
        s.version == expected_version,
        "Manifest version differs from accepted toolchains.env"
    );
    assert_managed(root, &s)?;
    verify_managed_set(root, &s)?;
    let native = crate::native::admit_native_components(root, &s)?;
    crate::projections::assert_projections(root, &s, &native.inputs)?;
    let snapshot = RuntimeAdmission {
        root: root.to_path_buf(),
        state: s,
        native,
        manifest_identity,
    };
    snapshot.recheck()?;
    Ok(snapshot)
}
pub fn read_runtime_project(root: &Path, expected_version: &str) -> Result<ProjectState> {
    Ok(admit_runtime_project(root, expected_version)?.state)
}
/// Shared by lifecycle operations and native runtime commands. No inherited
/// process-wide lock protocol is needed; the open file owns the advisory lock.
pub struct CheckoutLock {
    _file: File,
    pub root: PathBuf,
}
impl CheckoutLock {
    pub fn acquire(root: &Path) -> Result<Self> {
        let root = project_root(root)?;
        let p = safe_path(&root, ".devkit/state/task.lock")?;
        create_dir_all_durable(p.parent().context("Lock parent")?)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)?;
        f.try_lock()
            .context("Another devkit operation owns this checkout")?;
        Ok(Self { _file: f, root })
    }
}
