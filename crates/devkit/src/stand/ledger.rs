use super::{epoch::read_bounded, ports::Owner};
use crate::{Result, files::*, processes::ProcessRunner};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

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
    #[serde(default)]
    pub launch_finished: bool,
    #[serde(default)]
    pub launch: Option<LaunchIntent>,
    #[serde(default)]
    pub inspections: BTreeMap<String, InspectionIntent>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchIntent {
    pub daemon_finished: bool,
    pub expected: BTreeMap<String, u16>,
    pub before: BTreeSet<String>,
    pub observed: BTreeMap<String, BTreeMap<u16, String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectionHelper {
    pub name: String,
    pub token: String,
    pub role: String,
    pub observed: Option<crate::processes::InspectionSnapshot>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectionIntent {
    pub nonce: String,
    pub model_sha256: String,
    pub references: BTreeMap<String, String>,
    #[serde(default)]
    pub external: BTreeMap<String, String>,
    pub helpers: BTreeMap<String, InspectionHelper>,
    pub creation_finished: bool,
    #[serde(default)]
    pub creation_uncertain: bool,
}
impl Ledger {
    pub fn launch_terminal(&self) -> bool {
        !self.launch_pending
            || (self.launch_finished && self.launch.as_ref().is_some_and(|l| l.daemon_finished))
    }
    pub fn completed_empty(&self) -> bool {
        self.services.is_empty()
            && self.containers.is_empty()
            && !self.launch_pending
            && self.launch_finished
            && self.inspections.is_empty()
    }
    pub fn read(root: &Path) -> Result<Option<Self>> {
        let value = read_bounded::<Self>(&safe_path(root, ACTIVE)?, 4 * 1024 * 1024)?;
        if let Some(value) = &value {
            ensure!(value.schema_version == 1, "Unsupported stand ledger schema");
            if let Some(launch) = &value.launch {
                ensure!(value.launch_pending, "Creator plan has no launch intent");
                let id = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
                ensure!(
                    launch.before.iter().all(|s| id(s)),
                    "Invalid prior creator identity"
                );
                for (service, rows) in &launch.observed {
                    let count = launch
                        .expected
                        .get(service)
                        .context("Unplanned creator service")?;
                    ensure!(
                        rows.iter()
                            .all(|(number, exact)| (1..=*count).contains(number)
                                && id(exact)
                                && !launch.before.contains(exact)),
                        "Invalid creator outcome"
                    );
                }
                ensure!(
                    launch.expected.values().all(|n| *n > 0),
                    "Invalid creator replica count"
                );
            }
        }
        Ok(value)
    }
    pub fn intent(root: &Path, owner: &Owner, services: &BTreeSet<String>) -> Result<Self> {
        let mut value = Self::open(root, owner)?;
        ensure!(
            value.launch_terminal(),
            "An unfinished stand launch requires recovery; preserving its ownership intent"
        );
        value.services.extend(services.iter().cloned());
        value.launch_pending = false;
        value.launch_finished = true;
        value.launch = None;
        value.save(root)?;
        Ok(value)
    }
    pub fn open(root: &Path, owner: &Owner) -> Result<Self> {
        let mut value = Self::read(root)?.unwrap_or(Self {
            schema_version: 1,
            owner: owner.clone(),
            services: BTreeSet::new(),
            containers: BTreeSet::new(),
            launch_pending: false,
            launch_finished: true,
            launch: None,
            inspections: BTreeMap::new(),
        });
        ensure!(
            value.owner == *owner,
            "Stand ledger belongs to a different checkout incarnation"
        );
        // Read-only until a durable launch or inspection intent is installed.
        value.owner = owner.clone();
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
        ensure!(self.launch_terminal(), "Unproved stand creator outcome");
        verify_owners(rows, &self.owner)?;
        self.containers = rows.iter().map(|c| c.id.clone()).collect();
        self.launch_pending = false;
        self.launch_finished = true;
        self.launch = None;
        self.save(root)
    }
    pub fn begin_launch(
        &mut self,
        root: &Path,
        model: &Value,
        active: &BTreeSet<String>,
        rows: &[Container],
    ) -> Result<()> {
        ensure!(
            self.launch_terminal(),
            "Unproved previous stand creator outcome"
        );
        verify_owners(rows, &self.owner)?;
        let mut expected = BTreeMap::new();
        for service in active {
            let count = super::ports::replica_count(&model["services"][service])?;
            if count > 0 {
                expected.insert(service.clone(), count);
            }
        }
        self.launch = Some(LaunchIntent {
            daemon_finished: false,
            expected,
            before: rows.iter().map(|r| r.id.clone()).collect(),
            observed: BTreeMap::new(),
        });
        self.launch_pending = true;
        self.launch_finished = false;
        self.save(root)
    }
    pub fn recover_launch(
        &mut self,
        root: &Path,
        project: &str,
        runner: &ProcessRunner,
    ) -> Result<()> {
        if self.launch_terminal() {
            return Ok(());
        }
        ensure!(
            self.launch_finished,
            "Stand creator has no terminal receipt; preserving its launch intent and leases"
        );
        ensure!(
            self.launch.is_some(),
            "Stand creator has unknown daemon history; preserving its launch intent and leases"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        for round in 0..10 {
            let rows = observe_query(project, runner, Query::Recovery(deadline))?;
            verify_owners(&rows, &self.owner)?;
            let launch = self.launch.as_mut().unwrap();
            for row in rows {
                if launch.before.contains(&row.id) {
                    continue;
                }
                if let Some(count) = launch.expected.get(&row.service)
                    && let Ok(number) = row.replica.parse::<u16>()
                    && (1..=*count).contains(&number)
                {
                    launch
                        .observed
                        .entry(row.service)
                        .or_default()
                        .insert(number, row.id);
                }
            }
            launch.daemon_finished = launch.expected.iter().all(|(service, count)| {
                launch
                    .observed
                    .get(service)
                    .is_some_and(|rows| rows.len() == usize::from(*count))
            });
            self.save(root)?;
            if self.launch_terminal() {
                return Ok(());
            }
            if round < 9 {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        anyhow::bail!(
            "Stand creator has incomplete daemon outcomes; preserving its launch intent and leases"
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub id: String,
    pub owner: String,
    pub project: String,
    pub service: String,
    #[serde(default)]
    pub replica: String,
    pub running: bool,
    pub image: String,
    pub ports: Value,
    pub configured_ports: Value,
    #[serde(default)]
    pub deployment: String,
    #[serde(default)]
    pub preview_env: BTreeMap<String, String>,
    pub mounts: Value,
}

pub fn inspect(id: &str, runner: &ProcessRunner) -> Result<Container> {
    inspect_query(id, runner, Query::Normal)
}
#[derive(Clone, Copy)]
enum Query {
    Normal,
    Metadata(Instant),
    Recovery(Instant),
}
fn capture(
    command: &mut Command,
    runner: &ProcessRunner,
    query: Query,
) -> Result<crate::processes::ProcessOutput> {
    match query {
        Query::Normal => runner.capture(command),
        Query::Metadata(deadline) => runner.capture_before(command, deadline),
        Query::Recovery(deadline) => runner.capture_recovery_before(command, deadline),
    }
}
fn inspect_query(id: &str, runner: &ProcessRunner, query: Query) -> Result<Container> {
    ensure!(
        id.len() >= 12 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid container ID"
    );
    let mut format = format!(
        "{{{{json (index .Config.Labels \"{}\")}}}}",
        super::preview::DEPLOYMENT_LABEL
    );
    for key in [
        "DEVKIT_PREVIEW_HOSTS_FILE",
        "DEVKIT_PREVIEW_HOSTS_SHA256",
        "DEVKIT_PREVIEW_HOSTS",
        "DEVKIT_PREVIEW_URL",
        "DEVKIT_SERVICE",
        "DEVKIT_BRANCH",
    ]
    .into_iter()
    .chain(super::preview::HOST_ENV_KEYS)
    {
        format += &format!(
            "\n{{{{$v := \"\"}}}}{{{{range .Config.Env}}}}{{{{if eq (index (split . \"=\") 0) \"{key}\"}}}}{{{{$v = .}}}}{{{{end}}}}{{{{end}}}}{{{{json $v}}}}"
        );
    }
    format += "\n{{json (index .Config.Labels \"com.docker.compose.container-number\")}}";
    let output = capture(Command::new("docker").args(["container", "inspect", "--format",
        &format!("{{{{json .Id}}}}\n{{{{json (index .Config.Labels \"{OWNER_LABEL}\")}}}}\n{{{{json (index .Config.Labels \"com.docker.compose.project\")}}}}\n{{{{json (index .Config.Labels \"com.docker.compose.service\")}}}}\n{{{{json .State.Running}}}}\n{{{{json .Image}}}}\n{{{{json .NetworkSettings.Ports}}}}\n{{{{json .Mounts}}}}\n{{{{json .HostConfig.PortBindings}}}}\n{format}"), id]), runner, query)?;
    let lines = output
        .text()?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Value>, _>>()?;
    ensure!(lines.len() == 19, "Invalid bounded stand observation");
    let mut preview_env = BTreeMap::new();
    for line in &lines[10..18] {
        if let Some((key, value)) = line.as_str().and_then(|s| s.split_once('=')) {
            preview_env.insert(key, value);
        }
    }
    let row: Container = serde_json::from_value(
        json!({"id":lines[0],"owner":lines[1].as_str().unwrap_or(""),
        "project":lines[2].as_str().unwrap_or(""),"service":lines[3].as_str().unwrap_or(""),
        "running":lines[4],"image":lines[5],"ports":lines[6],"mounts":lines[7],"configuredPorts":lines[8],"deployment":lines[9].as_str().unwrap_or(""),"previewEnv":preview_env,"replica":lines[18].as_str().unwrap_or("")}),
    )?;
    ensure!(
        row.id.len() == 64 && row.id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid observed container ID"
    );
    Ok(row)
}
pub fn observe(project: &str, runner: &ProcessRunner) -> Result<Vec<Container>> {
    observe_query(project, runner, Query::Normal)
}
pub fn observe_until(
    project: &str,
    runner: &ProcessRunner,
    deadline: Instant,
) -> Result<Vec<Container>> {
    observe_query(project, runner, Query::Metadata(deadline))
}
fn observe_query(project: &str, runner: &ProcessRunner, query: Query) -> Result<Vec<Container>> {
    let out = capture(
        Command::new("docker").args([
            "ps",
            "--all",
            "--quiet",
            "--no-trunc",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
        ]),
        runner,
        query,
    )?;
    let mut rows = Vec::new();
    for id in out.text()?.split_whitespace() {
        ensure!(
            rows.len() < 1024,
            "Stand observation exceeds container budget"
        );
        let row = inspect_query(id, runner, query)?;
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
