//! Durable checkout ownership, independent of the current port lease owner.
use super::epoch::{Physical, read_bounded};
use crate::{
    Result,
    files::*,
    info::git_checkout,
    model::ComposeInventory,
    processes::{OutputMode, ProcessRunner},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const RECEIPT: &str = ".devkit/state/stand.history.json";
const WITNESS: &str = "devkit-stand-witness.json";
const RESOURCE_OWNER: &str = "io.docouno.devkit.resource-generation";
const ANONYMOUS_VOLUME: &str = "com.docker.volume.anonymous";

#[derive(Default)]
pub(crate) struct ResourceDeclarations {
    named_volumes: BTreeSet<String>,
    named_networks: BTreeSet<String>,
    protected_volumes: BTreeSet<String>,
    protected_networks: BTreeSet<String>,
}

impl ResourceDeclarations {
    pub(crate) fn read(root: &Path, inventory: &ComposeInventory, model: &Value) -> Result<Self> {
        let mut result = Self::default();
        for (path, identity) in &inventory.inputs.files {
            let Some(identity) = identity else { continue };
            if path.rsplit('/').next() == Some(".env") {
                continue;
            }
            let (bytes, current) = read_regular_file(
                &input_path(root, path)?,
                4 * 1024 * 1024,
                "resource declarations",
            )?
            .context("Compose source disappeared")?;
            ensure!(
                &current == identity,
                "Compose source changed before resource admission"
            );
            let data = crate::compose::parse_compose(&bytes)?;
            for (kind, names, protected) in [
                (
                    "volumes",
                    &mut result.named_volumes,
                    &mut result.protected_volumes,
                ),
                (
                    "networks",
                    &mut result.named_networks,
                    &mut result.protected_networks,
                ),
            ] {
                for (alias, resource) in data[kind].as_object().into_iter().flatten() {
                    let external = resource
                        .get("external")
                        .filter(|v| *v != false && !v.is_null());
                    let nested = match external {
                        Some(value) if value != true => Some(
                            value["name"]
                                .as_str()
                                .context("Unrecognized Compose external resource declaration")?,
                        ),
                        _ => None,
                    };
                    let explicit = resource["name"].as_str().or(nested);
                    if external.is_some() || explicit.is_some() {
                        names.insert(alias.clone());
                    }
                    if external.is_some() || explicit.is_some() {
                        let value = explicit.unwrap_or(alias);
                        let name = if value.contains('$') {
                            model[kind][alias]["name"].as_str()
                        } else {
                            Some(value)
                        };
                        protected.insert(name.context("Cannot resolve an explicit Compose resource name; review external declarations")?.to_owned());
                    }
                }
            }
        }
        Ok(result)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Resource {
    name: String,
    identity: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    root: Physical,
    state: Physical,
    lock: Physical,
    generation: String,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Daemon {
    endpoint: String,
    server: String,
}

impl Daemon {
    fn current(runner: &ProcessRunner, deadline: Instant) -> Result<Self> {
        let query = |args: &[&str]| -> Result<String> {
            let result = runner.capture_before(Command::new("docker").args(args), deadline)?;
            Ok(result.text()?.trim_end_matches('\n').to_owned())
        };
        let inspected = query(&[
            "context",
            "inspect",
            "--format",
            "{{.Name}}\n{{.Endpoints.docker.Host}}",
        ])?;
        let mut lines = inspected.lines();
        let context = lines.next().unwrap_or_default().to_owned();
        let host = lines.next().unwrap_or_default().to_owned();
        ensure!(
            !context.is_empty()
                && context.len() <= 256
                && !context.starts_with('-')
                && !host.is_empty()
                && lines.next().is_none(),
            "Invalid Docker context"
        );
        let server = query(&["info", "--format", "{{.ID}}"])?;
        ensure!(
            !server.is_empty()
                && server.len() <= 256
                && server
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || b"-:".contains(&ch)),
            "Docker daemon identity is unavailable"
        );
        let env = ["DOCKER_HOST", "DOCKER_CONTEXT"].map(std::env::var_os);
        let endpoint = digest(json_bytes(&(context, host, env))?);
        Ok(Self { endpoint, server })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct History {
    identity: Identity,
    daemon: Option<Daemon>,
    pub(crate) retired: bool,
    pub(crate) cleaned: bool,
    #[serde(default)]
    actors_stopped: bool,
    #[serde(default)]
    lease_cache: Option<PathBuf>,
    #[serde(default)]
    networks: BTreeMap<String, Resource>,
    #[serde(default)]
    volumes: BTreeMap<String, Resource>,
    #[serde(default)]
    builds: BTreeMap<String, Resource>,
    #[serde(default)]
    legacy_volumes: BTreeMap<String, String>,
    #[serde(default)]
    anonymous_volumes: BTreeMap<String, String>,
}

fn witness_path(root: &Path, runner: &ProcessRunner, deadline: Instant) -> Result<PathBuf> {
    if let Some(git) = git_checkout(root, runner, deadline)? {
        return safe_path(&git.admin, WITNESS);
    }
    ensure!(
        !exists(&root.join(".git"))?,
        "Cannot verify the current Git worktree registration"
    );
    safe_path(root, &format!(".devkit/cache/{WITNESS}"))
}

impl Identity {
    fn current(root: &Path, generation: String) -> Result<Self> {
        Ok(Self {
            root: Physical::at(root)?,
            state: Physical::at(&safe_path(root, ".devkit/state")?)?,
            lock: Physical::at(&safe_path(root, ".devkit/state/task.lock")?)?,
            generation,
        })
    }
}

impl History {
    pub(crate) fn read(
        root: &Path,
        runner: &ProcessRunner,
        deadline: Instant,
    ) -> Result<Option<Self>> {
        let witness: Option<Identity> =
            read_bounded(&witness_path(root, runner, deadline)?, 65536)?;
        let local: Option<Identity> = read_bounded(
            &safe_path(root, &format!(".devkit/cache/{WITNESS}"))?,
            65536,
        )?;
        ensure!(
            witness
                .as_ref()
                .zip(local.as_ref())
                .is_none_or(|(git, cache)| git == cache),
            "Conflicting checkout ownership witnesses"
        );
        let witness = witness.or(local);
        let receipt: Option<Self> = read_bounded(&safe_path(root, RECEIPT)?, 4 * 1024 * 1024)?;
        match (witness, receipt) {
            (None, None) => Ok(None),
            (Some(witness), receipt) => {
                ensure!(
                    GENERATION_RE.is_match(&witness.generation)
                        && Identity::current(root, witness.generation.clone())
                            .ok()
                            .as_ref()
                            == Some(&witness),
                    "Stand ownership history was lost or belongs to another checkout"
                );
                if let Some(receipt) = receipt {
                    ensure!(
                        witness == receipt.identity,
                        "Stand ownership history changed"
                    );
                    Ok(Some(receipt))
                } else {
                    anyhow::bail!(
                        "Stand ownership receipt is missing; restore the recorded checkout state"
                    )
                }
            }
            (None, Some(_)) => anyhow::bail!("Stand ownership witness is missing"),
        }
    }

    pub(crate) fn open(root: &Path, runner: &ProcessRunner, deadline: Instant) -> Result<Self> {
        if let Some(history) = Self::read(root, runner, deadline)? {
            return Ok(history);
        }
        let path = witness_path(root, runner, deadline)?;
        let identity = Identity::current(root, uuid::Uuid::new_v4().to_string())?;
        ensure!(
            Instant::now() < deadline,
            "Stand ownership admission deadline exceeded"
        );
        let history = Self {
            identity,
            daemon: None,
            retired: false,
            cleaned: false,
            actors_stopped: false,
            lease_cache: None,
            networks: BTreeMap::new(),
            volumes: BTreeMap::new(),
            builds: BTreeMap::new(),
            legacy_volumes: BTreeMap::new(),
            anonymous_volumes: BTreeMap::new(),
        };
        // Publish the independent witness before a registry or daemon mutation.
        // An interrupted publication leaves an explicit lost-history state.
        write_json(&path, &history.identity)?;
        history.save(root)?;
        Ok(history)
    }

    pub(crate) fn save(&self, root: &Path) -> Result<()> {
        let bytes = json_bytes(self)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "Stand ownership history exceeds byte budget"
        );
        write_atomic(&safe_path(root, RECEIPT)?, &bytes, 0o644)
    }

    pub(crate) fn allow_start(&self) -> Result<()> {
        ensure!(
            !self.retired,
            "Stand is retired; finish removing this worktree and use a new checkout"
        );
        Ok(())
    }

    pub(crate) fn has_daemon(&self) -> bool {
        self.daemon.is_some()
    }

    pub(crate) fn may_have_actors(&self) -> bool {
        self.has_daemon() && !self.actors_stopped && !self.cleaned
    }

    pub(crate) fn record_stopped(&mut self, root: &Path) -> Result<()> {
        self.actors_stopped = true;
        self.save(root)
    }

    pub(crate) fn prove_lease_history(&self, root: &Path) -> Result<()> {
        if let Some(cache) = &self.lease_cache {
            super::ports::Ports::verify_checkout_claims(root, cache)?;
        }
        Ok(())
    }

    pub(crate) fn bind_lease_cache(&mut self, root: &Path, cache: &Path) -> Result<()> {
        self.prove_lease_history(root)?;
        super::ports::Ports::verify_checkout_claims(root, cache)?;
        self.lease_cache = Some(cache.to_owned());
        self.save(root)
    }

    pub(crate) fn verify_daemon(&self, runner: &ProcessRunner, deadline: Instant) -> Result<()> {
        if let Some(previous) = &self.daemon {
            ensure!(
                Daemon::current(runner, deadline)? == *previous,
                "Stand belongs to a different Docker daemon; retry with the original Docker context"
            );
        }
        Ok(())
    }

    pub(crate) fn bind_daemon(&mut self, root: &Path, runner: &ProcessRunner) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        if self.has_daemon() {
            self.verify_daemon(runner, deadline)?;
            if !self.actors_stopped {
                return Ok(());
            }
        } else {
            self.daemon = Some(Daemon::current(runner, deadline)?);
        }
        self.actors_stopped = false;
        self.save(root)
    }

    pub(crate) fn prepare_resources(
        &mut self,
        root: &Path,
        project: &str,
        model: &Value,
        active: &BTreeSet<String>,
        declarations: &ResourceDeclarations,
        runner: &ProcessRunner,
    ) -> Result<Vec<u8>> {
        let mut lines = String::new();
        let name = |kind: &str, alias: &str| {
            resource_name(project, &self.identity.generation, kind, alias)
        };
        for (kind, names, known) in [
            ("volume", &declarations.named_volumes, &mut self.volumes),
            ("network", &declarations.named_networks, &mut self.networks),
        ] {
            let section = format!("{kind}s");
            let mut entries = Vec::new();
            for (alias, spec) in model[&section].as_object().into_iter().flatten() {
                if names.contains(alias) || spec["external"] == true {
                    continue;
                }
                if kind == "volume" && !known.contains_key(alias) {
                    let original = spec["name"]
                        .as_str()
                        .context("Missing normalized volume name")?;
                    if !self.legacy_volumes.contains_key(alias)
                        && let Some(previous) = inspect(runner, kind, original)?
                    {
                        ensure!(
                            previous["Driver"] == "local"
                                && previous["Labels"]["com.docker.compose.project"] == project
                                && previous["Labels"]["com.docker.compose.volume"]
                                    == alias.as_str()
                                && previous["Labels"].get(RESOURCE_OWNER).is_none(),
                            "Existing volume {original} has ambiguous ownership. Verify its identity and declare an external volume explicitly"
                        );
                        self.legacy_volumes
                            .insert(alias.clone(), original.to_owned());
                    }
                    if let Some(volume) = self.legacy_volumes.get(alias) {
                        ensure!(
                            volume == original,
                            "Protected volume mapping changed: {alias}"
                        );
                        entries.push((
                            alias,
                            format!("!override {}", json!({"name":volume,"external":true})),
                        ));
                        continue;
                    }
                }
                let expected = name(kind, alias);
                if !known.contains_key(alias) {
                    ensure!(
                        inspect(runner, kind, &expected)?.is_none(),
                        "Stand generation resource already exists: {expected}"
                    );
                    known.insert(
                        alias.clone(),
                        Resource {
                            name: expected.clone(),
                            identity: None,
                        },
                    );
                }
                ensure!(known[alias].name == expected, "Stand resource name changed");
                if let Some(actual) = inspect(runner, kind, &expected)? {
                    verify_resource(&actual, &known[alias], kind, &self.identity.generation)?;
                }
                entries.push((
                    alias,
                    json!({"name":expected,"labels":{RESOURCE_OWNER: self.identity.generation}})
                        .to_string(),
                ));
            }
            if !entries.is_empty() {
                lines += &format!("{section}:\n");
                for (alias, value) in entries {
                    lines += &format!("  {}: {value}\n", serde_json::to_string(alias)?);
                }
            }
        }
        let mut services = BTreeMap::new();
        for service in super::inspection::image_dependencies(model, active)? {
            let spec = &model["services"][&service];
            if spec.get("build").is_none() || spec.get("image").is_some() {
                continue;
            }
            let tag = format!("{}:stand", name("image", &service));
            if !self.builds.contains_key(&service) {
                ensure!(
                    inspect(runner, "image", &tag)?.is_none(),
                    "Stand generation build tag already exists: {tag}"
                );
                self.builds.insert(
                    service.clone(),
                    Resource {
                        name: tag.clone(),
                        identity: None,
                    },
                );
            }
            let resource = self
                .builds
                .get_mut(&service)
                .context("Missing build intent")?;
            ensure!(resource.name == tag, "Stand build tag changed");
            if let Some(actual) = inspect(runner, "image", &tag)? {
                verify_resource(&actual, resource, "image", &self.identity.generation)?;
            }
            // This admitted Compose build may replace its own tag. Capture the
            // new image identity after the creator has settled.
            resource.identity = None;
            services.insert(
                service,
                json!({"image":tag,"build":{"labels":{RESOURCE_OWNER: self.identity.generation}}}),
            );
        }
        if !services.is_empty() {
            lines += &format!("services: {}\n", serde_json::to_string(&services)?);
        }
        self.save(root)?;
        Ok(lines.into_bytes())
    }

    pub(crate) fn record_anonymous(
        &mut self,
        root: &Path,
        rows: &[super::ledger::Container],
        declarations: &ResourceDeclarations,
        runner: &ProcessRunner,
    ) -> Result<()> {
        for row in rows {
            let candidate_mounts: Vec<_> = row
                .mounts
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["Type"] == "volume")
                .filter_map(|m| Some((m["Name"].as_str()?, m["Destination"].as_str()?)))
                .filter(|(name, _)| {
                    SHA256_RE.is_match(name) && !declarations.protected_volumes.contains(*name)
                })
                .collect();
            if candidate_mounts.is_empty() {
                continue;
            }
            let inspected = runner.capture(Command::new("docker").args([
                "container", "inspect", "--format", "{{json .HostConfig.VolumesFrom}}\n{{json .HostConfig.Mounts}}\n{{json .HostConfig.Binds}}", &row.id
            ]))?;
            let fields = inspected
                .text()?
                .lines()
                .map(serde_json::from_str::<Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(
                fields.len() == 3
                    && fields
                        .iter()
                        .all(|value| value.is_array() || value.is_null()),
                "Cannot establish anonymous mount origin"
            );
            // Docker repeats the provider's anonymous label on volumes_from.
            // Admission is limited to allocations made by the observed container.
            let inherited = fields[0]
                .as_array()
                .is_some_and(|providers| !providers.is_empty());
            for (name, target) in candidate_mounts {
                let own_anonymous = fields[1].as_array().into_iter().flatten().any(|mount| {
                    mount["Type"] == "volume"
                        && mount["Target"] == target
                        && mount["Source"].as_str().is_none_or(str::is_empty)
                }) || fields[2]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|bind| {
                        bind == target
                            || bind.split_once(':').is_some_and(|(path, options)| {
                                path == target && ["rw", "ro"].contains(&options)
                            })
                    });
                if inherited && !own_anonymous {
                    continue;
                }
                let explicit_mount = fields[1]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|mount| mount["Source"] == name);
                let explicit_bind = fields[2]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|bind| {
                        bind.split_once(':')
                            .is_some_and(|(source, _)| source == name)
                    });
                if explicit_mount || explicit_bind {
                    continue;
                }
                let volume = inspect(runner, "volume", name)?.context("Missing mounted volume")?;
                if let Some(id) = anonymous_identity(&volume, name) {
                    ensure!(
                        self.anonymous_volumes
                            .get(name)
                            .is_none_or(|saved| saved == id),
                        "Anonymous stand volume was replaced: {name}"
                    );
                    self.anonymous_volumes
                        .insert(name.to_owned(), id.to_owned());
                }
            }
        }
        self.save(root)
    }

    pub(crate) fn record_resources(
        &mut self,
        root: &Path,
        declarations: &ResourceDeclarations,
        runner: &ProcessRunner,
    ) -> Result<()> {
        for (kind, resources) in [
            ("network", &mut self.networks),
            ("volume", &mut self.volumes),
            ("image", &mut self.builds),
        ] {
            for resource in resources.values_mut() {
                if is_protected(declarations, kind, &resource.name) {
                    continue;
                }
                if let Some(actual) = inspect(runner, kind, &resource.name)? {
                    let id = verify_resource(&actual, resource, kind, &self.identity.generation)?;
                    resource.identity = Some(id);
                } else {
                    resource.identity = None;
                }
            }
        }
        self.save(root)
    }

    pub(crate) fn remove_resources(
        &mut self,
        root: &Path,
        project: &str,
        declarations: &ResourceDeclarations,
        runner: &ProcessRunner,
        networks_only: bool,
    ) -> Result<()> {
        for (kind, resources) in [
            ("network", &mut self.networks),
            ("volume", &mut self.volumes),
            ("image", &mut self.builds),
        ] {
            if networks_only && kind != "network" {
                continue;
            }
            for (alias, resource) in resources.iter_mut() {
                let expected = resource_name(project, &self.identity.generation, kind, alias);
                ensure!(
                    resource.name
                        == if kind == "image" {
                            format!("{expected}:stand")
                        } else {
                            expected
                        },
                    "Unexpected stand resource name"
                );
                if is_protected(declarations, kind, &resource.name) {
                    continue;
                }
                let Some(actual) = inspect(runner, kind, &resource.name)? else {
                    resource.identity = None;
                    continue;
                };
                let id = verify_resource(&actual, resource, kind, &self.identity.generation)?;
                let argument = match kind {
                    "network" => {
                        ensure!(
                            actual["Containers"]
                                .as_object()
                                .is_some_and(|v| v.is_empty()),
                            "Stand network {} still has attached containers",
                            resource.name
                        );
                        &id
                    }
                    "volume" => {
                        let filter = format!("volume={}", resource.name);
                        let users = runner.capture(
                            Command::new("docker")
                                .args(["ps", "--all", "--quiet", "--filter", &filter]),
                        )?;
                        ensure!(
                            users.text()?.trim().is_empty(),
                            "Stand volume {} still has users",
                            resource.name
                        );
                        &resource.name
                    }
                    _ => &resource.name,
                };
                let current = inspect(runner, kind, &resource.name)?
                    .context("Stand resource changed before cleanup")?;
                ensure!(
                    verify_resource(&current, resource, kind, &self.identity.generation)? == id,
                    "Stand resource changed before cleanup: {}",
                    resource.name
                );
                runner.capture(Command::new("docker").args([kind, "rm", argument]))?;
                ensure!(
                    inspect(runner, kind, &resource.name)?.is_none(),
                    "Stand resource is still present: {}",
                    resource.name
                );
                resource.identity = None;
            }
        }
        if !networks_only {
            for (name, id) in &self.anonymous_volumes {
                ensure!(SHA256_RE.is_match(name), "Invalid anonymous volume name");
                if declarations.protected_volumes.contains(name) {
                    continue;
                }
                let Some(actual) = inspect(runner, "volume", name)? else {
                    continue;
                };
                ensure!(
                    anonymous_identity(&actual, name) == Some(id),
                    "Anonymous stand volume was replaced: {name}"
                );
                ensure_unused_volume(runner, name)?;
                let current = inspect(runner, "volume", name)?
                    .context("Anonymous stand volume changed before cleanup")?;
                ensure!(
                    anonymous_identity(&current, name) == Some(id),
                    "Anonymous stand volume changed before cleanup: {name}"
                );
                runner.capture(Command::new("docker").args(["volume", "rm", name]))?;
                ensure!(
                    inspect(runner, "volume", name)?.is_none(),
                    "Anonymous stand volume still exists: {name}"
                );
            }
            self.anonymous_volumes.clear();
        }
        self.save(root)
    }
}

