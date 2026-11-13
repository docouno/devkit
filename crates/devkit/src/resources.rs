//! Container resource budgeting, cgroup v2/DinD delegation detection, and toolchain concurrency.
use crate::{Result, model::*, processes::ProcessRunner};
use anyhow::{Context, ensure};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
};

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DelegationState {
    Delegated,
    ThreadedBlocked,
    RootCgroupHasProcesses,
    RootlessUndelegated,
    RemoteDaemon,
    UnverifiedMock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CgroupCapabilities {
    pub state: DelegationState,
    pub can_cpus: bool,
    pub can_cpu_shares: bool,
    pub can_memory: bool,
    pub can_swap: bool,
    pub can_pids: bool,
}

impl CgroupCapabilities {
    pub fn all_delegated() -> Self {
        Self {
            state: DelegationState::Delegated,
            can_cpus: true,
            can_cpu_shares: true,
            can_memory: true,
            can_swap: true,
            can_pids: true,
        }
    }

    pub fn unverified_mock() -> Self {
        Self {
            state: DelegationState::UnverifiedMock,
            can_cpus: true,
            can_cpu_shares: true,
            can_memory: true,
            can_swap: true,
            can_pids: true,
        }
    }

    pub fn undelegated(state: DelegationState) -> Self {
        Self {
            state,
            can_cpus: false,
            can_cpu_shares: false,
            can_memory: false,
            can_swap: false,
            can_pids: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerDaemonInfo {
    pub os_type: String,
    pub ncpu: Option<u32>,
    pub mem_total: Option<u64>,
    pub memory_limit: bool,
    pub swap_limit: bool,
    pub cpu_cfs_quota: bool,
    pub cpu_shares: bool,
    pub pids_limit: bool,
    pub driver: Option<String>,
    pub cgroup_driver: Option<String>,
    pub cgroup_version: Option<String>,
    pub rootless: bool,
    pub is_mock_text: bool,
}

impl DockerDaemonInfo {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed == "linux" {
            return Ok(Self {
                os_type: "linux".into(),
                ncpu: None,
                mem_total: None,
                memory_limit: true,
                swap_limit: true,
                cpu_cfs_quota: true,
                cpu_shares: true,
                pids_limit: true,
                driver: None,
                cgroup_driver: None,
                cgroup_version: None,
                rootless: false,
                is_mock_text: true,
            });
        }
        let v: Value =
            serde_json::from_str(trimmed).context("Cannot parse Docker daemon info JSON")?;
        let os_type = v
            .get("OSType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        ensure!(os_type == "linux", "A Linux Docker daemon is required");
        let rootless = v
            .get("SecurityOptions")
            .and_then(Value::as_array)
            .is_some_and(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .any(|s| s.contains("rootless"))
            });
        Ok(Self {
            os_type,
            ncpu: v
                .get("NCPU")
                .and_then(Value::as_u64)
                .filter(|&n| n > 0)
                .map(|n| n.min(1024) as u32),
            mem_total: v.get("MemTotal").and_then(Value::as_u64).filter(|&m| m > 0),
            memory_limit: v
                .get("MemoryLimit")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            swap_limit: v.get("SwapLimit").and_then(Value::as_bool).unwrap_or(true),
            cpu_cfs_quota: v
                .get("CpuCfsQuota")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            cpu_shares: v.get("CPUShares").and_then(Value::as_bool).unwrap_or(true),
            pids_limit: v.get("PidsLimit").and_then(Value::as_bool).unwrap_or(true),
            driver: v.get("Driver").and_then(Value::as_str).map(str::to_owned),
            cgroup_driver: v
                .get("CgroupDriver")
                .and_then(Value::as_str)
                .map(str::to_owned),
            cgroup_version: v
                .get("CgroupVersion")
                .and_then(Value::as_str)
                .map(str::to_owned),
            rootless,
            is_mock_text: false,
        })
    }
}

pub fn query_docker_info(runner: &ProcessRunner, docker: &mut Command) -> Result<DockerDaemonInfo> {
    let output = runner.capture(docker.args(["info", "--format", "{{json .}}"]))?;
    DockerDaemonInfo::parse(output.text()?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
}

#[derive(Clone, Debug)]
pub struct ProbeEnvironment {
    pub cgroup_root: PathBuf,
    pub docker_host: Option<String>,
    pub peer_cred: std::result::Result<PeerCred, std::io::ErrorKind>,
    pub daemon_cgroup_text: Option<String>,
    pub self_cgroup_text: Option<String>,
    pub daemon_oom_score_adj: Option<i32>,
    pub affinity_cpus: u32,
    pub proc_meminfo_total: Option<u64>,
    pub proc_pids_max: Option<u64>,
    pub devkit_cpus: Option<String>,
    pub devkit_memory_limit: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostResourceBudget {
    pub capabilities: CgroupCapabilities,
    pub same_hierarchy: bool,
    pub daemon_uid: Option<u32>,
    pub daemon_oom_score_adj: Option<i32>,
    pub hard_cpu_ceiling_millicores: u32,
    pub effective_millicores: u32,
    pub hard_mem_ceiling_bytes: u64,
    pub effective_mem_bytes: u64,
    pub reserve_mem_bytes: u64,
    pub budget_mem_bytes: u64,
    pub working_set_bytes: Option<u64>,
    pub effective_pids_max: u64,
    pub cpus_override_millicores: Option<u32>,
    pub memory_limit_override_bytes: Option<u64>,
    #[serde(skip)]
    pub daemon_info: Option<DockerDaemonInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerResourceSpec {
    pub cpus_millicores: Option<u32>,
    pub cpu_shares: Option<u64>,
    pub implied_jobs: usize,
    pub target_memory_bytes: u64,
    pub memory_bytes: Option<u64>,
    pub memory_swap_bytes: Option<u64>,
    pub pids_limit: Option<u64>,
    pub tmpfs_mount: String,
    pub shm_size_bytes: Option<u64>,
    pub oom_score_adj: Option<i32>,
}

impl ContainerResourceSpec {
    pub fn docker_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(milli) = self.cpus_millicores {
            out.push("--cpus".into());
            out.push(format_cpus(milli));
        }
        if let Some(shares) = self.cpu_shares {
            out.push("--cpu-shares".into());
            out.push(shares.to_string());
        }
        if let Some(mem) = self.memory_bytes {
            out.push("--memory".into());
            out.push(format_mib(mem));
        }
        if let Some(swap) = self.memory_swap_bytes {
            out.push("--memory-swap".into());
            out.push(format_mib(swap));
        }
        if let Some(pids) = self.pids_limit {
            out.push("--pids-limit".into());
            out.push(pids.to_string());
        }
        if let Some(oom) = self.oom_score_adj {
            out.push("--oom-score-adj".into());
            out.push(oom.to_string());
        }
        out.push("--tmpfs".into());
        out.push(self.tmpfs_mount.clone());
        if let Some(shm) = self.shm_size_bytes {
            out.push("--shm-size".into());
            out.push(format_mib(shm));
        }
        out
    }

    pub fn write_docker_args(&self, command: &mut Command) {
        for arg in self.docker_args() {
            command.arg(arg);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolchainEnvPlan {
    pub vars: BTreeMap<String, String>,
}

impl ToolchainEnvPlan {
    pub fn write_docker_args(&self, command: &mut Command) {
        for (k, v) in &self.vars {
            command.args(["--env", &format!("{k}={v}")]);
        }
    }
}

pub fn format_cpus(millicores: u32) -> String {
    let whole = millicores / 1000;
    let frac = millicores % 1000;
    if frac == 0 {
        return whole.to_string();
    }
    let raw = format!("{whole}.{frac:03}");
    raw.trim_end_matches('0').trim_end_matches('.').to_owned()
}

pub fn format_mib(bytes: u64) -> String {
    format!("{}m", (bytes / MIB).max(1))
}

pub fn parse_cpus_value(var_name: &str, raw: &str) -> Result<u32> {
    let trimmed = raw.trim();
    ensure!(
        !trimmed.is_empty() && !trimmed.contains(['e', 'E', '+']),
        "Invalid {var_name} value '{raw}'; expected a decimal number in 0.1..=256"
    );
    let val: f64 = trimmed.parse().with_context(|| {
        format!("Invalid {var_name} value '{raw}'; expected a decimal number in 0.1..=256")
    })?;
    ensure!(
        val.is_finite() && (0.1..=256.0).contains(&val),
        "Invalid {var_name} value '{raw}'; expected a decimal number in 0.1..=256"
    );
    Ok((val * 1000.0).round() as u32)
}

pub fn parse_memory_bytes(var_name: &str, raw: &str) -> Result<u64> {
    let trimmed = raw.trim();
    ensure!(
        !trimmed.is_empty(),
        "Invalid {var_name} value '{raw}'; expected a byte size >= 64m (for example, 256m or 4g)"
    );
    let lower = trimmed.to_ascii_lowercase();
    let split_at = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (num_part, unit_part) = lower.split_at(split_at);
    ensure!(
        !num_part.is_empty(),
        "Invalid {var_name} value '{raw}'; expected a byte size >= 64m (for example, 256m or 4g)"
    );
    let mult: u64 = match unit_part.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => MIB,
        "g" | "gb" | "gib" => GIB,
        _ => anyhow::bail!(
            "Invalid {var_name} value '{raw}'; unsupported unit '{unit_part}' (expected b, k, m, or g)"
        ),
    };
    let bytes = if let Ok(int_val) = num_part.parse::<u64>() {
        int_val.checked_mul(mult)
    } else {
        let float_val: f64 = num_part.parse().with_context(|| {
            format!("Invalid {var_name} value '{raw}'; expected a positive byte size")
        })?;
        if !float_val.is_finite() || float_val <= 0.0 {
            None
        } else {
            let product = float_val * (mult as f64);
            (product <= (1024 * GIB) as f64).then_some(product.round() as u64)
        }
    }
    .with_context(|| format!("Invalid {var_name} value '{raw}'; byte size out of range"))?;
    ensure!(
        (64 * MIB..=1024 * GIB).contains(&bytes),
        "Invalid {var_name} value '{raw}'; expected a byte size between 64m and 1024g"
    );
    Ok(bytes)
}

pub fn is_cgroup_delegation_failure(exit_code: u8, stderr: &[u8]) -> bool {
    if !(125..=127).contains(&exit_code) {
        return false;
    }
    let text = String::from_utf8_lossy(stderr);
    [
        "cannot enter cgroupv2",
        "cgroup.subtree_control",
        "cpu.max: no such file",
        "memory.max: no such file",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn socket_peer_cred(path: &Path) -> std::result::Result<PeerCred, std::io::ErrorKind> {
    let stream = UnixStream::connect(path).map_err(|e| e.kind())?;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: stream.as_raw_fd() is an open AF_UNIX socket and cred/len match libc::ucred.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().kind());
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
    })
}

fn sched_affinity_cpus() -> u32 {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: set is properly sized for sched_getaffinity on the calling process (pid 0).
    let rc =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw mut set) };
    if rc == 0 {
        let mut count = 0u32;
        for cpu in 0..libc::CPU_SETSIZE as usize {
            // SAFETY: cpu < CPU_SETSIZE.
            if unsafe { libc::CPU_ISSET(cpu, &set) } {
                count += 1;
            }
        }
        if count > 0 {
            return count;
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(2)
        .max(1)
}

fn read_proc_meminfo_total() -> Option<u64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            if kb > 0 {
                return kb.checked_mul(1024);
            }
        }
    }
    None
}

fn parse_cpuset_count(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut count = 0u32;
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            let start: u32 = a.trim().parse().ok()?;
            let end: u32 = b.trim().parse().ok()?;
            if end >= start {
                count = count.saturating_add(end - start + 1);
            }
        } else {
            let _: u32 = part.parse().ok()?;
            count = count.saturating_add(1);
        }
    }
    (count > 0).then_some(count)
}

fn sanitize_cgroup_rel(raw: &str) -> Option<PathBuf> {
    let cleaned = raw.trim().strip_suffix(" (deleted)").unwrap_or(raw.trim());
    let stripped = cleaned.trim_start_matches('/');
    let mut out = PathBuf::new();
    for comp in Path::new(stripped).components() {
        match comp {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn parse_cgroup_v2_rel(cgroup_text: &str) -> Option<PathBuf> {
    for line in cgroup_text.lines() {
        if let Some(rel) = line.strip_prefix("0::") {
            return sanitize_cgroup_rel(rel);
        }
    }
    None
}

fn ancestor_chain(leaf: &Path, root: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut cur = if leaf.starts_with(root) {
        leaf.to_path_buf()
    } else {
        root.to_path_buf()
    };
    loop {
        chain.push(cur.clone());
        if cur == root {
            break;
        }
        match cur.parent() {
            Some(parent) if parent.starts_with(root) => cur = parent.to_path_buf(),
            _ => {
                chain.push(root.to_path_buf());
                break;
            }
        }
    }
    chain
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn parse_tokens(text: &str) -> BTreeSet<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

fn read_inactive_file_bytes(stat_path: &Path) -> u64 {
    let Ok(content) = fs::read_to_string(stat_path) else {
        return 0;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("inactive_file ")
            && let Ok(bytes) = rest.trim().parse::<u64>()
        {
            return bytes;
        }
    }
    0
}

impl ProbeEnvironment {
    pub fn capture_host() -> Self {
        let docker_host = std::env::var("DOCKER_HOST")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let unix_sock = match docker_host.as_deref() {
            None => Some(PathBuf::from("/var/run/docker.sock")),
            Some(h) => h
                .strip_prefix("unix://")
                .or_else(|| h.strip_prefix("unix:"))
                .map(PathBuf::from),
        };
        let peer_cred = match &unix_sock {
            Some(sock) => socket_peer_cred(sock),
            None => Err(std::io::ErrorKind::Unsupported),
        };
        let (daemon_cgroup_text, daemon_oom_score_adj) = match peer_cred {
            Ok(cred) if cred.pid > 0 => {
                let cg = fs::read_to_string(format!("/proc/{}/cgroup", cred.pid)).ok();
                let oom = fs::read_to_string(format!("/proc/{}/oom_score_adj", cred.pid))
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok());
                (cg, oom)
            }
            _ => (None, None),
        };
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            docker_host,
            peer_cred,
            daemon_cgroup_text,
            self_cgroup_text: fs::read_to_string("/proc/self/cgroup").ok(),
            daemon_oom_score_adj,
            affinity_cpus: sched_affinity_cpus(),
            proc_meminfo_total: read_proc_meminfo_total(),
            proc_pids_max: fs::read_to_string("/proc/sys/kernel/pid_max")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|&n| n > 0),
            devkit_cpus: std::env::var("DEVKIT_CPUS").ok(),
            devkit_memory_limit: std::env::var("DEVKIT_MEMORY_LIMIT").ok(),
        }
    }
}

impl HostResourceBudget {
    pub fn probe_with_docker(runner: &ProcessRunner, docker: &mut Command) -> Result<Self> {
        let info = query_docker_info(runner, docker)?;
        let env = ProbeEnvironment::capture_host();
        let (budget, warnings) = Self::evaluate(&env, Some(info))?;
        for w in warnings {
            eprintln!("devkit: warning: {w}");
        }
        Ok(budget)
    }

    pub fn probe_local() -> Result<Self> {
        let env = ProbeEnvironment::capture_host();
        let (budget, warnings) = Self::evaluate(&env, None)?;
        for w in warnings {
            eprintln!("devkit: warning: {w}");
        }
        Ok(budget)
    }

    pub fn evaluate(
        env: &ProbeEnvironment,
        daemon_info: Option<DockerDaemonInfo>,
    ) -> Result<(Self, Vec<String>)> {
        let mut warnings = Vec::new();
        let is_mock_text = daemon_info.as_ref().is_some_and(|d| d.is_mock_text);
        let is_remote_host = env.docker_host.as_deref().is_some_and(|h| {
            !h.trim().is_empty() && !h.starts_with("unix://") && !h.starts_with("unix:")
        });

        let daemon_caps_from_info = |state: DelegationState, info: Option<&DockerDaemonInfo>| {
            if let Some(d) = info {
                CgroupCapabilities {
                    state,
                    can_cpus: d.cpu_cfs_quota,
                    can_cpu_shares: d.cpu_shares,
                    can_memory: d.memory_limit,
                    can_swap: d.memory_limit && d.swap_limit,
                    can_pids: d.pids_limit,
                }
            } else {
                CgroupCapabilities {
                    state,
                    can_cpus: true,
                    can_cpu_shares: true,
                    can_memory: true,
                    can_swap: true,
                    can_pids: true,
                }
            }
        };

        let mut same_hierarchy = false;
        let mut daemon_uid = None;
        let mut daemon_oom = None;
        let mut cg_quota_milli: Option<u32> = None;
        let mut cg_cpuset_milli: Option<u32> = None;
        let mut hard_cg_mem: Option<u64> = None;
        let mut effective_cg_mem: Option<u64> = None;
        let mut working_set_bytes: Option<u64> = None;
        let mut cg_pids_max: Option<u64> = None;

        let capabilities = if is_mock_text {
            CgroupCapabilities::unverified_mock()
        } else if is_remote_host {
            daemon_caps_from_info(DelegationState::RemoteDaemon, daemon_info.as_ref())
        } else {
            match env.peer_cred {
                Err(_) => CgroupCapabilities::unverified_mock(),
                Ok(cred) if cred.pid <= 0 => {
                    daemon_uid = Some(cred.uid);
                    daemon_caps_from_info(DelegationState::Delegated, daemon_info.as_ref())
                }
                Ok(cred) => {
                    same_hierarchy = true;
                    daemon_uid = Some(cred.uid);
                    daemon_oom = env.daemon_oom_score_adj;
                    let cgroup_text = env
                        .daemon_cgroup_text
                        .as_deref()
                        .or(env.self_cgroup_text.as_deref())
                        .unwrap_or("0::/");
                    let is_v2 = env.cgroup_root.join("cgroup.controllers").exists();
                    if is_v2 {
                        let rel = parse_cgroup_v2_rel(cgroup_text).unwrap_or_default();
                        let candidate = if rel.as_os_str().is_empty() {
                            env.cgroup_root.clone()
                        } else {
                            env.cgroup_root.join(&rel)
                        };
                        let daemon_cg = if candidate.is_dir() {
                            candidate
                        } else {
                            env.cgroup_root.clone()
                        };
                        let docker_cg = env.cgroup_root.join("docker");

                        // Walk ancestors from daemon_cg (and docker_cg when present) up to cgroup_root.
                        let mut dirs = ancestor_chain(&daemon_cg, &env.cgroup_root);
                        if docker_cg.is_dir() && !dirs.contains(&docker_cg) {
                            dirs.insert(0, docker_cg.clone());
                        }
                        for dir in &dirs {
                            if let Some(cpu_max) = read_trimmed(&dir.join("cpu.max")) {
                                let mut parts = cpu_max.split_whitespace();
                                if let (Some(q_str), Some(p_str)) = (parts.next(), parts.next())
                                    && q_str != "max"
                                    && let (Ok(quota), Ok(period)) =
                                        (q_str.parse::<u64>(), p_str.parse::<u64>())
                                    && period > 0
                                {
                                    let milli = ((quota as u128 * 1000) / period as u128)
                                        .max(10)
                                        .min(u32::MAX as u128)
                                        as u32;
                                    cg_quota_milli =
                                        Some(cg_quota_milli.map_or(milli, |cur| cur.min(milli)));
                                }
                            }
                            let cpuset_str = read_trimmed(&dir.join("cpuset.cpus.effective"))
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    read_trimmed(&dir.join("cpuset.cpus")).filter(|s| !s.is_empty())
                                });
                            if let Some(s) = cpuset_str
                                && let Some(cnt) = parse_cpuset_count(&s)
                            {
                                let milli = cnt.saturating_mul(1000);
                                cg_cpuset_milli =
                                    Some(cg_cpuset_milli.map_or(milli, |cur| cur.min(milli)));
                            }
                            let mem_max_val = read_trimmed(&dir.join("memory.max"))
                                .filter(|s| s != "max")
                                .and_then(|s| s.parse::<u64>().ok())
                                .filter(|&v| v > 0);
                            let mem_high_val = read_trimmed(&dir.join("memory.high"))
                                .filter(|s| s != "max")
                                .and_then(|s| s.parse::<u64>().ok())
                                .filter(|&v| v > 0);
                            if let Some(m) = mem_max_val {
                                hard_cg_mem = Some(hard_cg_mem.map_or(m, |cur| cur.min(m)));
                            }
                            let dir_eff_mem = match (mem_max_val, mem_high_val) {
                                (Some(a), Some(b)) => Some(a.min(b)),
                                (Some(a), None) => Some(a),
                                (None, Some(b)) => Some(b),
                                (None, None) => None,
                            };
                            if let Some(m) = dir_eff_mem {
                                let should_sample_ws = effective_cg_mem.is_none_or(|cur| m <= cur);
                                effective_cg_mem =
                                    Some(effective_cg_mem.map_or(m, |cur| cur.min(m)));
                                if should_sample_ws
                                    && let Some(cur_bytes) =
                                        read_trimmed(&dir.join("memory.current"))
                                            .and_then(|s| s.parse::<u64>().ok())
                                {
                                    let inactive =
                                        read_inactive_file_bytes(&dir.join("memory.stat"));
                                    working_set_bytes = Some(cur_bytes.saturating_sub(inactive));
                                }
                            }
                            if let Some(p_max) = read_trimmed(&dir.join("pids.max"))
                                .filter(|s| s != "max")
                                .and_then(|s| s.parse::<u64>().ok())
                                .filter(|&v| v > 0)
                            {
                                cg_pids_max = Some(cg_pids_max.map_or(p_max, |cur| cur.min(p_max)));
                            }
                        }
                        if working_set_bytes.is_none()
                            && let Some(cur_bytes) =
                                read_trimmed(&env.cgroup_root.join("memory.current"))
                                    .and_then(|s| s.parse::<u64>().ok())
                        {
                            let inactive =
                                read_inactive_file_bytes(&env.cgroup_root.join("memory.stat"));
                            working_set_bytes = Some(cur_bytes.saturating_sub(inactive));
                        }

                        // Delegation verification on cgroup v2.
                        evaluate_v2_delegation(
                            &env.cgroup_root,
                            &daemon_cg,
                            cred.uid,
                            daemon_info.as_ref(),
                        )
                    } else {
                        // Cgroup v1 fallback ceilings & delegation.
                        evaluate_v1_ceilings(
                            &env.cgroup_root,
                            cgroup_text,
                            &mut cg_quota_milli,
                            &mut cg_cpuset_milli,
                            &mut hard_cg_mem,
                            &mut effective_cg_mem,
                            &mut cg_pids_max,
                        );
                        daemon_caps_from_info(DelegationState::Delegated, daemon_info.as_ref())
                    }
                }
            }
        };

        let daemon_ncpu_milli = daemon_info
            .as_ref()
            .and_then(|d| d.ncpu)
            .map(|n| n.saturating_mul(1000));
        let daemon_mem = daemon_info.as_ref().and_then(|d| d.mem_total);

        let hard_cpu_ceiling_millicores = if same_hierarchy {
            [
                Some(env.affinity_cpus.max(1).saturating_mul(1000)),
                cg_quota_milli,
                cg_cpuset_milli,
                daemon_ncpu_milli,
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(2000)
            .max(10)
        } else {
            daemon_ncpu_milli
                .unwrap_or_else(|| env.affinity_cpus.max(1).saturating_mul(1000))
                .max(10)
        };

        let hard_mem_ceiling_bytes = if same_hierarchy {
            [env.proc_meminfo_total, hard_cg_mem, daemon_mem]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(4 * GIB)
                .max(64 * MIB)
        } else {
            daemon_mem
                .or(env.proc_meminfo_total)
                .unwrap_or(4 * GIB)
                .max(64 * MIB)
        };

        let effective_mem_bytes = if same_hierarchy {
            [env.proc_meminfo_total, effective_cg_mem, daemon_mem]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(hard_mem_ceiling_bytes)
                .min(hard_mem_ceiling_bytes)
                .max(64 * MIB)
        } else {
            hard_mem_ceiling_bytes
        };

        let effective_pids_max = if same_hierarchy {
            [cg_pids_max, env.proc_pids_max]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(32768)
                .max(64)
        } else {
            env.proc_pids_max.unwrap_or(32768).max(64)
        };

        let cpus_override_millicores = if let Some(raw) = &env.devkit_cpus {
            let parsed = parse_cpus_value("DEVKIT_CPUS", raw)?;
            if parsed > hard_cpu_ceiling_millicores {
                warnings.push(format!(
                    "DEVKIT_CPUS={} exceeds available CPU limit ({}); clamping to {}",
                    raw.trim(),
                    format_cpus(hard_cpu_ceiling_millicores),
                    format_cpus(hard_cpu_ceiling_millicores),
                ));
                Some(hard_cpu_ceiling_millicores)
            } else {
                Some(parsed)
            }
        } else {
            None
        };

        let memory_limit_override_bytes = if let Some(raw) = &env.devkit_memory_limit {
            let parsed = parse_memory_bytes("DEVKIT_MEMORY_LIMIT", raw)?;
            if parsed > hard_mem_ceiling_bytes {
                warnings.push(format!(
                    "DEVKIT_MEMORY_LIMIT={} exceeds hard memory limit ({}); clamping to {}",
                    raw.trim(),
                    format_mib(hard_mem_ceiling_bytes),
                    format_mib(hard_mem_ceiling_bytes),
                ));
                Some(hard_mem_ceiling_bytes)
            } else {
                Some(parsed)
            }
        } else {
            None
        };

        let effective_millicores = cpus_override_millicores.unwrap_or(hard_cpu_ceiling_millicores);
        let raw_reserve = ((effective_mem_bytes as u128 * 18) / 100) as u64;
        let reserve_mem_bytes = raw_reserve
            .clamp(256 * MIB, 2048 * MIB)
            .min(effective_mem_bytes / 4);
        let budget_mem_bytes = effective_mem_bytes.saturating_sub(reserve_mem_bytes);

        Ok((
            Self {
                capabilities,
                same_hierarchy,
                daemon_uid,
                daemon_oom_score_adj: daemon_oom,
                hard_cpu_ceiling_millicores,
                effective_millicores,
                hard_mem_ceiling_bytes,
                effective_mem_bytes,
                reserve_mem_bytes,
                budget_mem_bytes,
                working_set_bytes,
                effective_pids_max,
                cpus_override_millicores,
                memory_limit_override_bytes,
                daemon_info,
            },
            warnings,
        ))
    }

    pub fn disable_cgroup_limits(&mut self) {
        self.capabilities.can_cpus = false;
        self.capabilities.can_cpu_shares = false;
        self.capabilities.can_memory = false;
        self.capabilities.can_swap = false;
        self.capabilities.can_pids = false;
    }

    pub fn task_oom_score_adj(&self) -> Option<i32> {
        if self.daemon_uid == Some(0) && self.same_hierarchy {
            let base = self.daemon_oom_score_adj.unwrap_or(0);
            Some((base.saturating_add(500)).clamp(base, 1000))
        } else {
            None
        }
    }

    pub fn dev_oom_score_adj(&self) -> Option<i32> {
        if self.daemon_uid == Some(0) && self.same_hierarchy {
            let base = self.daemon_oom_score_adj.unwrap_or(0);
            Some((base.saturating_add(300)).clamp(base, 1000))
        } else {
            None
        }
    }

    fn compute_pool_target_memory(&self, profile_cap: u64) -> u64 {
        let raw = if let Some(override_bytes) = self.memory_limit_override_bytes {
            override_bytes
        } else {
            let pool_share = if self.effective_mem_bytes <= (5 * GIB) / 2 {
                self.budget_mem_bytes
            } else {
                ((self.budget_mem_bytes as u128 * 80) / 100) as u64
            };
            pool_share.clamp(256 * MIB, profile_cap)
        };
        let clamped = raw.min(self.hard_mem_ceiling_bytes);
        ((clamped / MIB).max(1)) * MIB
    }

    fn pids_for_heavy(&self) -> u64 {
        (self.effective_pids_max / 4)
            .clamp(256, 1024)
            .min(self.effective_pids_max)
    }

    fn pids_for_node(&self) -> u64 {
        (self.effective_pids_max / 4)
            .clamp(192, 512)
            .min(self.effective_pids_max)
    }

    pub fn for_task(&self, profile: ToolingProfile) -> ContainerResourceSpec {
        let profile_cap = match profile {
            ToolingProfile::Rust => 8 * GIB,
            ToolingProfile::Electron | ToolingProfile::Web => 4 * GIB,
            ToolingProfile::Node
            | ToolingProfile::ReactLibrary
            | ToolingProfile::TsLibrary
            | ToolingProfile::ReactUi => 3 * GIB,
        };
        let target_mem = self.compute_pool_target_memory(profile_cap);
        let cpu_cap = if profile == ToolingProfile::Rust {
            16_000
        } else {
            8_000
        };
        let target_milli = if self.cpus_override_millicores.is_some() {
            self.effective_millicores
        } else {
            self.effective_millicores.min(cpu_cap)
        }
        .max(10);
        let implied_jobs = (target_milli / 1000).max(1) as usize;
        let pids = match profile {
            ToolingProfile::Rust | ToolingProfile::Electron => self.pids_for_heavy(),
            _ => self.pids_for_node(),
        };
        let tmpfs_mount = tmpfs_for_profile(profile, target_mem);
        let shm_size_bytes = matches!(profile, ToolingProfile::Electron | ToolingProfile::Web)
            .then(|| {
                let s = (target_mem / 4)
                    .clamp(64 * MIB, 256 * MIB)
                    .min(target_mem / 2);
                ((s / MIB).max(1)) * MIB
            });

        self.finish_spec(
            target_milli,
            implied_jobs,
            target_mem,
            pids,
            tmpfs_mount,
            shm_size_bytes,
            self.task_oom_score_adj(),
        )
    }

    pub fn for_npm_resolver(&self) -> ContainerResourceSpec {
        let target_mem = self.compute_pool_target_memory(3 * GIB);
        let target_milli = if self.cpus_override_millicores.is_some() {
            self.effective_millicores
        } else {
            self.effective_millicores.min(8_000)
        }
        .max(10);
        let implied_jobs = (target_milli / 1000).max(1) as usize;
        let tmpfs_size = ((target_mem / 4)
            .clamp(256 * MIB, 768 * MIB)
            .min(target_mem / 2)
            / MIB)
            .max(1);
        let tmpfs_mount = format!("/tmp:mode=1777,size={tmpfs_size}m");
        self.finish_spec(
            target_milli,
            implied_jobs,
            target_mem,
            self.pids_for_node(),
            tmpfs_mount,
            None,
            self.task_oom_score_adj(),
        )
    }

    pub fn for_npm_pack(&self) -> ContainerResourceSpec {
        let target_mem = self.compute_pool_target_memory(GIB);
        let target_milli = if self.cpus_override_millicores.is_some() {
            self.effective_millicores
        } else {
            self.effective_millicores.min(2_000)
        }
        .max(10);
        let implied_jobs = (target_milli / 1000).max(1) as usize;
        let tmpfs_size = ((target_mem / 4)
            .clamp(128 * MIB, 256 * MIB)
            .min(target_mem / 2)
            / MIB)
            .max(1);
        let tmpfs_mount = format!("/tmp:mode=1777,size={tmpfs_size}m");
        self.finish_spec(
            target_milli,
            implied_jobs,
            target_mem,
            self.pids_for_node(),
            tmpfs_mount,
            None,
            self.task_oom_score_adj(),
        )
    }

    pub fn author_flags(
        &self,
        requested_cpus_milli: u32,
        requested_mem_bytes: u64,
        requested_pids: Option<u64>,
        node_fallback: bool,
    ) -> Vec<String> {
        let target_milli = if self.cpus_override_millicores.is_some() {
            self.effective_millicores
        } else {
            requested_cpus_milli.min(self.effective_millicores)
        }
        .max(10);
        let mem_cap = self
            .memory_limit_override_bytes
            .unwrap_or_else(|| self.budget_mem_bytes.max(256 * MIB))
            .min(self.hard_mem_ceiling_bytes);
        let target_mem = ((requested_mem_bytes.min(mem_cap) / MIB).max(1)) * MIB;

        let mut args = Vec::new();
        if self.capabilities.can_cpus {
            args.push("--cpus".into());
            args.push(format_cpus(target_milli));
            if self.capabilities.can_cpu_shares {
                args.push("--cpu-shares".into());
                args.push("512".into());
            }
        }
        if self.capabilities.can_memory {
            args.push("--memory".into());
            args.push(format_mib(target_mem));
            if self.capabilities.can_swap {
                args.push("--memory-swap".into());
                args.push(format_mib(target_mem));
            }
        } else if node_fallback {
            let m = (target_mem / MIB).max(1);
            let heap_mb = ((m * 55) / 100).clamp(256, 2048);
            args.push("--env".into());
            args.push(format!("NODE_OPTIONS=--max-old-space-size={heap_mb}"));
        }
        if self.capabilities.can_pids
            && let Some(p) = requested_pids
        {
            let clamped = p.min(self.effective_pids_max).max(64);
            args.push("--pids-limit".into());
            args.push(clamped.to_string());
        }
        if let Some(oom) = self.task_oom_score_adj() {
            args.push("--oom-score-adj".into());
            args.push(oom.to_string());
        }
        args
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_spec(
        &self,
        target_milli: u32,
        implied_jobs: usize,
        target_mem: u64,
        pids: u64,
        tmpfs_mount: String,
        shm_size_bytes: Option<u64>,
        oom_score_adj: Option<i32>,
    ) -> ContainerResourceSpec {
        let caps = &self.capabilities;
        ContainerResourceSpec {
            cpus_millicores: caps.can_cpus.then_some(target_milli),
            cpu_shares: (caps.can_cpus && caps.can_cpu_shares).then_some(512),
            implied_jobs,
            target_memory_bytes: target_mem,
            memory_bytes: caps.can_memory.then_some(target_mem),
            memory_swap_bytes: (caps.can_memory && caps.can_swap).then_some(target_mem),
            pids_limit: caps.can_pids.then_some(pids),
            tmpfs_mount,
            shm_size_bytes,
            oom_score_adj,
        }
    }

    pub fn toolchain_env_for_task(
        &self,
        profile: ToolingProfile,
        spec: &ContainerResourceSpec,
    ) -> Result<ToolchainEnvPlan> {
        let devkit_jobs = std::env::var_os("DEVKIT_CARGO_BUILD_JOBS");
        let cargo_jobs = std::env::var_os("CARGO_BUILD_JOBS");
        let rust_test_threads = std::env::var_os("RUST_TEST_THREADS");
        let ci = std::env::var_os("CI");
        self.toolchain_env_for_task_with(
            profile,
            spec,
            devkit_jobs.as_deref(),
            cargo_jobs.as_deref(),
            rust_test_threads.as_deref(),
            ci.as_deref(),
        )
    }

    pub fn toolchain_env_for_task_with(
        &self,
        profile: ToolingProfile,
        spec: &ContainerResourceSpec,
        devkit_cargo_jobs: Option<&OsStr>,
        cargo_jobs: Option<&OsStr>,
        rust_test_threads: Option<&OsStr>,
        host_ci: Option<&OsStr>,
    ) -> Result<ToolchainEnvPlan> {
        let m = (spec.target_memory_bytes / MIB).max(1);
        let j = spec.implied_jobs.max(1);
        let mut vars = BTreeMap::new();
        vars.insert("RAYON_NUM_THREADS".into(), j.clamp(1, 8).to_string());

        if profile == ToolingProfile::Rust {
            let jobs = resolve_ram_aware_cargo_jobs(devkit_cargo_jobs, cargo_jobs, host_ci, j, m)?;
            let test_threads = resolve_rust_test_threads(rust_test_threads, j, m);
            vars.insert("CARGO_BUILD_JOBS".into(), jobs.to_string());
            vars.insert("RUST_TEST_THREADS".into(), test_threads.to_string());
            if !self.capabilities.can_cpus {
                vars.insert("TOKIO_WORKER_THREADS".into(), j.clamp(2, 8).to_string());
            }
        } else {
            if spec.memory_bytes.is_none() {
                let heap_mb = ((m * 55) / 100).clamp(256, 2048);
                vars.insert(
                    "NODE_OPTIONS".into(),
                    format!("--max-old-space-size={heap_mb}"),
                );
            }
            let cpu_workers = j.saturating_sub(usize::from(j > 2)).max(1);
            let mem_workers = ((m.saturating_sub(512)) / 512).max(1) as usize;
            let workers = cpu_workers.min(mem_workers).clamp(1, 4);
            let worker_heap = ((m * 65) / (100 * (workers as u64 + 1))).clamp(256, 1024);
            vars.insert("DEVKIT_TEST_WORKERS".into(), workers.to_string());
            vars.insert(
                "DEVKIT_WORKER_MAX_OLD_SPACE_SIZE".into(),
                worker_heap.to_string(),
            );
            vars.insert("ROLLDOWN_WORKER_THREADS".into(), j.clamp(1, 8).to_string());
            vars.insert("UV_THREADPOOL_SIZE".into(), j.clamp(2, 4).to_string());
            if profile == ToolingProfile::Electron {
                vars.insert("GOMAXPROCS".into(), j.clamp(1, 8).to_string());
            }
        }
        Ok(ToolchainEnvPlan { vars })
    }

    pub fn toolchain_env_for_resolver(&self, spec: &ContainerResourceSpec) -> ToolchainEnvPlan {
        let m = (spec.target_memory_bytes / MIB).max(1);
        let j = spec.implied_jobs.max(1);
        let mut vars = BTreeMap::new();
        if spec.memory_bytes.is_none() {
            let heap_mb = ((m * 55) / 100).clamp(256, 2048);
            vars.insert(
                "NODE_OPTIONS".into(),
                format!("--max-old-space-size={heap_mb}"),
            );
        }
        vars.insert("UV_THREADPOOL_SIZE".into(), j.clamp(2, 4).to_string());
        ToolchainEnvPlan { vars }
    }
}

fn tmpfs_for_profile(profile: ToolingProfile, target_mem: u64) -> String {
    let half = (target_mem / 2).max(64 * MIB);
    let (raw, exec) = match profile {
        ToolingProfile::Rust => ((target_mem / 4).clamp(256 * MIB, 1536 * MIB), true),
        ToolingProfile::Electron => ((target_mem / 3).clamp(384 * MIB, 1536 * MIB), false),
        _ => ((target_mem / 4).clamp(256 * MIB, 768 * MIB), false),
    };
    let size_mb = (raw.min(half) / MIB).max(1);
    if exec {
        format!("/tmp:mode=1777,exec,size={size_mb}m")
    } else {
        format!("/tmp:mode=1777,size={size_mb}m")
    }
}

fn evaluate_v2_delegation(
    cgroup_root: &Path,
    daemon_cg: &Path,
    uid: u32,
    daemon_info: Option<&DockerDaemonInfo>,
) -> CgroupCapabilities {
    let docker_cg = cgroup_root.join("docker");
    if let Some(cg_type) = read_trimmed(&daemon_cg.join("cgroup.type"))
        && cg_type != "domain"
    {
        return CgroupCapabilities::undelegated(DelegationState::ThreadedBlocked);
    }
    if docker_cg.is_dir() {
        if let Some(cg_type) = read_trimmed(&docker_cg.join("cgroup.type"))
            && cg_type != "domain"
        {
            return CgroupCapabilities::undelegated(DelegationState::ThreadedBlocked);
        }
        let controllers = read_trimmed(&docker_cg.join("cgroup.controllers"))
            .map(|s| parse_tokens(&s))
            .unwrap_or_default();
        if controllers.is_empty() && uid != 0 {
            return CgroupCapabilities::undelegated(DelegationState::RootlessUndelegated);
        }
        let can_cpus = controllers.contains("cpu") && daemon_info.is_none_or(|d| d.cpu_cfs_quota);
        let can_memory =
            controllers.contains("memory") && daemon_info.is_none_or(|d| d.memory_limit);
        let can_pids = controllers.contains("pids") && daemon_info.is_none_or(|d| d.pids_limit);
        let state = if !can_cpus && !can_memory {
            let root_procs_nonempty =
                read_trimmed(&cgroup_root.join("cgroup.procs")).is_some_and(|s| !s.is_empty());
            if uid == 0 && root_procs_nonempty {
                DelegationState::RootCgroupHasProcesses
            } else {
                DelegationState::Delegated
            }
        } else {
            DelegationState::Delegated
        };
        return CgroupCapabilities {
            state,
            can_cpus,
            can_cpu_shares: can_cpus && daemon_info.is_none_or(|d| d.cpu_shares),
            can_memory,
            can_swap: can_memory && daemon_info.is_none_or(|d| d.swap_limit),
            can_pids,
        };
    }

    let daemon_controllers = read_trimmed(&daemon_cg.join("cgroup.controllers"))
        .map(|s| parse_tokens(&s))
        .unwrap_or_default();
    if daemon_controllers.is_empty() && uid != 0 {
        return CgroupCapabilities::undelegated(DelegationState::RootlessUndelegated);
    }

    let is_systemd_unit = daemon_info
        .and_then(|d| d.cgroup_driver.as_deref())
        .is_some_and(|drv| drv == "systemd")
        || daemon_cg != cgroup_root
            && daemon_cg
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".service") || n.ends_with(".scope"));

    let parent_cg = if is_systemd_unit {
        daemon_cg.parent().unwrap_or(cgroup_root)
    } else if uid == 0 {
        cgroup_root
    } else {
        daemon_cg
    };

    if let Some(cg_type) = read_trimmed(&parent_cg.join("cgroup.type"))
        && cg_type != "domain"
    {
        return CgroupCapabilities::undelegated(DelegationState::ThreadedBlocked);
    }

    let avail = read_trimmed(&parent_cg.join("cgroup.controllers"))
        .map(|s| parse_tokens(&s))
        .unwrap_or_default();
    if avail.is_empty() && uid != 0 {
        return CgroupCapabilities::undelegated(DelegationState::RootlessUndelegated);
    }
    let subtree = read_trimmed(&parent_cg.join("cgroup.subtree_control"))
        .map(|s| parse_tokens(&s))
        .unwrap_or_default();
    let procs_empty = read_trimmed(&parent_cg.join("cgroup.procs")).is_none_or(|s| s.is_empty());

    let delegatable = |c: &str| avail.contains(c) && (subtree.contains(c) || procs_empty);
    let blocked_by_procs = !procs_empty
        && ["cpu", "memory", "pids"]
            .iter()
            .any(|c| avail.contains(*c) && !subtree.contains(*c));

    let can_cpus = delegatable("cpu") && daemon_info.is_none_or(|d| d.cpu_cfs_quota);
    let can_memory = delegatable("memory") && daemon_info.is_none_or(|d| d.memory_limit);
    let can_pids = delegatable("pids") && daemon_info.is_none_or(|d| d.pids_limit);

    CgroupCapabilities {
        state: if blocked_by_procs {
            DelegationState::RootCgroupHasProcesses
        } else {
            DelegationState::Delegated
        },
        can_cpus,
        can_cpu_shares: can_cpus && daemon_info.is_none_or(|d| d.cpu_shares),
        can_memory,
        can_swap: can_memory && daemon_info.is_none_or(|d| d.swap_limit),
        can_pids,
    }
}

