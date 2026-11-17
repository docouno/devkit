use super::{epoch::read_bounded, ports::Owner};
use crate::{Result, files::*, processes::ProcessRunner};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, process::Command};

pub const OWNER_LABEL: &str = "io.docouno.devkit.stand-owner";
pub const ACTIVE: &str = ".devkit/state/stand.active";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ledger {
    pub schema_version: u32,
    pub owner: Owner,
    pub services: BTreeSet<String>,
    pub containers: BTreeSet<String>,
    pub launch_pending: bool,
}
impl Ledger {
    pub fn read(root: &Path) -> Result<Option<Self>> {
        let value = read_bounded::<Self>(&safe_path(root, ACTIVE)?, 4 * 1024 * 1024)?;
        if let Some(value) = &value {
            ensure!(value.schema_version == 1, "Unsupported stand ledger schema");
        }
        Ok(value)
    }
    pub fn intent(root: &Path, owner: &Owner, services: &BTreeSet<String>) -> Result<Self> {
        let mut value = Self::read(root)?.unwrap_or(Self {
            schema_version: 1,
            owner: owner.clone(),
            services: BTreeSet::new(),
            containers: BTreeSet::new(),
            launch_pending: true,
        });
        ensure!(
            value.owner == *owner,
            "Stand ledger belongs to a different checkout incarnation"
        );
        value.services.extend(services.iter().cloned());
        value.launch_pending = true;
        value.save(root)?;
        Ok(value)
    }
    pub fn save(&self, root: &Path) -> Result<()> {
        let bytes = json_bytes(self)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "Stand ledger exceeds byte budget"
        );
        write_atomic(&safe_path(root, ACTIVE)?, &bytes, 0o644)
    }
    pub fn observed(&mut self, root: &Path, rows: &[Container]) -> Result<()> {
        verify_owners(rows, &self.owner)?;
        self.containers = rows.iter().map(|c| c.id.clone()).collect();
        self.launch_pending = false;
        self.save(root)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub id: String,
    pub owner: String,
    pub project: String,
    pub service: String,
    pub running: bool,
    pub image: String,
    pub ports: Value,
    pub configured_ports: Value,
    pub mounts: Value,
}

pub fn inspect(id: &str, runner: &ProcessRunner) -> Result<Container> {
    ensure!(
        id.len() >= 12 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid container ID"
    );
    let output = runner.capture(Command::new("docker").args(["container", "inspect", "--format",
        &format!("{{{{json .Id}}}}\n{{{{json (index .Config.Labels \"{OWNER_LABEL}\")}}}}\n{{{{json (index .Config.Labels \"com.docker.compose.project\")}}}}\n{{{{json (index .Config.Labels \"com.docker.compose.service\")}}}}\n{{{{json .State.Running}}}}\n{{{{json .Image}}}}\n{{{{json .NetworkSettings.Ports}}}}\n{{{{json .Mounts}}}}\n{{{{json .HostConfig.PortBindings}}}}"), id]))?;
    let lines = output
        .text()?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Value>, _>>()?;
    ensure!(lines.len() == 9, "Invalid bounded stand observation");
    let row: Container = serde_json::from_value(
        json!({"id":lines[0],"owner":lines[1].as_str().unwrap_or(""),
        "project":lines[2].as_str().unwrap_or(""),"service":lines[3].as_str().unwrap_or(""),
        "running":lines[4],"image":lines[5],"ports":lines[6],"mounts":lines[7],"configuredPorts":lines[8]}),
    )?;
    ensure!(
        row.id.len() == 64 && row.id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid observed container ID"
    );
    Ok(row)
}
pub fn observe(project: &str, runner: &ProcessRunner) -> Result<Vec<Container>> {
    let out = runner.capture(Command::new("docker").args([
        "ps",
        "--all",
        "--quiet",
        "--no-trunc",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
    ]))?;
    let mut rows = Vec::new();
    for id in out.text()?.split_whitespace() {
        ensure!(
            rows.len() < 1024,
            "Stand observation exceeds container budget"
        );
        let row = inspect(id, runner)?;
        ensure!(
            row.project == project,
            "Container project changed during observation"
        );
        rows.push(row);
    }
    Ok(rows)
}
pub fn verify_owners(rows: &[Container], owner: &Owner) -> Result<()> {
    for row in rows {
        ensure!(
            row.owner == owner.key,
            "Stand ownership conflict: container {} ({}) has unknown or different origin; inspect it and run make down explicitly before retrying cleanup",
            row.id,
            row.service
        );
    }
    Ok(())
}
pub fn active_ports(rows: &[Container], owner: &Owner) -> Result<super::ports::ActivePorts> {
    verify_owners(rows, owner)?;
    let mut result = BTreeSet::new();
    for row in rows.iter().filter(|r| r.running) {
        let Some(ports) = row.ports.as_object() else {
            continue;
        };
        for (target, bindings) in ports {
            let (target, protocol) = target
                .split_once('/')
                .context("Invalid observed port protocol")?;
            for binding in bindings.as_array().into_iter().flatten() {
                let port: u16 = binding["HostPort"]
                    .as_str()
                    .context("Invalid observed host port")?
                    .parse()?;
                let host: std::net::IpAddr = binding["HostIp"]
                    .as_str()
                    .context("Invalid observed host IP")?
                    .parse()?;
                result.insert((
                    row.service.clone(),
                    port,
                    protocol.to_owned(),
                    host.to_canonical().to_string(),
                    target.parse()?,
                ));
            }
        }
    }
    Ok(result)
}

pub fn remove_owned(
    rows: &[Container],
    project: &str,
    owner: &Owner,
    runner: &ProcessRunner,
) -> Result<()> {
    verify_owners(rows, owner)?;
    for row in rows {
        let actual = inspect(&row.id, runner)?;
        ensure!(
            actual.project == project && actual.owner == owner.key && actual.service == row.service,
            "Container ownership changed before cleanup"
        );
        runner.capture(Command::new("docker").args(["rm", "--force", &row.id]))?;
    }
    ensure!(
        observe(project, runner)?.is_empty(),
        "Stand remnants remain after cleanup"
    );
    Ok(())
}
