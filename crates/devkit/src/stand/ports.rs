use super::epoch::{Physical, read_bounded};
use crate::{Result, files::*};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub type ActivePorts = BTreeSet<(String, u16, String, String, u16)>;

pub const RECEIPT: &str = ".devkit/state/ports.json";
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Owner {
    pub root: PathBuf,
    pub physical: Physical,
    pub nonce: String,
    pub key: String,
}
impl Owner {
    fn new(root: &Path) -> Result<Self> {
        let physical = Physical::at(root)?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let key = digest(serde_json::to_vec(&(root, &physical, &nonce))?);
        Ok(Self {
            root: root.to_owned(),
            physical,
            nonce,
            key,
        })
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.root.is_absolute()
                && GENERATION_RE.is_match(&self.nonce)
                && self.key
                    == digest(serde_json::to_vec(&(
                        &self.root,
                        &self.physical,
                        &self.nonce
                    ))?),
            "Invalid stand owner identity"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub service: String,
    pub slot: usize,
    pub target: u16,
    pub protocol: String,
    pub host_ip: String,
    pub mode: String,
    pub name: Option<String>,
    pub app_protocol: Option<String>,
    pub replicas: u16,
    pub explicit: Option<(u16, u16)>,
}
impl Binding {
    pub fn key(&self) -> String {
        digest(
            serde_json::to_vec(&(
                &self.service,
                self.name.is_none().then_some(self.slot),
                self.target,
                &self.protocol,
                &self.host_ip,
                &self.mode,
                &self.name,
            ))
            .expect("serializable binding"),
        )
    }
    fn ip(&self) -> Result<IpAddr> {
        if self.host_ip.is_empty() {
            Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        } else {
            Ok(self
                .host_ip
                .parse()
                .context("Invalid normalized port host_ip")?)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Group {
    pub binding: Binding,
    pub first: u16,
    pub last: u16,
    pub automatic: bool,
}
impl Group {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.first > 0
                && self.first <= self.last
                && self.binding.target > 0
                && self.binding.replicas > 0
                && self.last as u32 - self.first as u32 + 1 >= self.binding.replicas as u32
                && ["tcp", "udp"].contains(&self.binding.protocol.as_str()),
            "Invalid port reservation"
        );
        self.binding.ip()?;
        ensure!(
            !self.binding.service.is_empty()
                && self.binding.service.len() <= 255
                && self
                    .binding
                    .service
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "Invalid normalized Compose service identity"
        );
        Ok(())
    }
    fn conflicts(&self, other: &Self) -> Result<bool> {
        Ok(self.binding.protocol == other.binding.protocol
            && self.first <= other.last
            && other.first <= self.last
            && hosts_overlap(self.binding.ip()?, other.binding.ip()?))
    }
}

fn hosts_overlap(a: IpAddr, b: IpAddr) -> bool {
    match (a.to_canonical(), b.to_canonical()) {
        (IpAddr::V4(a), IpAddr::V4(b)) => a == b || a.is_unspecified() || b.is_unspecified(),
        (IpAddr::V6(a), IpAddr::V6(b)) => a == b || a.is_unspecified() || b.is_unspecified(),
        // A dual-stack IPv6 wildcard can also own the IPv4 listener.
        (IpAddr::V6(a), _) => a.is_unspecified(),
        (_, IpAddr::V6(b)) => b.is_unspecified(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ports {
    pub schema_version: u32,
    pub owner: Owner,
    pub cache_root: PathBuf,
    pub groups: Vec<Group>,
    pub retained: Vec<Group>,
    pub allocation_intent: bool,
    pub possible_actors: bool,
    #[serde(default)]
    pub preview_files: Vec<super::preview::HostFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_renderer: Option<super::preview::Renderer>,
    #[serde(default)]
    pub deployments: std::collections::BTreeMap<String, super::preview::Deployment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_deployment: Option<String>,
    #[serde(default)]
    pub service_desired: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub service_preview_renderer: std::collections::BTreeMap<String, super::preview::Renderer>,
}
pub struct Allocation<'a> {
    pub declarations: &'a [Binding],
    pub selected: &'a BTreeSet<String>,
    pub stopped: &'a BTreeSet<String>,
    pub primary: Option<&'a str>,
    pub seed: &'a str,
}
impl Ports {
    pub fn read(root: &Path) -> Result<Option<Self>> {
        let Some(value): Option<Self> = read_bounded(&safe_path(root, RECEIPT)?, MAX_BYTES)? else {
            return Ok(None);
        };
        ensure!(value.schema_version == 1, "Unsupported port receipt schema");
        value.owner.validate()?;
        ensure!(
            value.owner.root == root && value.owner.physical == Physical::at(root)?,
            "Port receipt belongs to another checkout incarnation"
        );
        for group in value.groups.iter().chain(&value.retained) {
            group.validate()?;
        }
        crate::cache::cache_root(Some(&value.cache_root))?;
        Ok(Some(value))
    }
    pub fn open(root: &Path, cache_root: &Path) -> Result<Self> {
        if let Some(value) = Self::read(root)? {
            ensure!(
                value.cache_root == cache_root,
                "Stand leases use {}; release them with make down cleanup=1 before changing DEVKIT_CACHE_DIR",
                value.cache_root.display()
            );
            return Ok(value);
        }
        let value = Self {
            schema_version: 1,
            owner: Owner::new(root)?,
            cache_root: cache_root.to_owned(),
            groups: Vec::new(),
            retained: Vec::new(),
            allocation_intent: false,
            possible_actors: false,
            preview_files: Vec::new(),
            preview_renderer: None,
            deployments: std::collections::BTreeMap::new(),
            desired_deployment: None,
            service_desired: std::collections::BTreeMap::new(),
            service_preview_renderer: std::collections::BTreeMap::new(),
        };
        value.save()?;
        Ok(value)
    }
    pub fn save(&self) -> Result<()> {
        ensure!(
            self.owner.physical == Physical::at(&self.owner.root)?,
            "Checkout incarnation changed during allocation"
        );
        let bytes = json_bytes(self)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Port receipt exceeds byte budget"
        );
        write_atomic(&safe_path(&self.owner.root, RECEIPT)?, &bytes, 0o644)
    }

    /// A durable local intent always precedes publication of shared claims.
    pub fn allocate(
        &mut self,
        declarations: &[Binding],
        primary: Option<&str>,
        seed: &str,
        active: &ActivePorts,
        retry: &BTreeSet<String>,
    ) -> Result<()> {
        self.allocate_with(declarations, primary, seed, active, retry, |_| Ok(()))
    }
    pub fn allocate_with(
        &mut self,
        declarations: &[Binding],
        primary: Option<&str>,
        seed: &str,
        active: &ActivePorts,
        retry: &BTreeSet<String>,
        before_claims: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let selected = declarations.iter().map(|b| b.service.clone()).collect();
        self.allocate_plan(
            Allocation {
                declarations,
                selected: &selected,
                stopped: &BTreeSet::new(),
                primary,
                seed,
            },
            active,
            retry,
            before_claims,
        )
    }
    pub fn allocate_plan(
        &mut self,
        plan: Allocation<'_>,
        active: &ActivePorts,
        retry: &BTreeSet<String>,
        before_claims: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let Allocation {
            declarations,
            selected,
            stopped,
            primary,
            seed,
        } = plan;
        ensure!(
            declarations.iter().all(|b| selected.contains(&b.service)),
            "Published binding lies outside selected services"
        );
        ensure!(declarations.len() <= 65535, "Too many published bindings");
        for binding in declarations {
            Group {
                binding: binding.clone(),
                first: 1,
                last: binding.replicas,
                automatic: true,
            }
            .validate()?;
        }
        let range = port_range(std::env::var("DEVKIT_PORT_RANGE").ok().as_deref())?;
        let lock = Registry::lock(&self.cache_root)?;
        let mut registry = Registry::read(&self.cache_root)?;
        registry.collect_stale()?;
        let mut next = Vec::new();
        let mut ordered: Vec<_> = declarations.iter().collect();
        ordered.sort_by_key(|b| (Some(b.service.as_str()) != primary, &b.service, b.slot));
        for binding in ordered {
            let key = binding.key();
            let previous = self
                .groups
                .iter()
                .chain(&self.retained)
                .find(|g| g.automatic && g.binding.key() == key);
            let count = binding.replicas as u32;
            let automatic = binding.explicit.is_none();
            let make = |first: u16, last: u16| Group {
                binding: binding.clone(),
                first,
                last,
                automatic,
            };
            let host = if binding.host_ip.is_empty() {
                String::new()
            } else {
                binding.ip()?.to_canonical().to_string()
            };
            let owned = |port| {
                active.contains(&(
                    binding.service.clone(),
                    port,
                    binding.protocol.clone(),
                    host.clone(),
                    binding.target,
                ))
            };
            let free = |g: &Group| -> Result<bool> {
                for claim in &registry.claims {
                    // A previous incarnation is never ours, even at the same path.
                    let replace_own = claim.owner.key == self.owner.key
                        && (claim.group.binding.key() == key
                            || (!automatic
                                && stopped.contains(&binding.service)
                                && claim.group.binding.service == binding.service));
                    if !replace_own && g.conflicts(&claim.group)? {
                        return Ok(false);
                    }
                }
                for other in &next {
                    if g.conflicts(other)? {
                        return Ok(false);
                    }
                }
                for port in g.first..=g.last {
                    if owned(port) {
                        continue;
                    }
                    if !socket_available(binding.ip()?, port, &binding.protocol) {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            let group = if let Some((first, last)) = binding.explicit {
                let group = make(first, last);
                group.validate()?;
                ensure!(
                    free(&group)?,
                    "Explicit port {first}-{last}/{} for {} is occupied",
                    binding.protocol,
                    binding.service
                );
                group
            } else {
                let old =
                    previous.filter(|p| !retry.contains(&key) || (p.first..=p.last).any(&owned));
                let old = old
                    .filter(|p| (p.first as u32 + count - 1) <= 65535)
                    .map(|p| make(p.first, (p.first as u32 + count - 1) as u16));
                if let Some(old) = old.filter(|g| free(g).unwrap_or(false)) {
                    old
                } else {
                    let width = range.1 as u32 - range.0 as u32 + 1;
                    ensure!(
                        count <= width,
                        "Port range has no contiguous block of {count} ports for {}",
                        binding.service
                    );
                    let candidates = width;
                    let hash = digest(
                        if Some(binding.service.as_str()) == primary && binding.slot == 0 {
                            seed.to_owned()
                        } else {
                            format!("{seed}:{}:{key}", self.owner.key)
                        },
                    );
                    let start = if Some(binding.service.as_str()) == primary && binding.slot == 0 {
                        // Match Worktrunk's hash_port candidate without requiring wt.
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        seed.hash(&mut hasher);
                        (hasher.finish() % candidates as u64) as u32
                    } else {
                        (u64::from_str_radix(&hash[..16], 16)? % candidates as u64) as u32
                    };
                    let mut selected = None;
                    for offset in 0..candidates {
                        let first = range.0 as u32 + (start + offset) % candidates;
                        if first + count - 1 > range.1 as u32 {
                            continue;
                        }
                        let group = make(first as u16, (first + count - 1) as u16);
                        if retry.contains(&key) && previous.is_some_and(|p| p.first == group.first)
                        {
                            continue;
                        }
                        if free(&group)? {
                            selected = Some(group);
                            break;
                        }
                    }
                    let group = selected.with_context(|| {
                        format!(
                            "No free contiguous block of {count} ports in {}-{} for {}",
                            range.0, range.1, binding.service
                        )
                    })?;
                    if let Some(old) = previous {
                        eprintln!(
                            "devkit: warning: {} stand port changed from {}-{} to {}-{}",
                            binding.service, old.first, old.last, group.first, group.last
                        );
                    }
                    group
                }
            };
            group.validate()?;
            next.push(group);
        }
        let mut candidate = self.clone();
        // Keep the complete prior union until a scoped observation retires it.
        for group in &self.groups {
            if !candidate.retained.contains(group) {
                candidate.retained.push(group.clone());
            }
        }
        next.extend(
            self.groups
                .iter()
                .filter(|g| !selected.contains(&g.binding.service))
                .cloned(),
        );
        candidate.groups = next;
        before_claims(&mut candidate)?;
        *self = candidate;
        self.allocation_intent = true;
        self.save()?;
        registry.claims.retain(|c| c.owner.key != self.owner.key);
        for group in self.groups.iter().chain(&self.retained) {
            if !registry
                .claims
                .iter()
                .any(|c| c.owner.key == self.owner.key && c.group == *group)
            {
                registry.claims.push(Claim {
                    owner: self.owner.clone(),
                    group: group.clone(),
                });
            }
        }
        registry.save(&self.cache_root)?;
        drop(lock);
        self.allocation_intent = false;
        self.save()
    }
    pub fn release(&self) -> Result<()> {
        super::preview::collect_files(&self.owner.root, &self.owner, &BTreeSet::new())?;
        let _lock = Registry::lock(&self.cache_root)?;
        let mut registry = Registry::read(&self.cache_root)?;
        registry.claims.retain(|c| c.owner.key != self.owner.key);
        registry.save(&self.cache_root)?;
        remove_file(&safe_path(&self.owner.root, RECEIPT)?)?;
        Ok(())
    }
    pub fn reconcile(&mut self, rows: &[super::ledger::Container]) -> Result<()> {
        super::ledger::verify_owners(rows, &self.owner)?;
        let mut retained = Vec::new();
        for old in &self.retained {
            // Explicit overrides keep the previous auto group dormant.
            let dormant = old.automatic
                && self
                    .groups
                    .iter()
                    .any(|g| !g.automatic && g.binding.key() == old.binding.key());
            let mut needed = dormant;
            for row in rows.iter().filter(|r| r.service == old.binding.service) {
                let Some(bindings) = row.configured_ports.as_object() else {
                    continue;
                };
                for (key, entries) in bindings {
                    if key != &format!("{}/{}", old.binding.target, old.binding.protocol) {
                        continue;
                    }
                    for entry in entries.as_array().into_iter().flatten() {
                        let raw = entry["HostPort"]
                            .as_str()
                            .context("Invalid configured host port")?;
                        let Some((a, b)) = interval(raw)? else {
                            continue;
                        };
                        for port in a..=b {
                            if port >= old.first
                                && port <= old.last
                                && !self.groups.iter().any(|g| {
                                    g.binding.service == old.binding.service
                                        && g.binding.protocol == old.binding.protocol
                                        && g.binding.target == old.binding.target
                                        && port >= g.first
                                        && port <= g.last
                                })
                            {
                                needed = true;
                            }
                        }
                    }
                }
            }
            if needed {
                retained.push(old.clone());
            }
        }
        let _lock = Registry::lock(&self.cache_root)?;
        let mut registry = Registry::read(&self.cache_root)?;
        self.retained = retained;
        self.allocation_intent = true;
        self.save()?;
        registry.claims.retain(|c| c.owner.key != self.owner.key);
        for group in self.groups.iter().chain(&self.retained) {
            registry.claims.push(Claim {
                owner: self.owner.clone(),
                group: group.clone(),
            });
        }
        registry.save(&self.cache_root)?;
        self.allocation_intent = false;
        self.save()
    }
}

pub fn port_range(raw: Option<&str>) -> Result<(u16, u16)> {
    let raw = raw.unwrap_or("10000-19999");
    let (a, b) = raw
        .split_once('-')
        .context("DEVKIT_PORT_RANGE must be A-B")?;
    let a: u16 = a.parse().context("Invalid DEVKIT_PORT_RANGE start")?;
    let b: u16 = b.parse().context("Invalid DEVKIT_PORT_RANGE end")?;
    ensure!(
        a > 0 && a <= b,
        "DEVKIT_PORT_RANGE must satisfy 1 <= A <= B <= 65535"
    );
    Ok((a, b))
}
pub(super) fn interval(value: &str) -> Result<Option<(u16, u16)>> {
    if value.is_empty() || value == "0" {
        return Ok(None);
    }
    let (a, b) = value.split_once('-').unwrap_or((value, value));
    let a: u16 = a.parse()?;
    let b: u16 = b.parse()?;
    ensure!(a > 0 && a <= b, "Invalid published port range");
    Ok(Some((a, b)))
}
pub fn replica_count(service: &Value) -> Result<u16> {
    let count = service
        .get("scale")
        .or_else(|| service.pointer("/deploy/replicas"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    u16::try_from(count).context("Stand replicas must be 0..65535")
}

pub fn declarations(model: &Value, active: &BTreeSet<String>) -> Result<Vec<Binding>> {
    let services = object(
        model
            .get("services")
            .context("Missing normalized services")?,
        "Compose services",
    )?;
    let mut result = Vec::new();
    for name in active {
        let service = &services[name];
        let scale = replica_count(service)?;
        if scale == 0 {
            continue;
        }
        let Some(ports) = service.get("ports") else {
            continue;
        };
        for (slot, port) in ports
            .as_array()
            .context("Invalid normalized ports")?
            .iter()
            .enumerate()
        {
            ensure!(result.len() < 65535, "Too many published bindings");
            let target = port
                .get("target")
                .and_then(Value::as_u64)
                .context("Invalid normalized port target")?;
            ensure!((1..=65535).contains(&target), "Invalid port target");
            let raw = port
                .get("published")
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default();
            let binding = Binding {
                service: name.clone(),
                slot,
                target: target as u16,
                protocol: port
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("tcp")
                    .into(),
                host_ip: port
                    .get("host_ip")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                mode: port
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("ingress")
                    .into(),
                name: port.get("name").and_then(Value::as_str).map(str::to_owned),
                app_protocol: port
                    .get("app_protocol")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                replicas: scale,
                explicit: interval(&raw)?,
            };
            binding.ip()?;
            ensure!(
                ["tcp", "udp"].contains(&binding.protocol.as_str()),
                "Unsupported port protocol"
            );
            result.push(binding);
        }
    }
    Ok(result)
}

fn socket_available(ip: IpAddr, port: u16, protocol: &str) -> bool {
    let addr = SocketAddr::new(ip, port);
    if protocol == "tcp" {
        TcpListener::bind(addr).is_ok()
    } else {
        UdpSocket::bind(addr).is_ok()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Claim {
    owner: Owner,
    group: Group,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registry {
    schema_version: u32,
    claims: Vec<Claim>,
}
impl Registry {
    fn lock(root: &Path) -> Result<File> {
        create_dir_all_durable(root)?;
        let path = safe_path(root, "port-leases.lock")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Port registry lock must be regular"
        );
        file.lock()?;
        ensure!(
            Physical::at(&path)? == Physical::of(&file.metadata()?),
            "Port registry lock replaced"
        );
        Ok(file)
    }
    fn read(root: &Path) -> Result<Self> {
        let registry = read_bounded::<Self>(&safe_path(root, "port-leases.json")?, MAX_BYTES)?
            .unwrap_or(Self {
                schema_version: 1,
                claims: Vec::new(),
            });
        ensure!(
            registry.schema_version == 1,
            "Unsupported port lease schema"
        );
        for claim in &registry.claims {
            claim.owner.validate()?;
            claim.group.validate()?;
        }
        Ok(registry)
    }
    fn save(&self, root: &Path) -> Result<()> {
        let bytes = json_bytes(self)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Port registry exceeds byte budget"
        );
        write_atomic(&safe_path(root, "port-leases.json")?, &bytes, 0o644)
    }
    fn collect_stale(&mut self) -> Result<()> {
        let mut stale = BTreeSet::new();
        let mut checked = BTreeSet::new();
        for claim in &self.claims {
            if !checked.insert(&claim.owner.key) {
                continue;
            }
            match fs::symlink_metadata(&claim.owner.root) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    stale.insert(claim.owner.key.clone());
                }
                Ok(meta) if !meta.is_dir() || Physical::of(&meta) != claim.owner.physical => {
                    stale.insert(claim.owner.key.clone());
                }
                Ok(_) => {
                    if let Some(receipt) = Ports::read(&claim.owner.root)?
                        && receipt.owner != claim.owner
                    {
                        stale.insert(claim.owner.key.clone());
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.claims.retain(|c| !stale.contains(&c.owner.key));
        Ok(())
    }
}

/// Retry only a daemon bind failure attributed to one automatic declaration.
/// An explicit or ambiguous endpoint must keep its requested address.
pub fn bind_conflicts(stderr: &[u8], groups: &[Group]) -> Result<BTreeSet<String>> {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let mut result = BTreeSet::new();
    if !message.contains("error response from daemon")
        || !(message.contains("address already in use")
            || message.contains("port is already allocated"))
    {
        return Ok(result);
    }
    static ADDRESS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
        r"(?:bind for |failed to bind host port (?:for )?|listen (?:tcp|udp)[46]? )(\[[0-9a-f:.]+\]|(?:[0-9]{1,3}\.){3}[0-9]{1,3}|[0-9a-f:]+):([0-9]{1,5})(?:/(tcp|udp))?(?:[\s:]|$)"
    ).unwrap()
    });
    for capture in ADDRESS.captures_iter(&message) {
        let ip: IpAddr = match capture[1].trim_matches(['[', ']']).parse() {
            Ok(ip) => ip,
            Err(_) => continue,
        };
        let port: u16 = match capture[2].parse() {
            Ok(port) => port,
            Err(_) => continue,
        };
        let protocol = capture.get(3).map(|m| m.as_str()).or_else(|| {
            let matched = capture.get(0)?.as_str();
            if matched.starts_with("listen tcp") {
                Some("tcp")
            } else if matched.starts_with("listen udp") {
                Some("udp")
            } else {
                None
            }
        });
        let mut matching = Vec::new();
        for group in groups {
            if port >= group.first
                && port <= group.last
                && protocol.is_none_or(|p| p == group.binding.protocol)
                && hosts_overlap(ip, group.binding.ip()?)
            {
                matching.push(group);
            }
        }
        if matching.len() != 1 || !matching[0].automatic {
            return Ok(BTreeSet::new());
        }
        result.insert(matching[0].binding.key());
    }
    Ok(result)
}