fn evaluate_v1_ceilings(
    cgroup_root: &Path,
    cgroup_text: &str,
    cg_quota_milli: &mut Option<u32>,
    cg_cpuset_milli: &mut Option<u32>,
    hard_cg_mem: &mut Option<u64>,
    effective_cg_mem: &mut Option<u64>,
    cg_pids_max: &mut Option<u64>,
) {
    for line in cgroup_text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_id), Some(ctrls), Some(raw_rel)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(rel) = sanitize_cgroup_rel(raw_rel) else {
            continue;
        };
        for ctrl in ctrls.split(',') {
            let mount = cgroup_root.join(ctrl);
            if !mount.is_dir() {
                continue;
            }
            let leaf = if rel.as_os_str().is_empty() {
                mount.clone()
            } else {
                mount.join(&rel)
            };
            for dir in ancestor_chain(&leaf, &mount) {
                match ctrl {
                    "cpu" | "cpu,cpuacct" => {
                        if let (Some(q), Some(p)) = (
                            read_trimmed(&dir.join("cpu.cfs_quota_us"))
                                .and_then(|s| s.parse::<i64>().ok())
                                .filter(|&q| q > 0),
                            read_trimmed(&dir.join("cpu.cfs_period_us"))
                                .and_then(|s| s.parse::<u64>().ok())
                                .filter(|&p| p > 0),
                        ) {
                            let milli = ((q as u128 * 1000) / p as u128)
                                .max(10)
                                .min(u32::MAX as u128)
                                as u32;
                            *cg_quota_milli =
                                Some(cg_quota_milli.map_or(milli, |cur| cur.min(milli)));
                        }
                    }
                    "cpuset" => {
                        let s = read_trimmed(&dir.join("cpuset.effective_cpus"))
                            .filter(|v| !v.is_empty())
                            .or_else(|| {
                                read_trimmed(&dir.join("cpuset.cpus")).filter(|v| !v.is_empty())
                            });
                        if let Some(s) = s
                            && let Some(cnt) = parse_cpuset_count(&s)
                        {
                            let milli = cnt.saturating_mul(1000);
                            *cg_cpuset_milli =
                                Some(cg_cpuset_milli.map_or(milli, |cur| cur.min(milli)));
                        }
                    }
                    "memory" => {
                        if let Some(limit) = read_trimmed(&dir.join("memory.limit_in_bytes"))
                            .and_then(|s| s.parse::<u64>().ok())
                            .filter(|&v| v > 0 && v < (1u64 << 60))
                        {
                            *hard_cg_mem = Some(hard_cg_mem.map_or(limit, |cur| cur.min(limit)));
                            *effective_cg_mem =
                                Some(effective_cg_mem.map_or(limit, |cur| cur.min(limit)));
                        }
                    }
                    "pids" => {
                        if let Some(p) = read_trimmed(&dir.join("pids.max"))
                            .filter(|s| s != "max")
                            .and_then(|s| s.parse::<u64>().ok())
                            .filter(|&v| v > 0)
                        {
                            *cg_pids_max = Some(cg_pids_max.map_or(p, |cur| cur.min(p)));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

pub fn parse_positive_jobs_env(var_name: &str, raw: &OsStr) -> Result<usize> {
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

pub fn resolve_ram_aware_cargo_jobs(
    devkit_jobs: Option<&OsStr>,
    cargo_jobs: Option<&OsStr>,
    host_ci: Option<&OsStr>,
    implied_jobs: usize,
    target_mem_mb: u64,
) -> Result<usize> {
    if let Some(raw) = devkit_jobs {
        return parse_positive_jobs_env("DEVKIT_CARGO_BUILD_JOBS", raw);
    }
    if let Some(raw) = cargo_jobs {
        return parse_positive_jobs_env("CARGO_BUILD_JOBS", raw);
    }
    let is_ci = host_ci
        .and_then(|v| v.to_str())
        .is_some_and(|v| matches!(v.trim(), "true" | "1"));
    let ram_cap = ((target_mem_mb / 900).max(1)) as usize;
    let max_cap = if is_ci { 4 } else { 8 };
    Ok(implied_jobs.max(1).min(ram_cap).clamp(1, max_cap))
}

pub fn resolve_rust_test_threads(
    host_threads: Option<&OsStr>,
    implied_jobs: usize,
    target_mem_mb: u64,
) -> usize {
    if let Some(valid) = host_threads
        .and_then(|v| v.to_str())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| (1..=256).contains(n))
    {
        return valid;
    }
    let ram_cap = ((target_mem_mb / 512).max(1)) as usize;
    implied_jobs.max(1).min(ram_cap).clamp(1, 8)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ComposeServiceResourceOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_shares: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memswap_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_score_adj: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shm_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmpfs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<BTreeMap<String, String>>,
}

impl ComposeServiceResourceOverlay {
    pub fn is_empty(&self) -> bool {
        self.cpus.is_none()
            && self.cpu_shares.is_none()
            && self.mem_limit.is_none()
            && self.memswap_limit.is_none()
            && self.pids_limit.is_none()
            && self.oom_score_adj.is_none()
            && self.shm_size.is_none()
            && self.tmpfs.is_none()
            && self.environment.is_none()
    }
}

fn compose_env_has_key(existing_env: Option<&Value>, key: &str) -> bool {
    match existing_env {
        Some(Value::Object(map)) => map.contains_key(key),
        Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_str).any(|entry| {
            entry == key
                || entry
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('='))
        }),
        _ => false,
    }
}

fn app_service_name<'a>(path: &'a str, c: &'a ComponentState) -> &'a str {
    c.development
        .as_ref()
        .and_then(|d| d.service.as_deref())
        .unwrap_or_else(|| path.rsplit('/').next().expect("component basename"))
}

pub fn build_compose_resource_overlay(
    budget: &HostResourceBudget,
    state: &ProjectState,
    rust_version: &str,
    model: &Value,
    active_services: &BTreeSet<String>,
) -> Result<Option<Value>> {
    let devkit_jobs = std::env::var_os("DEVKIT_CARGO_BUILD_JOBS");
    let cargo_jobs = std::env::var_os("CARGO_BUILD_JOBS");
    let ci = std::env::var_os("CI");
    build_compose_resource_overlay_with(
        budget,
        state,
        rust_version,
        model,
        active_services,
        devkit_jobs.as_deref(),
        cargo_jobs.as_deref(),
        ci.as_deref(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_compose_resource_overlay_with(
    budget: &HostResourceBudget,
    state: &ProjectState,
    rust_version: &str,
    model: &Value,
    active_services: &BTreeSet<String>,
    devkit_cargo_jobs: Option<&OsStr>,
    cargo_jobs: Option<&OsStr>,
    host_ci: Option<&OsStr>,
) -> Result<Option<Value>> {
    if active_services.is_empty() {
        return Ok(None);
    }
    let Some(services_obj) = model.get("services").and_then(Value::as_object) else {
        return Ok(None);
    };

    let mut managed_by_service: BTreeMap<&str, &ComponentState> = BTreeMap::new();
    for (path, c) in &state.components {
        if c.role == Role::App {
            managed_by_service.insert(app_service_name(path, c), c);
        }
    }

    struct Entry<'a> {
        name: &'a str,
        existing: &'a Value,
        managed_component: Option<&'a ComponentState>,
        weight: u64,
        mem_cap: u64,
        cpu_cap_milli: u32,
        heavy_pids: bool,
    }

    let mut entries = Vec::new();
    for svc_name in active_services {
        let Some(existing) = services_obj.get(svc_name) else {
            continue;
        };
        let comp = managed_by_service.get(svc_name.as_str()).copied();
        let is_managed_owner = comp
            .and_then(|c| c.development.as_ref())
            .is_some_and(|d| d.owner == DevelopmentOwner::Managed);
        let managed_component = is_managed_owner.then_some(comp).flatten();
        let (weight, mem_cap, cpu_cap_milli, heavy_pids) = match managed_component {
            Some(c) if c.tooling_profile == ToolingProfile::Rust => (3, 8 * GIB, 16_000, true),
            Some(c) if c.tooling_profile == ToolingProfile::Electron => (3, 4 * GIB, 8_000, true),
            Some(c) if c.tooling_profile == ToolingProfile::Web => (2, 4 * GIB, 8_000, false),
            Some(_) => (2, 3 * GIB, 8_000, false),
            None => (2, 4 * GIB, 8_000, false),
        };
        entries.push(Entry {
            name: svc_name.as_str(),
            existing,
            managed_component,
            weight,
            mem_cap,
            cpu_cap_milli,
            heavy_pids,
        });
    }
    if entries.is_empty() {
        return Ok(None);
    }

    let k = entries.len();
    let total_weight: u64 = entries.iter().map(|e| e.weight).sum::<u64>().max(1);
    let compose_pool = budget
        .memory_limit_override_bytes
        .unwrap_or_else(|| ((budget.budget_mem_bytes as u128 * 85) / 100) as u64)
        .min(budget.hard_mem_ceiling_bytes)
        .max(256 * MIB);
    let (burst_num, burst_den): (u128, u128) = if k >= 2 { (3, 2) } else { (2, 2) };
    let caps = &budget.capabilities;

    let mut overlay_services = serde_json::Map::new();
    for entry in entries {
        let base_mem = (compose_pool as u128 * entry.weight as u128) / total_weight as u128;
        let bursted_mem = ((base_mem * burst_num) / burst_den)
            .min(compose_pool as u128)
            .min(budget.hard_mem_ceiling_bytes as u128) as u64;
        let target_mem = ((bursted_mem
            .clamp(256 * MIB, entry.mem_cap)
            .min(budget.hard_mem_ceiling_bytes))
            / MIB)
            .max(1)
            * MIB;

        let base_cpu =
            (budget.effective_millicores as u128 * entry.weight as u128) / total_weight as u128;
        let bursted_cpu =
            ((base_cpu * burst_num) / burst_den).min(budget.effective_millicores as u128) as u32;
        let target_milli = if budget.cpus_override_millicores.is_some() {
            bursted_cpu
        } else {
            bursted_cpu.min(entry.cpu_cap_milli)
        }
        .max(100)
        .min(budget.hard_cpu_ceiling_millicores);
        let implied_jobs = (target_milli / 1000).max(1) as usize;

        let ex = entry.existing;
        let has_cpus =
            ex.get("cpus").is_some() || ex.pointer("/deploy/resources/limits/cpus").is_some();
        let has_cpu_shares = ex.get("cpu_shares").is_some();
        let has_mem = ex.get("mem_limit").is_some()
            || ex.pointer("/deploy/resources/limits/memory").is_some();
        let has_memswap = ex.get("memswap_limit").is_some();
        let has_pids =
            ex.get("pids_limit").is_some() || ex.pointer("/deploy/resources/limits/pids").is_some();
        let has_oom = ex.get("oom_score_adj").is_some();
        let has_tmpfs = ex.get("tmpfs").is_some();
        let has_shm = ex.get("shm_size").is_some();

        let mut svc_overlay = ComposeServiceResourceOverlay {
            cpus: (caps.can_cpus && !has_cpus).then_some((target_milli as f64) / 1000.0),
            cpu_shares: (caps.can_cpus && caps.can_cpu_shares && !has_cpu_shares).then_some(512),
            mem_limit: (caps.can_memory && !has_mem).then(|| format_mib(target_mem)),
            memswap_limit: (caps.can_memory && caps.can_swap && !has_mem && !has_memswap)
                .then(|| format_mib(target_mem)),
            pids_limit: (caps.can_pids && !has_pids).then(|| {
                if entry.heavy_pids {
                    budget.pids_for_heavy()
                } else {
                    budget.pids_for_node()
                }
            }),
            oom_score_adj: (!has_oom).then(|| budget.dev_oom_score_adj()).flatten(),
            shm_size: None,
            tmpfs: None,
            environment: None,
        };

        if let Some(comp) = entry.managed_component {
            if !has_tmpfs {
                svc_overlay.tmpfs = Some(vec![tmpfs_for_profile(comp.tooling_profile, target_mem)]);
            }
            if !has_shm && comp.tooling_profile == ToolingProfile::Web {
                let shm = (target_mem / 4)
                    .clamp(64 * MIB, 256 * MIB)
                    .min(target_mem / 2);
                svc_overlay.shm_size = Some(format_mib(shm));
            }

            let existing_env = ex.get("environment");
            let m = (target_mem / MIB).max(1);
            let mut env_map = BTreeMap::new();
            let mut insert_if_absent = |k: &str, v: String| {
                if !compose_env_has_key(existing_env, k) {
                    env_map.insert(k.to_owned(), v);
                }
            };

            insert_if_absent("RAYON_NUM_THREADS", implied_jobs.clamp(1, 8).to_string());
            if comp.toolchain == Toolchain::Rust {
                let jobs = resolve_ram_aware_cargo_jobs(
                    devkit_cargo_jobs,
                    cargo_jobs,
                    host_ci,
                    implied_jobs,
                    m,
                )?;
                insert_if_absent("CARGO_BUILD_JOBS", jobs.to_string());
                insert_if_absent("DEVKIT_RUST_VERSION", rust_version.to_owned());
                if !caps.can_cpus {
                    insert_if_absent("TOKIO_WORKER_THREADS", implied_jobs.clamp(2, 8).to_string());
                }
            } else {
                if !caps.can_memory && !has_mem {
                    let heap_mb = ((m * 55) / 100).clamp(256, 2048);
                    insert_if_absent("NODE_OPTIONS", format!("--max-old-space-size={heap_mb}"));
                }
                insert_if_absent(
                    "ROLLDOWN_WORKER_THREADS",
                    implied_jobs.clamp(1, 8).to_string(),
                );
                insert_if_absent("UV_THREADPOOL_SIZE", implied_jobs.clamp(2, 4).to_string());
            }
            if !env_map.is_empty() {
                svc_overlay.environment = Some(env_map);
            }
        }

        if !svc_overlay.is_empty() {
            overlay_services.insert(entry.name.to_owned(), serde_json::to_value(svc_overlay)?);
        }
    }

    if overlay_services.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::json!({ "services": overlay_services })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_cg(dir: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        for (name, content) in files {
            fs::write(dir.join(name), content).unwrap();
        }
    }

    fn base_probe(cgroup_root: &Path) -> ProbeEnvironment {
        ProbeEnvironment {
            cgroup_root: cgroup_root.to_path_buf(),
            docker_host: None,
            peer_cred: Ok(PeerCred { pid: 252, uid: 0 }),
            daemon_cgroup_text: Some("0::/init\n".into()),
            self_cgroup_text: Some("0::/init\n".into()),
            daemon_oom_score_adj: Some(0),
            affinity_cpus: 16,
            proc_meminfo_total: Some(64 * GIB),
            proc_pids_max: Some(32768),
            devkit_cpus: None,
            devkit_memory_limit: None,
        }
    }

    #[test]
    fn ancestor_walk_on_evacuated_init_cgroup_detects_root_quota_and_delegation() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = tmp.path();
        write_cg(
            cg,
            &[
                ("cgroup.controllers", "cpuset cpu io memory pids\n"),
                ("cgroup.subtree_control", "cpuset cpu io memory pids\n"),
                ("cgroup.procs", ""),
                ("cpu.max", "400000 100000\n"),
                ("memory.max", &(8 * GIB).to_string()),
                ("memory.current", &(2 * GIB).to_string()),
                ("memory.stat", "inactive_file 536870912\n"),
                ("pids.max", "4096\n"),
            ],
        );
        write_cg(
            &cg.join("init"),
            &[
                ("cgroup.type", "domain\n"),
                ("cgroup.controllers", "cpuset cpu io memory pids\n"),
                ("cgroup.subtree_control", ""),
                ("cgroup.procs", "1\n252\n"),
                ("cpu.max", "max 100000\n"),
                ("memory.max", "max\n"),
            ],
        );
        write_cg(
            &cg.join("docker"),
            &[
                ("cgroup.type", "domain\n"),
                ("cgroup.controllers", "cpuset cpu io memory pids\n"),
            ],
        );

        let env = base_probe(cg);
        let (budget, warnings) = HostResourceBudget::evaluate(&env, None).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(budget.capabilities.state, DelegationState::Delegated);
        assert!(budget.capabilities.can_cpus);
        assert!(budget.capabilities.can_memory);
        assert!(budget.capabilities.can_pids);
        assert_eq!(budget.effective_millicores, 4000);
        assert_eq!(budget.effective_mem_bytes, 8 * GIB);
        assert_eq!(budget.working_set_bytes, Some(2 * GIB - 512 * MIB));

        let node_spec = budget.for_task(ToolingProfile::Web);
        let args = node_spec.docker_args();
        assert!(args.windows(2).any(|w| w == ["--cpus", "4"]));
        assert!(args.windows(2).any(|w| w == ["--cpu-shares", "512"]));
        assert!(args.windows(2).any(|w| w[0] == "--memory"));
        assert!(args.windows(2).any(|w| w[0] == "--shm-size"));
        assert!(args.windows(2).any(|w| w == ["--oom-score-adj", "500"]));
        let plan = budget
            .toolchain_env_for_task_with(ToolingProfile::Web, &node_spec, None, None, None, None)
            .unwrap();
        assert!(!plan.vars.contains_key("NODE_OPTIONS"));
        assert_eq!(plan.vars.get("DEVKIT_TEST_WORKERS").unwrap(), "3");
    }

    #[test]
    fn fractional_cpu_quotas_preserve_decimal_cpus_and_implied_jobs_floor() {
        for (quota_line, expected_cpus) in [("50000 100000\n", "0.5"), ("150000 100000\n", "1.5")] {
            let tmp = tempfile::tempdir().unwrap();
            let cg = tmp.path();
            write_cg(
                cg,
                &[
                    ("cgroup.controllers", "cpu memory pids\n"),
                    ("cgroup.subtree_control", "cpu memory pids\n"),
                    ("cgroup.procs", ""),
                    ("cpu.max", quota_line),
                    ("memory.max", &(4 * GIB).to_string()),
                ],
            );
            let env = base_probe(cg);
            let (budget, _) = HostResourceBudget::evaluate(&env, None).unwrap();
            let spec = budget.for_task(ToolingProfile::Node);
            assert_eq!(spec.implied_jobs, 1);
            let args = spec.docker_args();
            assert!(
                args.windows(2).any(|w| w == ["--cpus", expected_cpus]),
                "expected --cpus {expected_cpus}, got {args:?}"
            );
        }
    }

    #[test]
    fn undelegated_cgroups_omit_resource_flags_enable_fallbacks_and_emit_zero_nulls_in_compose() {
        for mode in ["threaded", "root-procs", "rootless"] {
            let tmp = tempfile::tempdir().unwrap();
            let cg = tmp.path();
            let mut env = base_probe(cg);
            match mode {
                "threaded" => {
                    write_cg(
                        cg,
                        &[
                            ("cgroup.controllers", "cpu memory pids\n"),
                            ("cpu.max", "100000 100000\n"),
                            ("memory.max", &(4 * GIB).to_string()),
                        ],
                    );
                    write_cg(
                        &cg.join("docker"),
                        &[
                            ("cgroup.type", "domain invalid\n"),
                            ("cgroup.controllers", "cpu memory\n"),
                        ],
                    );
                }
                "root-procs" => {
                    env.daemon_cgroup_text = Some("0::/\n".into());
                    write_cg(
                        cg,
                        &[
                            ("cgroup.controllers", "cpu memory pids\n"),
                            ("cgroup.subtree_control", ""),
                            ("cgroup.procs", "1\n252\n"),
                            ("cpu.max", "200000 100000\n"),
                            ("memory.max", &(4 * GIB).to_string()),
                        ],
                    );
                }
                "rootless" => {
                    env.peer_cred = Ok(PeerCred {
                        pid: 900,
                        uid: 1000,
                    });
                    env.daemon_cgroup_text = Some("0::/user.slice\n".into());
                    write_cg(cg, &[("cgroup.controllers", "cpu memory pids\n")]);
                    write_cg(
                        &cg.join("user.slice"),
                        &[("cgroup.type", "domain\n"), ("cgroup.controllers", "")],
                    );
                }
                _ => unreachable!(),
            }

            let (budget, _) = HostResourceBudget::evaluate(&env, None).unwrap();
            assert!(!budget.capabilities.can_cpus, "mode={mode}");
            assert!(!budget.capabilities.can_memory, "mode={mode}");
            assert!(!budget.capabilities.can_pids, "mode={mode}");

            let spec = budget.for_task(ToolingProfile::Electron);
            let args = spec.docker_args();
            for forbidden in [
                "--cpus",
                "--cpu-shares",
                "--memory",
                "--memory-swap",
                "--pids-limit",
            ] {
                assert!(
                    !args.iter().any(|a| a == forbidden),
                    "mode={mode} leaked {forbidden} in {args:?}"
                );
            }
            if mode == "rootless" {
                assert!(!args.iter().any(|a| a == "--oom-score-adj"));
            }

            let node_env = budget
                .toolchain_env_for_task_with(
                    ToolingProfile::Electron,
                    &spec,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
            assert!(
                node_env
                    .vars
                    .get("NODE_OPTIONS")
                    .is_some_and(|v| v.starts_with("--max-old-space-size="))
            );
            assert!(node_env.vars.contains_key("GOMAXPROCS"));

            let rust_spec = budget.for_task(ToolingProfile::Rust);
            let rust_env = budget
                .toolchain_env_for_task_with(
                    ToolingProfile::Rust,
                    &rust_spec,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
            let expected_tokio = if mode == "rootless" { "8" } else { "2" };
            assert_eq!(
                rust_env.vars.get("TOKIO_WORKER_THREADS").unwrap(),
                expected_tokio
            );

            // Verify Compose JSON has zero null values and respects desktop tmpfs/shm_size.
            let mut components = BTreeMap::new();
            components.insert(
                "apps/desktop".into(),
                ComponentState {
                    role: Role::App,
                    toolchain: Toolchain::Node,
                    tooling_profile: ToolingProfile::Electron,
                    origin: Origin {
                        template: "electron-react".into(),
                        version: "0.4.0".into(),
                        extra: BTreeMap::new(),
                    },
                    tool_dependencies: BTreeMap::new(),
                    dependencies: vec![],
                    development: Some(Development {
                        owner: DevelopmentOwner::Managed,
                        kind: DevelopmentKind::Desktop,
                        service: Some("desktop".into()),
                        api: Some("apps/api".into()),
                        extra: BTreeMap::new(),
                    }),
                    extra: BTreeMap::new(),
                },
            );
            components.insert(
                "apps/api".into(),
                ComponentState {
                    role: Role::App,
                    toolchain: Toolchain::Rust,
                    tooling_profile: ToolingProfile::Rust,
                    origin: Origin {
                        template: "rust-api".into(),
                        version: "0.4.0".into(),
                        extra: BTreeMap::new(),
                    },
                    tool_dependencies: BTreeMap::new(),
                    dependencies: vec![],
                    development: Some(Development {
                        owner: DevelopmentOwner::Managed,
                        kind: DevelopmentKind::Compose,
                        service: Some("api".into()),
                        api: None,
                        extra: BTreeMap::new(),
                    }),
                    extra: BTreeMap::new(),
                },
            );
            let state = ProjectState {
                schema_version: 2,
                version: "0.4.0".into(),
                release_identity: "0".repeat(64),
                generation: "gen".into(),
                package_mode: PackageMode::Local,
                managed: BTreeMap::new(),
                components,
                extra: BTreeMap::new(),
            };
            let model = json!({
                "services": {
                    "desktop": {
                        "tmpfs": ["/tmp:mode=1777"],
                        "shm_size": "256m"
                    },
                    "api": {},
                    "postgres": {}
                }
            });
            let active: BTreeSet<String> = ["desktop", "api", "postgres"]
                .into_iter()
                .map(str::to_owned)
                .collect();
            let overlay = build_compose_resource_overlay_with(
                &budget, &state, "1.99.1", &model, &active, None, None, None,
            )
            .unwrap()
            .unwrap();
            let serialized = serde_json::to_string(&overlay).unwrap();
            assert!(
                !serialized.contains(":null"),
                "Compose overlay must never serialize null: {serialized}"
            );
            assert!(overlay["services"]["desktop"].get("tmpfs").is_none());
            assert!(overlay["services"]["desktop"].get("shm_size").is_none());
            assert!(overlay["services"]["api"].get("tmpfs").is_some());
            assert_eq!(
                overlay["services"]["api"]["environment"]["DEVKIT_RUST_VERSION"],
                "1.99.1"
            );
            assert!(
                overlay["services"].get("postgres").is_none() || {
                    let pg = &overlay["services"]["postgres"];
                    pg.get("environment").is_none() && pg.get("tmpfs").is_none()
                }
            );
        }
    }

    #[test]
    fn overrides_and_cargo_job_priorities_clamp_and_validate() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = tmp.path();
        write_cg(
            cg,
            &[
                ("cgroup.controllers", "cpu memory pids\n"),
                ("cgroup.subtree_control", "cpu memory pids\n"),
                ("cgroup.procs", ""),
                ("cpu.max", "200000 100000\n"),
                ("memory.max", &(2 * GIB).to_string()),
            ],
        );
        let mut env = base_probe(cg);
        env.devkit_cpus = Some("8".into());
        env.devkit_memory_limit = Some("4g".into());
        let (budget, warnings) = HostResourceBudget::evaluate(&env, None).unwrap();
        assert_eq!(warnings.len(), 2);
        assert_eq!(budget.effective_millicores, 2000);
        assert_eq!(budget.memory_limit_override_bytes, Some(2 * GIB));

        env.devkit_cpus = Some("0".into());
        assert!(HostResourceBudget::evaluate(&env, None).is_err());
        env.devkit_cpus = None;
        env.devkit_memory_limit = Some("16m".into());
        assert!(HostResourceBudget::evaluate(&env, None).is_err());

        assert_eq!(
            resolve_ram_aware_cargo_jobs(
                Some(OsStr::new("6")),
                Some(OsStr::new("3")),
                Some(OsStr::new("true")),
                8,
                8192,
            )
            .unwrap(),
            6
        );
        assert_eq!(
            resolve_ram_aware_cargo_jobs(None, Some(OsStr::new("5")), None, 8, 8192).unwrap(),
            5
        );
        // 1800 MiB RAM -> 1800 / 900 = 2 jobs even with 8 CPUs!
        assert_eq!(
            resolve_ram_aware_cargo_jobs(None, None, None, 8, 1800).unwrap(),
            2
        );
    }

    #[test]
    fn cgroup_delegation_failure_matcher_recognizes_dind_errors() {
        assert!(is_cgroup_delegation_failure(
            125,
            b"docker: Error response from daemon: cannot enter cgroupv2 \"/sys/fs/cgroup/docker\" with domain controllers -- it is in threaded mode."
        ));
        assert!(is_cgroup_delegation_failure(
            126,
            b"open /sys/fs/cgroup/docker/cpu.max: no such file or directory"
        ));
        assert!(!is_cgroup_delegation_failure(1, b"cannot enter cgroupv2"));
        assert!(!is_cgroup_delegation_failure(125, b"pull access denied"));
    }
}
