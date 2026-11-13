//! Lock-free host and checkout diagnostics (`devkit doctor`), `statvfs` disk checks,
//! Compose `ps --format json` port discovery, and zero-Docker local readiness receipts.
use crate::{
    Result,
    files::*,
    model::*,
    processes::{OutputMode, ProcessRunner},
    resources::{
        DelegationState, DockerDaemonInfo, GIB, HostResourceBudget, MIB, ProbeEnvironment,
    },
    runtime::{ToolPins, validate_compose_version},
    state,
};
use anyhow::{Context, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpaceStats {
    pub path: PathBuf,
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub available_inodes: u64,
    pub total_inodes: u64,
}

fn nearest_existing_dir(target: &Path) -> PathBuf {
    let mut cur = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(target)
    };
    loop {
        if let Ok(meta) = fs::metadata(&cur)
            && meta.is_dir()
        {
            return cur;
        }
        let Some(parent) = cur.parent() else {
            return PathBuf::from("/");
        };
        if parent.as_os_str().is_empty() {
            return PathBuf::from("/");
        }
        cur = parent.to_path_buf();
    }
}

pub fn statvfs_stats(target: &Path) -> Result<DiskSpaceStats> {
    let probe = nearest_existing_dir(target);
    let c_path = CString::new(probe.as_os_str().as_bytes())
        .with_context(|| format!("Path contains interior NUL byte: {}", probe.display()))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated path and stat points to zeroed libc::statvfs.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs failed for {}", probe.display()));
    }
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Ok(DiskSpaceStats {
        path: probe,
        available_bytes: stat.f_bavail.saturating_mul(block_size),
        total_bytes: stat.f_blocks.saturating_mul(block_size),
        available_inodes: stat.f_favail,
        total_inodes: stat.f_files,
    })
}

pub fn ensure_disk_space(target: &Path) -> Result<DiskSpaceStats> {
    let stats = statvfs_stats(target)?;
    ensure!(
        stats.available_bytes >= GIB,
        "Insufficient free disk space at {} ({} MiB available, >= 1024 MiB required); run 'docker image prune -f'",
        stats.path.display(),
        stats.available_bytes / MIB
    );
    if stats.total_inodes > 0 {
        ensure!(
            stats.available_inodes >= 1_024,
            "Insufficient free inodes at {} ({} available, >= 1024 required); run 'docker image prune -f'",
            stats.path.display(),
            stats.available_inodes
        );
    }
    Ok(stats)
}

pub fn compose_project_name(root: &Path) -> String {
    format!(
        "devkit-{}",
        &digest(root.as_os_str().as_encoded_bytes())[..16]
    )
}

fn recorded_derived_image_ready(root: &Path, kind: &str, recipe: &str, pin: &str) -> bool {
    let Ok(recipe_path) = safe_path(root, &format!(".devkit/runtime/{recipe}")) else {
        return false;
    };
    let Ok(Some(recipe_id)) = identity(&recipe_path) else {
        return false;
    };
    if recipe_id.sha256 != pin {
        return false;
    }
    let Ok(state_path) = safe_path(root, &format!(".devkit/state/{kind}-image")) else {
        return false;
    };
    let Ok(Some(_)) = identity(&state_path) else {
        return false;
    };
    let Ok(recorded) = fs::read_to_string(&state_path) else {
        return false;
    };
    recorded
        .trim()
        .strip_prefix("sha256:")
        .is_some_and(|h| SHA256_RE.is_match(h))
}