fn anonymous_identity<'a>(volume: &'a Value, name: &str) -> Option<&'a str> {
    (volume["Name"] == name
        && volume["Driver"] == "local"
        && volume["Labels"].get(ANONYMOUS_VOLUME).is_some())
    .then(|| volume["CreatedAt"].as_str())
    .flatten()
}

fn ensure_unused_volume(runner: &ProcessRunner, name: &str) -> Result<()> {
    let filter = format!("volume={name}");
    let users = runner
        .capture(Command::new("docker").args(["ps", "--all", "--quiet", "--filter", &filter]))?;
    ensure!(
        users.text()?.trim().is_empty(),
        "Stand volume {name} still has users"
    );
    Ok(())
}

fn is_protected(declarations: &ResourceDeclarations, kind: &str, name: &str) -> bool {
    match kind {
        "network" => declarations.protected_networks.contains(name),
        "volume" => declarations.protected_volumes.contains(name),
        _ => false,
    }
}

fn resource_name(project: &str, generation: &str, kind: &str, alias: &str) -> String {
    format!("{project}-{}-{kind}-{}", generation, &digest(alias)[..12])
}

fn inspect(runner: &ProcessRunner, kind: &str, name: &str) -> Result<Option<Value>> {
    let output = runner.run_with_timeout(
        Command::new("docker").args([kind, "inspect", name]),
        OutputMode::Capture(1024 * 1024),
        Duration::from_secs(3),
    )?;
    if output.code != 0 {
        let message = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if message.contains("no such volume")
            || message.contains("no such image")
            || message.contains("network") && message.contains("not found")
        {
            return Ok(None);
        }
    }
    let output = output.success("Docker stand resource inspection")?;
    let data: Vec<Value> = serde_json::from_slice(&output.stdout)?;
    ensure!(
        data.len() == 1,
        "Unexpected Docker stand resource inspection"
    );
    Ok(data.into_iter().next())
}

fn verify_resource(
    actual: &Value,
    resource: &Resource,
    kind: &str,
    generation: &str,
) -> Result<String> {
    let labels = if kind == "image" {
        &actual["Config"]["Labels"]
    } else {
        &actual["Labels"]
    };
    ensure!(
        labels[RESOURCE_OWNER] == generation,
        "Stand resource ownership changed: {}",
        resource.name
    );
    let id = match kind {
        "volume" => {
            ensure!(
                actual["Name"] == resource.name && actual["Driver"] == "local",
                "Stand volume identity changed"
            );
            actual["CreatedAt"].as_str()
        }
        "image" => {
            ensure!(
                actual["RepoTags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag == &resource.name)),
                "Stand build tag changed"
            );
            actual["Id"].as_str()
        }
        _ => {
            ensure!(
                actual["Name"] == resource.name,
                "Stand network name changed"
            );
            actual["Id"].as_str()
        }
    }
    .context("Missing stand resource identity")?;
    ensure!(
        resource
            .identity
            .as_deref()
            .is_none_or(|previous| previous == id),
        "Stand resource was replaced: {}",
        resource.name
    );
    Ok(id.to_owned())
}
