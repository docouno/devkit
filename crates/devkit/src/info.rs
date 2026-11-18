//! Bounded checkout metadata for `make info`. All observations are read-only.
use crate::{
    Result,
    doctor::ComposeServiceStatus,
    files::*,
    model::{DevelopmentKind, Role, Toolchain, ToolingProfile},
    processes::ProcessRunner,
    runtime::ToolPins,
    stand::{epoch::Physical, ledger, ports::Ports, preview},
    state,
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

pub(crate) struct GitCheckout {
    pub(crate) linked: bool,
    pub(crate) branch: Option<String>,
    pub(crate) admin: PathBuf,
}

pub(crate) fn git_checkout(
    root: &Path,
    runner: &ProcessRunner,
    deadline: Instant,
) -> Result<Option<GitCheckout>> {
    let git = match fs::symlink_metadata(root.join(".git")) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        git.is_dir() || git.is_file(),
        "Invalid Git worktree metadata"
    );
    if git.is_file() {
        read_regular_file(&root.join(".git"), 8192, "Git checkout pointer")?
            .context("Missing Git checkout pointer")?;
    }
    let query = |args: &[&str]| -> Result<String> {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let output = runner.capture_before(
            command
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .args(["-c", "core.fsmonitor=false"])
                .args(args),
            deadline,
        )?;
        Ok(output.text()?.trim_end_matches('\n').to_owned())
    };
    let dirs = query(&[
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-dir",
        "--git-common-dir",
    ])?;
    let mut dirs = dirs.lines();
    let top_level = fs::canonicalize(dirs.next().context("Missing Git top level")?)?;
    if top_level != root {
        return Ok(None);
    }
    let git_dir = fs::canonicalize(dirs.next().context("Missing Git directory")?)?;
    let common = fs::canonicalize(dirs.next().context("Missing shared Git directory")?)?;
    let linked = git_dir != common;
    if linked {
        let directory = common.join("worktrees");
        ensure!(
            git.is_file() && git_dir.parent() == Some(&directory),
            "Unexpected linked Git admin directory"
        );
        let (bytes, _) = read_regular_file(
            &safe_path(&git_dir, "gitdir")?,
            8192,
            "Git reverse worktree pointer",
        )?
        .context("Missing Git reverse worktree pointer")?;
        let back = git_dir.join(std::str::from_utf8(&bytes)?.trim_end_matches('\n'));
        ensure!(
            fs::canonicalize(back)? == root.join(".git"),
            "Git worktree registration changed"
        );
    } else {
        ensure!(
            git.is_dir() && git_dir == root.join(".git"),
            "Unexpected main Git admin directory"
        );
    }
    let branch = query(&["branch", "--show-current"])?;
    Ok(Some(GitCheckout {
        linked,
        branch: (!branch.is_empty()).then_some(branch),
        admin: git_dir,
    }))
}