/// Computes local workspace and derived-image readiness receipts strictly from
/// filesystem state (`0` Docker subprocess calls, `0` writes).
pub fn local_receipts(
    root: &Path,
    project_state: &ProjectState,
    node_inputs_digest: Option<&str>,
) -> Result<Value> {
    let pins = safe_path(root, ".devkit/runtime/toolchains.env")
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| ToolPins::parse(&b).ok());

    let computed_node_digest = match node_inputs_digest {
        Some(d) => Some(d.to_owned()),
        None => crate::native::admit_native_components(root, project_state)
            .ok()
            .map(|n| n.node_inputs.digest),
    };
    let node_workspace_ready = if let Some(d) = computed_node_digest {
        let expected = digest(format!("{}\n{}", project_state.release_identity, d));
        let lock_ok = safe_path(root, ".devkit/state/node-workspace.lock")
            .ok()
            .and_then(|p| {
                let id = identity(&p).ok()??;
                let _ = id;
                fs::read_to_string(p).ok()
            })
            .is_some_and(|s| s.trim() == expected);
        let nm_ok = safe_path(root, "node_modules")
            .ok()
            .and_then(|p| fs::symlink_metadata(p).ok())
            .is_some_and(|m| m.is_dir() && !m.file_type().is_symlink());
        lock_ok && nm_ok
    } else {
        false
    };

    let (rust_image_ready, desktop_image_ready) = if let Some(pins) = &pins {
        (
            recorded_derived_image_ready(root, "rust", "rust.Dockerfile", &pins.rust_recipe),
            recorded_derived_image_ready(
                root,
                "desktop",
                "desktop.Dockerfile",
                &pins.desktop_recipe,
            ),
        )
    } else {
        (false, false)
    };

    Ok(json!({
        "nodeWorkspaceReady": node_workspace_ready,
        "rustImageReady": rust_image_ready,
        "desktopImageReady": desktop_image_ready,
    }))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeServiceStatus {
    pub service: String,
    pub component: Option<String>,
    pub name: String,
    pub state: String,
    pub status: String,
    pub health: Option<String>,
    pub urls: Vec<String>,
}

fn normalize_url_host(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || matches!(
            trimmed,
            "0.0.0.0" | "::" | "[::]" | "*" | "::1" | "localhost"
        )
    {
        "127.0.0.1"
    } else {
        trimmed
    }
}

fn format_http_url(host: &str, port: u64) -> String {
    let h = normalize_url_host(host);
    if h.contains(':') && !h.starts_with('[') {
        format!("http://[{h}]:{port}")
    } else {
        format!("http://{h}:{port}")
    }
}

fn extract_urls_from_ports_string(ports: &str, urls: &mut BTreeSet<String>) {
    for segment in ports.split(',') {
        let seg = segment.trim();
        let Some((left, right)) = seg.split_once("->") else {
            continue;
        };
        if right.ends_with("/udp") || right.ends_with("/sctp") {
            continue;
        }
        if let Some((host, port_str)) = left.rsplit_once(':')
            && let Ok(port) = port_str.trim().parse::<u64>()
            && (1..=65535).contains(&port)
        {
            urls.insert(format_http_url(host, port));
        }
    }
}

pub fn parse_compose_ps_json(
    stdout: &[u8],
    service_to_component: &BTreeMap<String, String>,
) -> Result<Vec<ComposeServiceStatus>> {
    let raw = std::str::from_utf8(stdout).context("Docker Compose ps output is not UTF-8")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<Value> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).context("Cannot parse Docker Compose ps JSON array")?
    } else {
        let mut out = Vec::new();
        for line in trimmed.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let item: Value =
                serde_json::from_str(line).context("Cannot parse Docker Compose ps JSON line")?;
            out.push(item);
        }
        out
    };
    let mut services = Vec::with_capacity(items.len());
    for item in items {
        let service = item
            .get("Service")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if service.is_empty() {
            continue;
        }
        let name = item
            .get("Name")
            .or_else(|| item.get("Names"))
            .and_then(Value::as_str)
            .unwrap_or(&service)
            .trim()
            .to_owned();
        let state = item
            .get("State")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .trim()
            .to_owned();
        let status = item
            .get("Status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let health = item
            .get("Health")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let mut urls = BTreeSet::new();
        if let Some(publishers) = item.get("Publishers").and_then(Value::as_array) {
            for pub_entry in publishers {
                let proto = pub_entry
                    .get("Protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("tcp")
                    .to_ascii_lowercase();
                if proto != "tcp" && !proto.is_empty() {
                    continue;
                }
                let Some(port) = pub_entry
                    .get("PublishedPort")
                    .and_then(Value::as_u64)
                    .filter(|&p| (1..=65535).contains(&p))
                else {
                    continue;
                };
                let host = pub_entry.get("URL").and_then(Value::as_str).unwrap_or("");
                urls.insert(format_http_url(host, port));
            }
        }
        if urls.is_empty()
            && let Some(ports_str) = item.get("Ports").and_then(Value::as_str)
        {
            extract_urls_from_ports_string(ports_str, &mut urls);
        }
        services.push(ComposeServiceStatus {
            component: service_to_component.get(&service).cloned(),
            service,
            name,
            state,
            status,
            health,
            urls: urls.into_iter().collect(),
        });
    }
    services.sort_by(|a, b| (&a.service, &a.name).cmp(&(&b.service, &b.name)));
    Ok(services)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub id: &'static str,
    pub status: &'static str,
    pub summary: String,
    pub details: Value,
    pub remediation: Option<String>,
}

fn sanitized_docker() -> Command {
    let mut c = Command::new("docker");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("COMPOSE_") {
            c.env_remove(key);
        }
    }
    c
}

