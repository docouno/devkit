//! Preview rendering and immutable, bounded host metadata. No vendor APIs.
use super::{
    epoch::Physical,
    ports::{Group, Owner},
};
use crate::{Result, files::*};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    net::IpAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const SLOTS: [&str; 2] = [
    "/run/devkit/preview-hosts.json",
    "/devkit/preview-hosts.json",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Renderer {
    pub template: Option<String>,
    pub branch_slug: String,
}
pub fn dns_slug(raw: &str) -> String {
    let mut slug = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() {
            slug.push((byte as char).to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        return "branch".into();
    }
    if slug.len() > 63 {
        slug = format!("{}-{}", slug[..54].trim_end_matches('-'), &digest(raw)[..8]);
    }
    slug
}
impl Renderer {
    pub fn from_env(branch: &str) -> Result<Self> {
        Self::new(
            std::env::var("DEVKIT_PREVIEW_URL_TEMPLATE")
                .ok()
                .filter(|v| !v.is_empty()),
            branch,
        )
    }
    pub fn new(template: Option<String>, branch: &str) -> Result<Self> {
        let value = Self {
            template: template.filter(|s| !s.is_empty()),
            branch_slug: dns_slug(branch),
        };
        if let Some(raw) = &value.template {
            ensure!(
                raw.len() <= 65536,
                "DEVKIT_PREVIEW_URL_TEMPLATE exceeds byte budget"
            );
            let example = value.substitute(raw, 1, "x")?;
            let authority = url_authority(raw)?;
            ensure!(
                !authority.contains('*'),
                "A preview host cannot be a wildcard"
            );
            if !authority.contains("{port}") && !authority.contains("{service}") {
                url_host(&example)?;
            }
        }
        Ok(value)
    }
    pub fn render(&self, port: u16, service: &str) -> Result<Option<RenderedUrl>> {
        let Some(raw) = &self.template else {
            return Ok(None);
        };
        ensure!(port > 0, "Preview URL requires a published port");
        let url = self.substitute(raw, port, service)?;
        let host = url_host(&url)?;
        Ok(Some(RenderedUrl { url, host }))
    }
    fn substitute(&self, raw: &str, port: u16, service: &str) -> Result<String> {
        let mut result = String::new();
        let mut rest = raw;
        while let Some(at) = rest.find('{') {
            ensure!(!rest[..at].contains('}'), "Invalid preview URL braces");
            result.push_str(&rest[..at]);
            rest = &rest[at + 1..];
            let end = rest.find('}').context("Unclosed preview URL placeholder")?;
            match &rest[..end] {
                "port" => result.push_str(&port.to_string()),
                "service" => result.push_str(service),
                "branch" | "slug" => result.push_str(&self.branch_slug),
                _ => anyhow::bail!(
                    "Unknown preview URL placeholder; use port, service, branch or slug"
                ),
            }
            rest = &rest[end + 1..];
        }
        ensure!(!rest.contains('}'), "Invalid preview URL braces");
        result.push_str(rest);
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedUrl {
    pub url: String,
    pub host: String,
}

pub fn canonical_host(raw: &str) -> Result<String> {
    let raw = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw);
    if let Ok(ip) = raw.parse::<IpAddr>() {
        ensure!(
            !ip.is_unspecified(),
            "A preview host cannot be a wildcard address"
        );
    } else {
        let host = raw.to_ascii_lowercase();
        let name = host.strip_suffix('.').unwrap_or(&host);
        ensure!(
            !name.is_empty()
                && name.len() <= 253
                && host.len() <= 254
                && name.split('.').all(|label| !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')),
            "Invalid exact preview hostname; use an ASCII or IDNA hostname/IP without wildcards"
        );
    }
    // The wire must use browser URL semantics, including mapped IPv6 and the
    // numeric IPv4 forms that a DNS-only validator would otherwise preserve.
    let authority = if raw.contains(':') {
        format!("[{raw}]")
    } else {
        raw.to_owned()
    };
    let parsed = url::Url::parse(&format!("http://{authority}/"))
        .context("Invalid browser preview hostname")?;
    let host = parsed.host_str().context("Missing preview hostname")?;
    let host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    ensure!(
        !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified()),
        "A preview host cannot be a wildcard address"
    );
    Ok(host.to_owned())
}
fn url_authority(url: &str) -> Result<&str> {
    ensure!(
        url.len() <= 65536 && !url.chars().any(|c| c.is_control() || c.is_whitespace()),
        "Invalid preview URL characters or size"
    );
    let (scheme, rest) = url
        .split_once("://")
        .context("Preview URL must be absolute HTTP or HTTPS")?;
    ensure!(
        ["http", "https"].contains(&scheme.to_ascii_lowercase().as_str()),
        "Preview URL must use HTTP or HTTPS"
    );
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    ensure!(
        !authority.contains('@') && !authority.is_empty(),
        "Preview URL must have a host and no userinfo"
    );
    Ok(authority)
}
pub fn url_host(url: &str) -> Result<String> {
    let authority = url_authority(url)?;
    let host;
    let port;
    if let Some(ipv6) = authority.strip_prefix('[') {
        let end = ipv6.find(']').context("Invalid preview IPv6 authority")?;
        host = &ipv6[..end];
        ensure!(
            host.parse::<std::net::Ipv6Addr>().is_ok(),
            "Invalid preview IPv6 address"
        );
        port = &ipv6[end + 1..];
    } else {
        let (hostname, suffix) = authority
            .split_once(':')
            .map(|(a, _)| (a, &authority[a.len()..]))
            .unwrap_or((authority, ""));
        host = hostname;
        port = suffix;
    }
    if !port.is_empty() {
        let number: u16 = port
            .strip_prefix(':')
            .context("Invalid preview port")?
            .parse()
            .context("Invalid preview port")?;
        ensure!(number > 0, "Preview port must be 1..65535");
    }
    let host = canonical_host(host)?;
    let parsed = url::Url::parse(url).context("Invalid browser preview URL")?;
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "Preview URL must have no userinfo"
    );
    ensure!(parsed.port() != Some(0), "Preview port must be 1..65535");
    Ok(host)
}

pub fn local_url(host: &str, port: u16) -> String {
    let host = match host {
        "" | "0.0.0.0" | "*" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    };
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostsDocument {
    pub schema_version: u32,
    pub service: String,
    pub hosts: Vec<String>,
}
impl HostsDocument {
    pub fn render(
        service: &str,
        groups: &[Group],
        renderer: &Renderer,
    ) -> Result<(Self, Option<String>)> {
        let mut hosts = BTreeSet::new();
        let mut selected: Vec<_> = groups
            .iter()
            .filter(|g| g.binding.service == service && g.binding.protocol == "tcp")
            .collect();
        let rank = |host: &str| match host {
            "" | "0.0.0.0" | "127.0.0.1" => 0,
            "::" | "::1" => 1,
            _ => 2,
        };
        selected.sort_by_key(|g| {
            (
                g.binding.target,
                rank(&g.binding.host_ip),
                &g.binding.host_ip,
                g.first,
            )
        });
        let mut primary = None;
        for group in selected {
            for port in group.first..=group.last {
                if let Some(rendered) = renderer.render(port, service)? {
                    if primary.is_none() {
                        primary = Some(rendered.url.clone());
                    }
                    hosts.insert(rendered.host);
                }
            }
        }
        ensure!(
            hosts.len() <= 65535,
            "Preview host count exceeds uint16 port space"
        );
        let document = Self {
            schema_version: 1,
            service: service.into(),
            hosts: hosts.into_iter().collect(),
        };
        ensure!(
            json_bytes(&Self {
                schema_version: 1,
                service: service.into(),
                hosts: vec![]
            })?
            .len()
                <= 65536,
            "Preview metadata header exceeds byte budget"
        );
        Ok((document, primary))
    }
    pub fn validate(&self, service: &str) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.service == service && self.hosts.len() <= 65535,
            "Invalid preview metadata schema/service/count"
        );
        let mut previous: Option<&str> = None;
        for host in &self.hosts {
            ensure!(
                canonical_host(host)? == *host && previous.is_none_or(|p| p < host.as_str()),
                "Invalid or unsorted preview host set"
            );
            previous = Some(host);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostFile {
    pub service: String,
    pub source: PathBuf,
    pub sha256: String,
    pub slot: String,
    pub primary_url: Option<String>,
}

pub const DEPLOYMENT_LABEL: &str = "io.docouno.devkit.stand-deployment";
pub const HOST_ENV_KEYS: [&str; 2] = ["DEVKIT_ALLOWED_HOSTS", "VITE_ALLOWED_HOSTS"];
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Deployment {
    pub renderer: Renderer,
    pub groups: Vec<Group>,
    pub files: Vec<HostFile>,
    pub images: std::collections::BTreeMap<String, String>,
    pub external: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub forwarded_hosts: std::collections::BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend: Option<bool>,
}
pub fn publish_deployment(
    ports: &mut super::ports::Ports,
    frontends: &BTreeSet<String>,
    active: &BTreeSet<String>,
    renderer: &Renderer,
    proof: &super::inspection::Proof,
    model: &serde_json::Value,
) -> Result<()> {
    for service in active {
        let frontend = frontends.contains(service);
        let groups: Vec<_> = ports
            .groups
            .iter()
            .filter(|group| group.binding.service == *service)
            .cloned()
            .collect();
        let mut files = Vec::new();
        if renderer.template.is_some() {
            let (document, primary) = if frontend {
                let (document, primary) = HostsDocument::render(service, &groups, renderer)?;
                (Some(document), primary)
            } else {
                for group in groups
                    .iter()
                    .filter(|group| group.binding.protocol == "tcp")
                {
                    for port in group.first..=group.last {
                        renderer
                            .render(port, service)
                            .with_context(|| format!("Invalid preview URL for {service}"))?;
                    }
                }
                (None, None)
            };
            if let Some(document) = document.filter(|document| !document.hosts.is_empty()) {
                let file = HostFile::publish(
                    &ports.owner.root,
                    &ports.owner,
                    proof
                        .slots
                        .get(service)
                        .context("Missing safe preview slot")?,
                    &document,
                    primary,
                )?;
                if !ports.preview_files.contains(&file) {
                    ports.preview_files.push(file.clone());
                }
                files.push(file);
            }
        }
        let own = BTreeSet::from([service.clone()]);
        let inputs = if files.is_empty() {
            own
        } else {
            super::inspection::mount_dependencies(model, &own)?
        };
        let images = inputs
            .iter()
            .filter_map(|name| proof.images.get(name).map(|id| (name.clone(), id.clone())))
            .collect();
        let external = inputs
            .iter()
            .flat_map(|name| {
                model["services"][name]["volumes_from"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .filter_map(|value| super::inspection::external_provider(value.as_str()?))
            .filter_map(|(name, _)| {
                proof
                    .external
                    .get(name)
                    .map(|id| (name.to_owned(), id.clone()))
            })
            .collect();
        let forwarded_hosts = if frontend {
            HOST_ENV_KEYS
                .into_iter()
                .filter(|key| model["services"][service]["environment"].get(key).is_none())
                .map(|key| (key.to_owned(), forwarded_host(key)))
                .collect()
        } else {
            Default::default()
        };
        let deployment = Deployment {
            renderer: if frontend {
                renderer.clone()
            } else {
                Renderer::new(None, "")?
            },
            groups,
            files,
            images,
            external,
            forwarded_hosts,
            frontend: Some(frontend),
        };
        let id = digest(json_bytes(&deployment)?);
        ports.deployments.insert(id.clone(), deployment);
        ports.service_desired.insert(service.clone(), id);
    }
    ports.desired_deployment = None;
    ports.preview_renderer = None;
    Ok(())
}

fn forwarded_host(key: &str) -> Option<String> {
    std::env::var(key).ok().map(digest)
}
pub fn directory(root: &Path, owner: &Owner) -> Result<PathBuf> {
    ensure!(
        owner.root == root
            && owner.physical == Physical::at(root)?
            && SHA256_RE.is_match(&owner.key),
        "Preview metadata owner changed"
    );
    safe_path(root, &format!(".devkit/state/preview-hosts/{}", owner.key))
}
impl HostFile {
    pub fn publish(
        root: &Path,
        owner: &Owner,
        slot: &str,
        document: &HostsDocument,
        primary_url: Option<String>,
    ) -> Result<Self> {
        ensure!(SLOTS.contains(&slot), "Invalid preview metadata slot");
        document.validate(&document.service)?;
        let bytes = json_bytes(document)?;
        ensure!(
            bytes.len() as u64 <= MAX_FILE_BYTES,
            "Preview host file exceeds 32 MiB"
        );
        let sha256 = digest(&bytes);
        let directory = directory(root, owner)?;
        create_dir_all_durable(&directory)?;
        let source = safe_path(
            root,
            &format!(".devkit/state/preview-hosts/{}/{}.json", owner.key, sha256),
        )?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".preview-")
            .tempfile_in(&directory)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&source) {
            Ok(_) => sync_directory(&directory)?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let (existing, _) =
                    read_regular_file(&source, MAX_FILE_BYTES, "immutable preview hosts")?
                        .context("Missing immutable preview hosts")?;
                ensure!(existing == bytes, "Immutable preview host file conflicts");
            }
            Err(error) => return Err(error.error.into()),
        }
        Ok(Self {
            service: document.service.clone(),
            source,
            sha256,
            slot: slot.into(),
            primary_url,
        })
    }
    pub fn validate(&self, root: &Path, owner: &Owner) -> Result<()> {
        ensure!(
            SLOTS.contains(&self.slot.as_str())
                && SHA256_RE.is_match(&self.sha256)
                && self.source == directory(root, owner)?.join(format!("{}.json", self.sha256)),
            "Invalid preview file reference"
        );
        let (bytes, _) = read_regular_file(&self.source, MAX_FILE_BYTES, "preview hosts")?
            .context("Missing applied preview host file")?;
        ensure!(digest(&bytes) == self.sha256, "Preview host digest changed");
        let document: HostsDocument = serde_json::from_slice(&bytes)?;
        document.validate(&self.service)?;
        if let Some(url) = &self.primary_url {
            ensure!(
                document.hosts.binary_search(&url_host(url)?).is_ok(),
                "Preview primary is outside host set"
            );
        }
        Ok(())
    }
}

pub fn collect_files(root: &Path, owner: &Owner, retained: &BTreeSet<PathBuf>) -> Result<()> {
    let directory = directory(root, owner)?;
    if !exists(&directory)? {
        return Ok(());
    }
    ensure!(
        fs::symlink_metadata(&directory)?.is_dir(),
        "Unsafe preview metadata namespace"
    );
    let mut garbage = Vec::new();
    for (index, item) in fs::read_dir(&directory)?.enumerate() {
        ensure!(index < 4096, "Preview metadata GC exceeds file budget");
        let item = item?;
        let name = item
            .file_name()
            .to_str()
            .context("Invalid preview metadata filename")?
            .to_owned();
        let content = name
            .strip_suffix(".json")
            .is_some_and(|s| SHA256_RE.is_match(s));
        let temporary = name
            .strip_prefix(".preview-")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()));
        ensure!(
            (content || temporary) && item.file_type()?.is_file(),
            "Unknown or unsafe preview metadata file"
        );
        if !retained.contains(&item.path()) {
            garbage.push(item.path());
        }
    }
    for path in garbage {
        remove_file(&path)?;
    }
    Ok(())
}

/// Compose config JSON escapes every literal dollar for re-use as a Compose
/// file. Keep a semantic model internally and encode only on serialization.
pub fn decode_compose(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => *s = s.replace("$$", "$"),
        serde_json::Value::Array(values) => values.iter_mut().for_each(decode_compose),
        serde_json::Value::Object(values) => values.values_mut().for_each(decode_compose),
        _ => {}
    }
}
pub fn compose_model(bytes: &[u8]) -> Result<serde_json::Value> {
    let mut model = serde_json::from_slice(bytes)?;
    decode_compose(&mut model);
    Ok(model)
}

/// Normalized Compose values must not undergo a second interpolation pass.
pub fn escape_compose(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => *s = s.replace('$', "$$"),
        serde_json::Value::Array(values) => values.iter_mut().for_each(escape_compose),
        serde_json::Value::Object(values) => values.values_mut().for_each(escape_compose),
        _ => {}
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewStatus {
    pub endpoints: Vec<Endpoint>,
    pub preview_urls: Vec<String>,
    pub applied_deployment: Option<String>,
    pub desired_deployment: Option<String>,
    pub pending_restart: bool,
    pub metadata_verified: bool,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub target: u16,
    pub published: u16,
    pub protocol: String,
    pub host_ip: String,
    pub local_url: Option<String>,
    pub preview_url: Option<String>,
}
pub fn enrich_status(
    root: &Path,
    project: &str,
    runner: &crate::processes::ProcessRunner,
    services: &mut [crate::doctor::ComposeServiceStatus],
) -> Result<()> {
    let Some(ports) = super::ports::Ports::read(root)? else {
        return Ok(());
    };
    let rows = super::ledger::observe(project, runner)?;
    let renderer = Renderer::from_env(&super::branch_name(root, runner)?)?;
    enrich_observed(&ports, &rows, &renderer, services)
}

pub fn enrich_observed(
    ports: &super::ports::Ports,
    rows: &[super::ledger::Container],
    renderer: &Renderer,
    services: &mut [crate::doctor::ComposeServiceStatus],
) -> Result<()> {
    let root = &ports.owner.root;
    for service in services {
        let Some(row) = rows.iter().find(|r| {
            !service.id.is_empty()
                && r.service == service.service
                && (r.id == service.id || r.id.starts_with(&service.id))
        }) else {
            continue;
        };
        let status = &mut service.preview;
        let observed = if row.running {
            &row.ports
        } else {
            &row.configured_ports
        };
        for (target, entries) in observed.as_object().into_iter().flat_map(|v| v.iter()) {
            let (target, protocol) = target
                .split_once('/')
                .context("Invalid observed endpoint protocol")?;
            for entry in entries.as_array().into_iter().flatten() {
                let published = entry["HostPort"]
                    .as_str()
                    .context("Invalid published endpoint")?;
                let (first, last) = published.split_once('-').unwrap_or((published, published));
                let first: u16 = first.parse()?;
                let last: u16 = last.parse()?;
                ensure!(
                    first > 0 && first <= last,
                    "Invalid observed endpoint interval"
                );
                let host = entry["HostIp"].as_str().context("Invalid endpoint host")?;
                for published in first..=last {
                    status.endpoints.push(Endpoint {
                        target: target.parse()?,
                        published,
                        protocol: protocol.into(),
                        host_ip: host.into(),
                        local_url: (protocol == "tcp").then(|| local_url(host, published)),
                        preview_url: None,
                    });
                }
            }
        }
        status.desired_deployment = ports
            .service_desired
            .get(&row.service)
            .cloned()
            .or_else(|| ports.desired_deployment.clone());
        status.applied_deployment = (!row.deployment.is_empty()).then(|| row.deployment.clone());
        let proof = (|| -> Result<()> {
            ensure!(
                row.owner == ports.owner.key,
                "Container has unknown stand origin"
            );
            let deployment = ports
                .deployments
                .get(&row.deployment)
                .context("Container has no recorded applied deployment")?;
            ensure!(
                digest(json_bytes(deployment)?) == row.deployment,
                "Applied deployment receipt changed"
            );
            if let Some(image) = deployment.images.get(&row.service) {
                ensure!(
                    image == &row.image,
                    "Applied image differs from inspected image"
                );
            }
            let frontend = deployment
                .frontend
                .unwrap_or_else(|| row.preview_env.contains_key("DEVKIT_SERVICE"));
            let applied_renderer = if frontend {
                &deployment.renderer
            } else {
                ports
                    .service_preview_renderer
                    .get(&row.service)
                    .unwrap_or(&deployment.renderer)
            };
            status.pending_restart = status.desired_deployment.as_deref() != Some(&row.deployment)
                || applied_renderer != renderer
                || deployment
                    .forwarded_hosts
                    .iter()
                    .any(|(key, value)| forwarded_host(key) != *value);
            for (key, expected) in &deployment.forwarded_hosts {
                if let Some(expected) = expected {
                    ensure!(
                        row.preview_env
                            .get(key)
                            .is_some_and(|value| digest(value) == *expected),
                        "Applied host allowlist differs from recorded deployment"
                    );
                }
            }
            if frontend {
                ensure!(
                    row.preview_env
                        .get("DEVKIT_PREVIEW_HOSTS")
                        .is_none_or(String::is_empty),
                    "Applied stand retained a stale CSV host carrier"
                );
            }
            if let Some(file) = deployment.files.iter().find(|f| f.service == row.service) {
                file.validate(root, &ports.owner)?;
                ensure!(
                    deployment.images.get(&row.service) == Some(&row.image),
                    "Applied image differs from inspected image"
                );
                for (key, expected) in [
                    ("DEVKIT_PREVIEW_HOSTS_FILE", file.slot.as_str()),
                    ("DEVKIT_PREVIEW_HOSTS_SHA256", &file.sha256),
                    (
                        "DEVKIT_PREVIEW_URL",
                        file.primary_url.as_deref().unwrap_or(""),
                    ),
                    ("DEVKIT_SERVICE", file.service.as_str()),
                    ("DEVKIT_BRANCH", &deployment.renderer.branch_slug),
                ] {
                    ensure!(
                        row.preview_env.get(key).map(String::as_str) == Some(expected),
                        "Applied preview environment differs from recorded deployment"
                    );
                }
                ensure!(
                    row.mounts
                        .as_array()
                        .is_some_and(|mounts| mounts.iter().any(|m| m["Type"] == "bind"
                            && m["Source"].as_str() == file.source.to_str()
                            && m["Destination"] == file.slot
                            && m["RW"] == false)),
                    "Applied preview bind differs from recorded deployment"
                );
            }
            for endpoint in &mut status.endpoints {
                if endpoint.protocol == "tcp"
                    && deployment.groups.iter().any(|g| {
                        g.binding.service == row.service
                            && g.binding.target == endpoint.target
                            && g.binding.protocol == endpoint.protocol
                            && endpoint.published >= g.first
                            && endpoint.published <= g.last
                    })
                    && let Some(url) = applied_renderer.render(endpoint.published, &row.service)?
                {
                    endpoint.preview_url = Some(url.url.clone());
                    status.preview_urls.push(url.url);
                }
            }
            status.preview_urls.sort();
            status.preview_urls.dedup();
            status.metadata_verified = true;
            Ok(())
        })();
        if let Err(error) = proof {
            status.diagnostic = Some(format!("{error:#}"));
            status.preview_urls.clear();
            for endpoint in &mut status.endpoints {
                endpoint.preview_url = None;
            }
        }
    }
    Ok(())
}

pub fn prune(ports: &mut super::ports::Ports, rows: &[super::ledger::Container]) -> Result<()> {
    super::ledger::verify_owners(rows, &ports.owner)?;
    let mut retained: BTreeSet<_> = rows
        .iter()
        .filter(|r| !r.deployment.is_empty())
        .map(|r| r.deployment.clone())
        .collect();
    retained.extend(ports.desired_deployment.clone());
    retained.extend(ports.service_desired.values().cloned());
    ports.deployments.retain(|id, _| retained.contains(id));
    ports.preview_files = ports
        .deployments
        .values()
        .flat_map(|d| d.files.iter().cloned())
        .fold(Vec::new(), |mut all, file| {
            if !all.contains(&file) {
                all.push(file)
            }
            all
        });
    ports.save()?;
    collect_files(
        &ports.owner.root,
        &ports.owner,
        &ports
            .preview_files
            .iter()
            .map(|f| f.source.clone())
            .collect(),
    )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reservation {
    pub service: String,
    pub target: u16,
    pub protocol: String,
    pub host_ip: String,
    pub first: u16,
    pub last: u16,
    pub replicas: u16,
    pub automatic: bool,
    pub state: &'static str,
    pub local_url: Option<String>,
    pub preview_url: Option<String>,
}
pub fn reservations(root: &Path) -> Result<Vec<Reservation>> {
    let Some(ports) = super::ports::Ports::read(root)? else {
        return Ok(Vec::new());
    };
    reservations_from(&ports, false)
}

pub(crate) fn reservations_from(
    ports: &super::ports::Ports,
    include_ranges: bool,
) -> Result<Vec<Reservation>> {
    ports
        .groups
        .iter()
        .map(|group| {
            let renderer = ports
                .service_preview_renderer
                .get(&group.binding.service)
                .or_else(|| {
                    ports
                        .service_desired
                        .get(&group.binding.service)
                        .and_then(|id| ports.deployments.get(id))
                        .map(|d| &d.renderer)
                });
            let endpoint =
                (include_ranges || group.first == group.last) && group.binding.protocol == "tcp";
            Ok(Reservation {
                service: group.binding.service.clone(),
                target: group.binding.target,
                protocol: group.binding.protocol.clone(),
                host_ip: group.binding.host_ip.clone(),
                first: group.first,
                last: group.last,
                replicas: group.binding.replicas,
                automatic: group.automatic,
                state: "reserved",
                local_url: endpoint.then(|| local_url(&group.binding.host_ip, group.first)),
                preview_url: if endpoint {
                    renderer
                        .map(|r| r.render(group.first, &group.binding.service))
                        .transpose()?
                        .flatten()
                        .map(|u| u.url)
                } else {
                    None
                },
            })
        })
        .collect()
}
pub fn reservation_lines(
    reservations: &[Reservation],
    services: &[crate::doctor::ComposeServiceStatus],
) -> Vec<String> {
    reservations
        .iter()
        .filter(|r| {
            !services
                .iter()
                .any(|s| s.service == r.service && s.state == "running")
        })
        .map(|r| {
            let ports = if r.first == r.last {
                r.first.to_string()
            } else {
                format!("{}-{}", r.first, r.last)
            };
            let mut line = format!(
                "Leased {}: {}:{ports}/{} (reserved)",
                r.service,
                if r.host_ip.is_empty() {
                    "*"
                } else {
                    &r.host_ip
                },
                r.protocol
            );
            if let Some(url) = &r.local_url {
                line += &format!(" {url}");
            }
            if let Some(url) = &r.preview_url {
                line += &format!(" {url}");
            }
            line
        })
        .collect()
}