fn observe_stand(
    root: &Path,
    runner: &ProcessRunner,
    busy: bool,
    ports: Option<&Ports>,
    state: &crate::model::ProjectState,
    branch: Option<&str>,
    selected: Option<&str>,
) -> Result<(&'static str, Vec<ComposeServiceStatus>)> {
    if busy {
        return Ok(("unknown", vec![]));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let resources = crate::stand::history::History::read(root, runner, deadline)?;
    let history = ledger::Ledger::read(root)?;
    let no_state = !exists(&safe_path(root, ".devkit/state")?)?;
    let no_services = state.components.values().all(|c| {
        c.development
            .as_ref()
            .is_none_or(|d| d.kind != DevelopmentKind::Compose)
    });
    if history.is_none() && (no_state || (no_services && ports.is_none())) {
        return Ok(("stopped", vec![]));
    }
    if history.as_ref().is_none_or(ledger::Ledger::completed_empty)
        && ports.is_some_and(|p| {
            !p.possible_actors && history.as_ref().is_none_or(|l| l.owner == p.owner)
        })
        || history.is_none() && ports.is_none() && crate::stand::epoch::trusted(root)?
    {
        return Ok(("stopped", vec![]));
    }
    let Some(ports) = ports else {
        return Ok(("unknown", vec![]));
    };
    let ledger = history.context("Stand observation requires actor history")?;
    ensure!(
        ledger.owner == ports.owner && ledger.launch_terminal(),
        "Stand actor history is unsettled"
    );
    let project = crate::doctor::compose_project_name(root);
    if let Some(resources) = resources {
        resources.verify_daemon(runner, deadline)?;
    }
    let rows = ledger::observe_until(&project, runner, deadline)?;
    if rows.is_empty() && ports.possible_actors {
        return Ok(("unknown", vec![]));
    }
    let mut services = Vec::new();
    for row in &rows {
        if row.owner != ports.owner.key
            || !ledger.containers.contains(&row.id)
            || selected.is_some_and(|s| s != row.service)
        {
            continue;
        }
        let component = state
            .components
            .iter()
            .find(|(path, component)| {
                component.role == Role::App
                    && component
                        .development
                        .as_ref()
                        .and_then(|d| d.service.as_deref())
                        .or_else(|| path.rsplit('/').next())
                        == Some(row.service.as_str())
            })
            .map(|(path, _)| path.clone());
        let condition = if row.running { "running" } else { "stopped" };
        services.push(ComposeServiceStatus {
            service: row.service.clone(),
            id: row.id.clone(),
            component,
            name: row.service.clone(),
            state: condition.into(),
            status: condition.into(),
            health: None,
            urls: vec![],
            preview: preview::PreviewStatus::default(),
        });
    }
    let scope_verified = rows
        .iter()
        .filter(|row| selected.is_none_or(|s| s == row.service))
        .all(|row| row.owner == ports.owner.key && ledger.containers.contains(&row.id));
    let known = if !scope_verified {
        "unknown"
    } else if services.iter().any(|s| s.state == "running") {
        "running"
    } else if !rows.is_empty() {
        "stopped"
    } else {
        "unknown"
    };
    let renderer = preview::Renderer::from_env(branch.unwrap_or_default())?;
    preview::enrich_observed(ports, &rows, &renderer, &mut services)?;
    for service in &mut services {
        service.urls = service
            .preview
            .endpoints
            .iter()
            .filter_map(|e| e.local_url.clone())
            .collect();
    }
    Ok((known, services))
}

pub(crate) fn inspect(
    root: &Path,
    app: Option<&str>,
    runner: &ProcessRunner,
) -> Result<(Value, String)> {
    let root = project_root(root)?;
    ensure!(root.is_dir(), "Checkout does not exist");
    let checkout = Physical::at(&root)?;
    let (kind, branch) = git_checkout(&root, runner, Instant::now() + Duration::from_millis(350))
        .ok()
        .flatten()
        .map_or(("none", None), |git| {
            (if git.linked { "linked" } else { "main" }, git.branch)
        });
    let lock = state::probe_checkout_lock(&root).ok();
    let busy = lock != Some(false);
    let lock_state = match lock {
        Some(true) => "busy",
        Some(false) => "idle",
        None => "unknown",
    };
    let owner = if lock == Some(true) {
        state::read_live_owner(&root)
    } else {
        None
    };
    let manifest_path = safe_path(&root, state::MANIFEST_PATH)?;
    let manifest = read_regular_snapshot(
        &manifest_path,
        state::MAX_MANIFEST_BYTES,
        "project manifest",
    )
    .ok()
    .flatten();
    let accepted = manifest
        .as_ref()
        .and_then(|(bytes, _, _)| state::parse_project_state(bytes).ok());
    let mut apps = Vec::new();
    let mut packages = Vec::new();
    let mut readiness = "unknown";
    let mut condition = "unknown";
    let mut services = Vec::new();
    let mut reservations = Vec::new();
    let pins_path = ".devkit/runtime/toolchains.env";
    let pins = read_regular_file(&safe_path(&root, pins_path)?, 65536, "toolchain pins")
        .ok()
        .flatten();
    let pins_version = pins
        .as_ref()
        .and_then(|(bytes, _)| ToolPins::parse(bytes).ok())
        .map(|pins| pins.version);
    let version = accepted
        .as_ref()
        .map(|s| s.version.as_str())
        .or(pins_version.as_deref());
    let pending = state::assert_no_pending(&root).is_err();
    if let Some(state) = &accepted {
        for (path, c) in &state.components {
            let name = path.split_once('/').context("Missing component slug")?.1;
            match c.role {
                Role::App => apps.push(name),
                Role::Library => packages.push(name),
            }
        }
        let selected = if let Some(app) = app {
            slug(app)?;
            let path = format!("apps/{app}");
            let component = state
                .components
                .get(&path)
                .with_context(|| format!("Unregistered app {app}"))?;
            Some(
                component
                    .development
                    .as_ref()
                    .and_then(|d| d.service.as_deref())
                    .unwrap_or(app),
            )
        } else {
            None
        };
        if !pending
            && pins.as_ref().is_some_and(|(_, id)| {
                state
                    .managed
                    .get(pins_path)
                    .is_some_and(|expected| id.matches_git_tracked(expected))
            })
            && pins_version.as_deref() == Some(&state.version)
            && state.version == env!("CARGO_PKG_VERSION")
        {
            let receipts = crate::doctor::local_receipts(&root, state, None)?;
            let needed = state.components.values().all(|c| match c.toolchain {
                Toolchain::Rust => receipts["rustImageReady"] == true,
                Toolchain::Node => {
                    receipts["nodeWorkspaceReady"] == true
                        && (c.tooling_profile != ToolingProfile::Electron
                            || receipts["desktopImageReady"] == true)
                }
            });
            readiness = if busy {
                "unknown"
            } else if needed {
                "ready"
            } else {
                "setup-required"
            };
            let ports = Ports::verified_snapshot(&root).ok().flatten();
            if let Some(ports) = &ports {
                reservations = preview::reservations_from(ports, true).unwrap_or_default();
                reservations.retain(|r| selected.is_none_or(|s| s == r.service));
            }
            if let Ok((observed, rows)) = observe_stand(
                &root,
                runner,
                busy,
                ports.as_ref(),
                state,
                branch.as_deref(),
                selected,
            ) {
                condition = observed;
                services = rows;
            }
        }
    }
    let stable = manifest
        .as_ref()
        .is_some_and(|(_, _, stamp)| stamp.matches(&manifest_path).unwrap_or(false))
        && Physical::at(&root)? == checkout
        && state::assert_no_pending(&root).is_ok();
    if !stable {
        readiness = "unknown";
        condition = "unknown";
        apps.clear();
        packages.clear();
        services.clear();
        reservations.clear();
    }
    runner.check_cancelled()?;
    let list = |items: &[&str]| {
        if items.is_empty() {
            "none".into()
        } else {
            items.join(", ")
        }
    };
    let mut lines = vec![
        format!("Checkout: {} ({kind})", root.display()),
        format!("Branch: {}", branch.as_deref().unwrap_or("none")),
        format!(
            "Devkit: {} ({}, lock {lock_state})",
            version.unwrap_or("unknown"),
            readiness.replace('-', " ")
        ),
        format!("Apps: {}", list(&apps)),
        format!("Packages: {}", list(&packages)),
        format!("Stand: {condition}"),
    ];
    if pending {
        lines.push("Update pending. Run make recover.".into());
    }
    for service in &services {
        lines.push(format!("Service {}: {}", service.service, service.state));
        lines.extend(
            service
                .urls
                .iter()
                .chain(&service.preview.preview_urls)
                .map(|url| format!("  {url}")),
        );
        if service.preview.pending_restart {
            lines.push("  Preview settings pending restart".into());
        }
    }
    lines.extend(preview::reservation_lines(&reservations, &services));
    Ok((
        json!({"root":root,"worktreeKind":kind,"branch":branch,"version":version,"readiness":readiness,
        "lock":{"state":lock_state,"owner":owner},"apps":apps,"packages":packages,
        "standState":condition,"services":services,"standPorts":reservations}),
        lines.join("\n"),
    ))
}