fn read_net_mtu(iface: &str) -> Option<u32> {
    if iface.is_empty() || iface.contains(['/', '\0', '.']) {
        return None;
    }
    fs::read_to_string(format!("/sys/class/net/{iface}/mtu"))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&m| m > 0)
}

fn detect_uplink_mtu() -> Option<(String, u32)> {
    if let Ok(routes) = fs::read_to_string("/proc/net/route") {
        for line in routes.lines().skip(1) {
            let cols: Vec<&str> = line.split_ascii_whitespace().collect();
            if cols.len() >= 2
                && cols[1] == "00000000"
                && let Some(mtu) = read_net_mtu(cols[0])
            {
                return Some((cols[0].to_owned(), mtu));
            }
        }
    }
    let entries = fs::read_dir("/sys/class/net").ok()?;
    let mut best: Option<(String, u32)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(iface) = name.to_str() else {
            continue;
        };
        if iface == "lo"
            || iface == "docker0"
            || iface.starts_with("br-")
            || iface.starts_with("veth")
        {
            continue;
        }
        if let Some(mtu) = read_net_mtu(iface)
            && best.as_ref().is_none_or(|(_, cur)| mtu < *cur)
        {
            best = Some((iface.to_owned(), mtu));
        }
    }
    best
}

pub fn run_doctor(target: &Path, runner: &ProcessRunner) -> Result<(Value, String, bool)> {
    let abs_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()?.join(target)
    };
    let resolved_target = lexical(&abs_target).unwrap_or(abs_target);
    let mut checks = Vec::with_capacity(6);

    // 1. Docker daemon & Compose >= 5.0.0
    let mut daemon_info: Option<DockerDaemonInfo> = None;
    let mut server_version: Option<String> = None;
    let mut compose_version: Option<String> = None;
    let mut docker_errors = Vec::new();
    let mut docker_remediation: Option<String> = None;

    match runner.run(
        sanitized_docker().args(["compose", "version", "--short"]),
        OutputMode::Capture(64 * 1024),
    ) {
        Ok(out) if out.code == 0 => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if let Err(err) = validate_compose_version(&ver) {
                docker_errors.push(format!("{err:#}"));
                docker_remediation = Some(
                    "Install Docker Compose >= 5.0.0 and verify with 'docker compose version --short'"
                        .into(),
                );
            }
            compose_version = Some(ver);
        }
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            docker_errors.push(if msg.is_empty() {
                format!("docker compose version failed with exit code {}", out.code)
            } else {
                format!("docker compose version failed: {msg}")
            });
            docker_remediation = Some(
                "Install the Docker Compose v5 CLI plugin and verify with 'docker compose version --short'"
                    .into(),
            );
        }
        Err(err) => {
            runner.check_cancelled()?;
            docker_errors.push(format!("Cannot invoke docker CLI: {err:#}"));
            docker_remediation = Some(
                "Install Docker Engine and Compose >= 5.0.0 on PATH, then verify with 'docker info'"
                    .into(),
            );
        }
    }

    match runner.run(
        sanitized_docker().args(["info", "--format", "{{json .}}"]),
        OutputMode::Capture(1024 * 1024),
    ) {
        Ok(out) if out.code == 0 => {
            let raw = String::from_utf8_lossy(&out.stdout);
            if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
                server_version = v
                    .get("ServerVersion")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            match DockerDaemonInfo::parse(&raw) {
                Ok(info) => daemon_info = Some(info),
                Err(err) => {
                    docker_errors.push(format!("{err:#}"));
                    if docker_remediation.is_none() {
                        docker_remediation = Some(
                            "Switch Docker context to a Linux Docker daemon and verify with 'docker info'"
                                .into(),
                        );
                    }
                }
            }
        }
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            docker_errors.push(if msg.is_empty() {
                format!("docker info failed with exit code {}", out.code)
            } else {
                format!("docker info failed: {msg}")
            });
            if docker_remediation.is_none() {
                docker_remediation =
                    Some("Start the Linux Docker daemon and verify with 'docker info'".into());
            }
        }
        Err(err) => {
            runner.check_cancelled()?;
            docker_errors.push(format!("Cannot query Docker daemon: {err:#}"));
            if docker_remediation.is_none() {
                docker_remediation =
                    Some("Start the Linux Docker daemon and verify with 'docker info'".into());
            }
        }
    }

    if docker_errors.is_empty() {
        checks.push(DoctorCheck {
            id: "docker_compose",
            status: "ok",
            summary: format!(
                "Linux Docker daemon{} and Compose {} verified",
                server_version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default(),
                compose_version.as_deref().unwrap_or("v5+")
            ),
            details: json!({
                "osType": "linux",
                "serverVersion": server_version,
                "composeVersion": compose_version,
            }),
            remediation: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "docker_compose",
            status: "fail",
            summary: docker_errors.join("; "),
            details: json!({
                "serverVersion": server_version,
                "composeVersion": compose_version,
                "errors": docker_errors,
            }),
            remediation: docker_remediation,
        });
    }

    // 2 & 3. Cgroup v2 delegation & Resource budget / overcommit
    let probe_env = ProbeEnvironment::capture_host();
    let mut override_error: Option<String> = None;
    let (budget, eval_warnings) =
        match HostResourceBudget::evaluate(&probe_env, daemon_info.clone()) {
            Ok(pair) => pair,
            Err(err) => {
                override_error = Some(format!("{err:#}"));
                let mut fallback_env = probe_env.clone();
                fallback_env.devkit_cpus = None;
                fallback_env.devkit_memory_limit = None;
                HostResourceBudget::evaluate(&fallback_env, daemon_info.clone())?
            }
        };

    let caps = budget.capabilities;
    let (cg_status, cg_summary, cg_remediation) = match caps.state {
        DelegationState::ThreadedBlocked | DelegationState::RootCgroupHasProcesses => (
            "warn",
            format!(
                "Cgroup v2 domain controllers are blocked ({:?}); Devkit will omit container --cpus/--memory limits and bound toolchains in-process",
                caps.state
            ),
            Some(
                "sudo sh -c 'mkdir -p /sys/fs/cgroup/init && xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true; echo \"+cpu +memory +pids\" > /sys/fs/cgroup/cgroup.subtree_control'"
                    .into(),
            ),
        ),
        DelegationState::RootlessUndelegated => (
            "warn",
            "Rootless Docker lacks cgroup v2 controller delegation; container CPU/memory limits are disabled"
                .into(),
            Some(
                "sudo mkdir -p /etc/systemd/system/user@.service.d && printf '[Service]\\nDelegate=cpu cpuset io memory pids\\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf >/dev/null && sudo systemctl daemon-reload"
                    .into(),
            ),
        ),
        DelegationState::Delegated if !(caps.can_cpus && caps.can_memory && caps.can_pids) => (
            "warn",
            format!(
                "Partial cgroup controller delegation (cpus={}, memory={}, pids={})",
                caps.can_cpus, caps.can_memory, caps.can_pids
            ),
            Some(
                "Enable cpu, memory, and pids controllers in your container runtime cgroup hierarchy"
                    .into(),
            ),
        ),
        _ if daemon_info.is_none() => (
            "warn",
            "Docker daemon unavailable; cgroup delegation evaluated from local sysfs only".into(),
            Some("Start the Docker daemon and rerun 'devkit doctor'".into()),
        ),
        _ => (
            "ok",
            format!("Cgroup resource controllers ready ({:?})", caps.state),
            None,
        ),
    };
    checks.push(DoctorCheck {
        id: "cgroup_delegation",
        status: cg_status,
        summary: cg_summary,
        details: serde_json::to_value(caps)?,
        remediation: cg_remediation,
    });

    let cpu_overcommit = daemon_info
        .as_ref()
        .and_then(|d| d.ncpu)
        .filter(|_| budget.hard_cpu_ceiling_millicores > 0)
        .map(|ncpu| (ncpu as f64 * 1000.0) / (budget.hard_cpu_ceiling_millicores as f64));
    let mem_overcommit = daemon_info
        .as_ref()
        .and_then(|d| d.mem_total)
        .filter(|_| budget.hard_mem_ceiling_bytes > 0)
        .map(|mem| (mem as f64) / (budget.hard_mem_ceiling_bytes as f64));
    let overcommitted =
        cpu_overcommit.is_some_and(|r| r > 1.25) || mem_overcommit.is_some_and(|r| r > 1.25);

    let (budget_status, budget_summary, budget_remediation) = if let Some(err) = &override_error {
        (
            "fail",
            err.clone(),
            Some("unset DEVKIT_CPUS DEVKIT_MEMORY_LIMIT".into()),
        )
    } else if budget.effective_mem_bytes < 512 * MIB {
        (
            "fail",
            format!(
                "Effective memory ceiling ({} MiB) is below the 512 MiB minimum for toolchain containers",
                budget.effective_mem_bytes / MIB
            ),
            Some(
                "Increase workspace memory quota to >= 2 GiB (or adjust DEVKIT_MEMORY_LIMIT)"
                    .into(),
            ),
        )
    } else if !eval_warnings.is_empty() || overcommitted || budget.budget_mem_bytes < GIB {
        let mut notes = Vec::new();
        if overcommitted {
            notes.push(format!(
                "host overcommit detected (daemon reports {:?} CPUs / {:?} MiB vs cgroup {} CPUs / {} MiB); Devkit clamped container budgets to real cgroup quota",
                daemon_info.as_ref().and_then(|d| d.ncpu),
                daemon_info.as_ref().and_then(|d| d.mem_total.map(|m| m / MIB)),
                crate::resources::format_cpus(budget.hard_cpu_ceiling_millicores),
                budget.hard_mem_ceiling_bytes / MIB
            ));
        }
        notes.extend(eval_warnings.iter().cloned());
        if notes.is_empty() {
            notes.push(format!(
                "tight memory budget ({} MiB available after daemon reserve)",
                budget.budget_mem_bytes / MIB
            ));
        }
        (
            "warn",
            notes.join("; "),
            Some(
                "Run container-heavy Make targets sequentially or tune DEVKIT_CPUS / DEVKIT_MEMORY_LIMIT"
                    .into(),
            ),
        )
    } else {
        (
            "ok",
            format!(
                "Effective budget: {} CPUs, {} MiB container memory ({} MiB ceiling, {} MiB reserve)",
                crate::resources::format_cpus(budget.effective_millicores),
                budget.budget_mem_bytes / MIB,
                budget.effective_mem_bytes / MIB,
                budget.reserve_mem_bytes / MIB
            ),
            None,
        )
    };
    checks.push(DoctorCheck {
        id: "resource_budget",
        status: budget_status,
        summary: budget_summary,
        details: json!({
            "effectiveCpus": crate::resources::format_cpus(budget.effective_millicores),
            "cpuOvercommitRatio": cpu_overcommit,
            "memoryOvercommitRatio": mem_overcommit,
            "workingSetBytes": budget.working_set_bytes,
            "warnings": eval_warnings,
            "budget": budget,
        }),
        remediation: budget_remediation,
    });

    // 4. Disk space & inodes via statvfs (never docker system df)
    match statvfs_stats(&resolved_target) {
        Ok(stats) => {
            let low_inodes_fail = stats.total_inodes > 0 && stats.available_inodes < 1_024;
            let low_inodes_warn = stats.total_inodes > 0 && stats.available_inodes < 16_384;
            let (status, summary, remediation) = if stats.available_bytes < GIB || low_inodes_fail {
                (
                    "fail",
                    format!(
                        "Critically low disk capacity at {} ({} MiB free, {} free inodes)",
                        stats.path.display(),
                        stats.available_bytes / MIB,
                        stats.available_inodes
                    ),
                    Some("docker image prune -f".into()),
                )
            } else if stats.available_bytes < 5 * GIB || low_inodes_warn {
                (
                    "warn",
                    format!(
                        "Low free disk capacity at {} ({} MiB free, {} free inodes)",
                        stats.path.display(),
                        stats.available_bytes / MIB,
                        stats.available_inodes
                    ),
                    Some("docker builder prune -f".into()),
                )
            } else {
                (
                    "ok",
                    format!(
                        "{} GiB free at {}",
                        stats.available_bytes / GIB,
                        stats.path.display()
                    ),
                    None,
                )
            };
            checks.push(DoctorCheck {
                id: "disk_space",
                status,
                summary,
                details: serde_json::to_value(stats)?,
                remediation,
            });
        }
        Err(err) => {
            checks.push(DoctorCheck {
                id: "disk_space",
                status: "warn",
                summary: format!("Cannot inspect filesystem capacity: {err:#}"),
                details: json!({ "target": resolved_target }),
                remediation: Some("df -h .".into()),
            });
        }
    }

    // 5. DinD network MTU & storage driver
    let docker0_mtu = read_net_mtu("docker0");
    let uplink = detect_uplink_mtu();
    let driver = daemon_info.as_ref().and_then(|d| d.driver.clone());
    let mtu_mismatch = match (docker0_mtu, &uplink) {
        (Some(d_mtu), Some((_, u_mtu))) => d_mtu > *u_mtu,
        _ => false,
    };
    let vfs_driver = driver.as_deref() == Some("vfs");
    let (net_status, net_summary, net_remediation) = if mtu_mismatch || vfs_driver {
        let mut issues = Vec::new();
        let mut rem = None;
        if let (true, Some(d_mtu), Some((iface, u_mtu))) = (mtu_mismatch, docker0_mtu, &uplink) {
            issues.push(format!(
                "docker0 MTU ({d_mtu}) exceeds uplink {iface} MTU ({u_mtu}); TLS/registry fetches inside DinD may stall"
            ));
            rem = Some(format!(
                "Set '\"mtu\": {u_mtu}' in /etc/docker/daemon.json and restart dockerd"
            ));
        }
        if vfs_driver {
            issues.push(
                "Docker storage driver is 'vfs' (unaccelerated full-copy layers in nested DinD)"
                    .into(),
            );
            if rem.is_none() {
                rem = Some(
                    "Mount a dedicated volume on /var/lib/docker so dockerd can use overlay2"
                        .into(),
                );
            }
        }
        ("warn", issues.join("; "), rem)
    } else {
        (
            "ok",
            format!(
                "Storage driver '{}' and bridge MTU healthy",
                driver.as_deref().unwrap_or("overlay2")
            ),
            None,
        )
    };
    checks.push(DoctorCheck {
        id: "dind_network_storage",
        status: net_status,
        summary: net_summary,
        details: json!({
            "storageDriver": driver,
            "docker0Mtu": docker0_mtu,
            "uplinkInterface": uplink.as_ref().map(|(i, _)| i),
            "uplinkMtu": uplink.as_ref().map(|(_, m)| m),
        }),
        remediation: net_remediation,
    });

    // 6. Lock-free checkout state inspection (when .devkit exists at target)
    let devkit_dir = resolved_target.join(".devkit");
    let has_devkit = fs::symlink_metadata(&devkit_dir).is_ok();
    if !has_devkit {
        checks.push(DoctorCheck {
            id: "checkout_state",
            status: "ok",
            summary: "No Devkit checkout at target path (host diagnostics completed)".into(),
            details: json!({ "present": false }),
            remediation: None,
        });
    } else {
        checks.push(inspect_checkout_lockfree(&resolved_target));
    }

    runner.check_cancelled()?;
    let overall_ok = !checks.iter().any(|c| c.status == "fail");
    let mut lines = Vec::with_capacity(checks.len() + 2);
    for c in &checks {
        let tag = match c.status {
            "ok" => "[OK]",
            "warn" => "[WARN]",
            _ => "[FAIL]",
        };
        lines.push(format!("{tag} {}: {}", c.id, c.summary));
        if let Some(rem) = &c.remediation {
            lines.push(format!("  Remediation: {rem}"));
        }
    }
    let human = lines.join("\n");
    let result = json!({
        "target": resolved_target,
        "ok": overall_ok,
        "checks": checks,
    });
    Ok((result, human, overall_ok))
}

