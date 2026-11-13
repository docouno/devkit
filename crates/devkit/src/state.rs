use crate::{Result, catalog::*, files::*, model::*};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
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
        !s.components.is_empty(),
        "Project has no registered components"
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
            identity(&safe_path(root, path)?)?
                .as_ref()
                .is_some_and(|actual| actual.matches_git_tracked(id)),
            "Managed file conflict: {path}; restore with 'git checkout -- {path}' before running 'make sync' (or 'devkit sync')"
        );
    }
    Ok(())
}
pub fn assert_no_pending(root: &Path) -> Result<()> {
    let path = safe_path(root, ".devkit")?.join("update-pending.json");
    match fs::symlink_metadata(path) {
        Ok(_) => anyhow::bail!("An update is incomplete; run devkit recover first"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
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
            } else if m.file_type().is_socket() || m.file_type().is_fifo() {
                continue;
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
    admit_runtime_snapshot(root, s, manifest_identity)
}
/// Upgrade a parsed minimal tool/service snapshot without reparsing manifests.
pub fn admit_runtime_snapshot(
    root: &Path,
    s: ProjectState,
    manifest_identity: FileIdentity,
) -> Result<RuntimeAdmission> {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckoutLockMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockOwner {
    pub pid: u32,
    pub start_time_ticks: u64,
    pub operation: String,
    pub started_at_ms: u64,
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn proc_stat_ppid_and_start(pid: u32) -> Option<(u32, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = raw.get(raw.rfind(')')? + 1..)?;
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    if fields.len() <= 19 || fields[0] == "Z" {
        return None;
    }
    let ppid: u32 = fields[1].parse().ok()?;
    let start_ticks: u64 = fields[19].parse().ok()?;
    Some((ppid, start_ticks))
}

fn is_ancestor_or_self(target_pid: u32) -> bool {
    let mut cur = std::process::id();
    for _ in 0..64 {
        if cur == target_pid {
            return true;
        }
        let Some((ppid, _)) = proc_stat_ppid_and_start(cur) else {
            break;
        };
        if ppid <= 1 || ppid == cur {
            break;
        }
        cur = ppid;
    }
    false
}

pub fn read_live_owner(root: &Path) -> Option<LockOwner> {
    let path = safe_path(root, ".devkit/state/task.owner.json").ok()?;
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    let owner: LockOwner = serde_json::from_slice(&bytes).ok()?;
    if owner.pid == 0 {
        return None;
    }
    let (_, ticks) = proc_stat_ppid_and_start(owner.pid)?;
    (ticks == owner.start_time_ticks).then_some(owner)
}

fn write_owner_sidecar(root: &Path, operation: &str) -> Result<PathBuf> {
    use std::io::Write;
    let owner_path = safe_path(root, ".devkit/state/task.owner.json")?;
    if let Ok(meta) = fs::symlink_metadata(&owner_path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "Expected a regular file for checkout lock owner: {}",
            owner_path.display()
        );
    }
    let pid = std::process::id();
    let start_time_ticks = proc_stat_ppid_and_start(pid).map(|(_, t)| t).unwrap_or(0);
    let payload = serde_json::to_vec(&LockOwner {
        pid,
        start_time_ticks,
        operation: operation.to_owned(),
        started_at_ms: now_epoch_ms(),
    })?;
    let tmp_path = safe_path(
        root,
        &format!(
            ".devkit/state/.task.owner.{pid}.{}.tmp",
            uuid::Uuid::new_v4()
        ),
    )?;
    let write_res = (|| -> Result<()> {
        let mut tmp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp_path)?;
        tmp.write_all(&payload)?;
        drop(tmp);
        fs::rename(&tmp_path, &owner_path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res?;
    Ok(owner_path)
}

fn lock_timeout_ms() -> Result<u64> {
    match std::env::var("DEVKIT_LOCK_TIMEOUT_MS") {
        Ok(raw) if !raw.trim().is_empty() => {
            let parsed: u64 = raw.trim().parse().with_context(|| {
                format!("Invalid DEVKIT_LOCK_TIMEOUT_MS value '{raw}'; expected milliseconds")
            })?;
            Ok(parsed)
        }
        _ => Ok(120_000),
    }
}

fn shared_readers_hold_lock(lock_path: &Path) -> bool {
    let Ok(probe) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
    else {
        return false;
    };
    probe.try_lock_shared().is_ok()
}

/// Shared by lifecycle operations and native runtime commands. No inherited
/// process-wide lock protocol is needed; the open file owns the advisory lock.
pub struct CheckoutLock {
    owner_path: Option<PathBuf>,
    _file: File,
    pub root: PathBuf,
}
impl Drop for CheckoutLock {
    fn drop(&mut self) {
        if let Some(owner_path) = &self.owner_path {
            let _ = fs::remove_file(owner_path);
        }
    }
}
impl CheckoutLock {
    pub fn acquire(root: &Path) -> Result<Self> {
        Self::acquire_exclusive(root, "checkout", None)
    }
    pub fn acquire_shared(
        root: &Path,
        operation: &str,
        runner: Option<&crate::processes::ProcessRunner>,
    ) -> Result<Self> {
        Self::acquire_with(root, CheckoutLockMode::Shared, operation, runner)
    }
    pub fn acquire_exclusive(
        root: &Path,
        operation: &str,
        runner: Option<&crate::processes::ProcessRunner>,
    ) -> Result<Self> {
        Self::acquire_with(root, CheckoutLockMode::Exclusive, operation, runner)
    }
    pub fn acquire_with(
        root: &Path,
        mode: CheckoutLockMode,
        operation: &str,
        runner: Option<&crate::processes::ProcessRunner>,
    ) -> Result<Self> {
        let root = project_root(root)?;
        let p = safe_path(&root, ".devkit/state/task.lock")?;
        create_dir_all_durable(p.parent().context("Lock parent")?)?;
        // Never open a FIFO/device or follow a replacement symlink as a lock.
        // O_NONBLOCK also makes an unexpected FIFO replacement fail promptly.
        if let Ok(metadata) = fs::symlink_metadata(&p) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Expected a regular file for checkout lock: {}",
                p.display()
            );
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&p)?;
        ensure!(
            f.metadata()?.is_file(),
            "Expected a regular file for checkout lock"
        );
        let try_acquire = |file: &File| -> std::result::Result<(), fs::TryLockError> {
            match mode {
                CheckoutLockMode::Shared => file.try_lock_shared(),
                CheckoutLockMode::Exclusive => file.try_lock(),
            }
        };
        let timeout_ms = lock_timeout_ms()?;
        let started = std::time::Instant::now();
        let mut printed_wait = false;
        loop {
            match try_acquire(&f) {
                Ok(()) => {
                    let owner_path = if mode == CheckoutLockMode::Exclusive {
                        Some(write_owner_sidecar(&root, operation)?)
                    } else {
                        None
                    };
                    return Ok(Self {
                        owner_path,
                        _file: f,
                        root,
                    });
                }
                Err(fs::TryLockError::Error(err)) => {
                    return Err(err).context("Cannot lock checkout");
                }
                Err(fs::TryLockError::WouldBlock) => {
                    if let Some(r) = runner {
                        r.check_cancelled()?;
                    }
                    let elapsed = started.elapsed();
                    if let Some(owner) = read_live_owner(&root) {
                        ensure!(
                            !is_ancestor_or_self(owner.pid),
                            "Recursive devkit invocation detected inside active operation (pid {})",
                            owner.pid
                        );
                        if !printed_wait && elapsed >= std::time::Duration::from_millis(250) {
                            eprintln!(
                                "devkit: waiting for active checkout operation ({}, pid {})...",
                                owner.operation, owner.pid
                            );
                            printed_wait = true;
                        }
                        if elapsed >= std::time::Duration::from_millis(timeout_ms) {
                            let held = now_epoch_ms().saturating_sub(owner.started_at_ms) / 1000;
                            anyhow::bail!(
                                "Another devkit operation owns this checkout ({}, pid {}, held {}s)",
                                owner.operation,
                                owner.pid,
                                held
                            );
                        }
                    } else {
                        let unowned_limit_ms = if mode == CheckoutLockMode::Exclusive
                            && shared_readers_hold_lock(&p)
                        {
                            5_000.min(timeout_ms)
                        } else {
                            250.min(timeout_ms)
                        };
                        if elapsed >= std::time::Duration::from_millis(unowned_limit_ms) {
                            anyhow::bail!("Another devkit operation owns this checkout");
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    }
}