fn inspect_checkout_lockfree(target: &Path) -> DoctorCheck {
    let root = match project_root(target) {
        Ok(r) => r,
        Err(err) => {
            return DoctorCheck {
                id: "checkout_state",
                status: "fail",
                summary: format!("Invalid checkout root: {err:#}"),
                details: json!({ "present": true }),
                remediation: Some(
                    "Verify directory permissions and symlinks on the checkout path".into(),
                ),
            };
        }
    };
    let active_owner = state::read_live_owner(&root);
    if let Err(err) = state::assert_no_pending(&root) {
        return DoctorCheck {
            id: "checkout_state",
            status: "fail",
            summary: format!("{err:#}"),
            details: json!({
                "present": true,
                "pendingUpdate": true,
                "activeLockOwner": active_owner,
            }),
            remediation: Some("devkit recover".into()),
        };
    }

    let admitted = (|| -> Result<(ProjectState, String, Value)> {
        let manifest_id = identity(&safe_path(&root, state::MANIFEST_PATH)?)?
            .context("Missing project manifest")?;
        let s = state::read_project_state(&root)?;
        state::assert_managed(&root, &s)?;
        state::verify_managed_set(&root, &s)?;
        let pins = ToolPins::parse(&fs::read(safe_path(
            &root,
            ".devkit/runtime/toolchains.env",
        )?)?)?;
        ensure!(
            pins.version == s.version,
            "Manifest version differs from accepted toolchains.env"
        );
        let full = state::admit_runtime_snapshot(&root, s, manifest_id)?;
        let receipts = local_receipts(&root, &full.state, Some(&full.native.node_inputs.digest))?;
        Ok((full.state, pins.version, receipts))
    })();

    match admitted {
        Err(err) => {
            let msg = format!("{err:#}");
            let rem = ["restore it with '", "Run '", "run '"]
                .iter()
                .find_map(|prefix| {
                    let rest = msg.split_once(prefix)?.1;
                    let cmd = rest.split_once('\'')?.0.trim();
                    (!cmd.is_empty()).then(|| cmd.to_owned())
                })
                .unwrap_or_else(|| {
                    if msg.contains("Managed file conflict") {
                        "git checkout -- .devkit && devkit sync".into()
                    } else if msg.contains("Cargo.lock") || msg.contains("Native lock") {
                        "make setup".into()
                    } else {
                        "devkit sync".into()
                    }
                });
            DoctorCheck {
                id: "checkout_state",
                status: "fail",
                summary: msg,
                details: json!({
                    "present": true,
                    "pendingUpdate": false,
                    "activeLockOwner": active_owner,
                }),
                remediation: Some(rem),
            }
        }
        Ok((project_state, version, receipts)) => {
            let need_node = project_state
                .components
                .values()
                .any(|c| c.toolchain == Toolchain::Node);
            let need_rust = project_state
                .components
                .values()
                .any(|c| c.toolchain == Toolchain::Rust);
            let need_desktop = project_state
                .components
                .values()
                .any(|c| c.tooling_profile == ToolingProfile::Electron);

            let node_ok = !need_node
                || receipts
                    .get("nodeWorkspaceReady")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let rust_ok = !need_rust
                || receipts
                    .get("rustImageReady")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let desktop_ok = !need_desktop
                || receipts
                    .get("desktopImageReady")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let ready = node_ok && rust_ok && desktop_ok;

            let (status, summary, remediation) = if let Some(owner) = &active_owner {
                (
                    "warn",
                    format!(
                        "Checkout Devkit {version} admitted; active operation '{}' owns lock (pid {})",
                        owner.operation, owner.pid
                    ),
                    None,
                )
            } else if !ready {
                (
                    "warn",
                    format!(
                        "Checkout Devkit {version} admitted, but local tool/workspace receipts are not yet prepared"
                    ),
                    Some("make setup".into()),
                )
            } else {
                (
                    "ok",
                    format!(
                        "Checkout Devkit {version} admitted ({} components ready)",
                        project_state.components.len()
                    ),
                    None,
                )
            };
            DoctorCheck {
                id: "checkout_state",
                status,
                summary,
                details: json!({
                    "present": true,
                    "version": version,
                    "componentCount": project_state.components.len(),
                    "pendingUpdate": false,
                    "activeLockOwner": active_owner,
                    "receipts": receipts,
                }),
                remediation,
            }
        }
    }
}
