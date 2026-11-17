//! Stopped, image-only inspection. No project process runs during namespace proof.
use super::{
    ledger::{InspectionHelper, InspectionIntent, Ledger},
    ports::Owner,
    preview::{SLOTS, escape_compose},
};
use crate::{
    Result,
    files::*,
    processes::{CpMetadata, OutputMode, OwnedContainer, ProcessRunner},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const TOKEN: &str = "io.docouno.devkit.inspection";
const ROLE: &str = "io.docouno.devkit.inspection-role";
const OWNER: &str = "io.docouno.devkit.inspection-owner";
const BUDGET: usize = 1024;

fn owned(helper: &InspectionHelper) -> OwnedContainer {
    OwnedContainer {
        name: helper.name.clone(),
        label: TOKEN.into(),
        token: helper.token.clone(),
    }
}
fn labels(owner: &Owner, helper: &InspectionHelper) -> BTreeMap<String, String> {
    BTreeMap::from([
        (OWNER.into(), owner.key.clone()),
        (ROLE.into(), helper.role.clone()),
    ])
}
/// A missing planned name proves nothing while its creator may still be alive.
pub fn recover(root: &Path, ledger: &mut Ledger, runner: &ProcessRunner) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let had_inspections = !ledger.inspections.is_empty();
    for key in ledger.inspections.keys().cloned().collect::<Vec<_>>() {
        ensure!(
            ledger.inspections[&key].creation_finished,
            "Image inspection creator has no terminal receipt; preserving recovery intent {key}"
        );
        for service in ledger.inspections[&key]
            .helpers
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            let helper = ledger.inspections[&key].helpers[&service].clone();
            let observed = runner.inspection_snapshot(
                &owned(&helper),
                &labels(&ledger.owner, &helper),
                deadline,
            )?;
            if let Some(snapshot) = &observed {
                if let Some(previous) = &helper.observed {
                    ensure!(
                        snapshot.id == previous.id
                            && snapshot.image == previous.image
                            && snapshot.anonymous_volumes == previous.anonymous_volumes,
                        "Image inspection identity changed"
                    );
                }
                ledger
                    .inspections
                    .get_mut(&key)
                    .unwrap()
                    .helpers
                    .get_mut(&service)
                    .unwrap()
                    .observed = Some(snapshot.clone());
                ledger.save(root)?;
            }
            if let Some(snapshot) = observed.as_ref().or(helper.observed.as_ref()) {
                runner.remove_inspection(
                    &owned(&helper),
                    &labels(&ledger.owner, &helper),
                    snapshot,
                    deadline,
                )?;
            }
        }
        ledger.inspections.remove(&key);
        ledger.save(root)?;
    }
    if had_inspections && ledger.completed_empty() {
        let mut ports =
            super::ports::Ports::read(root)?.context("Missing preflight ownership receipt")?;
        ensure!(
            ports.owner == ledger.owner,
            "Preflight ownership changed during recovery"
        );
        ports.possible_actors = false;
        ports.save()?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Missing,
    Directory,
    File,
    Link(String),
}
fn tar_string(bytes: &[u8]) -> Result<&str> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    Ok(std::str::from_utf8(&bytes[..end])?)
}
fn header_entry(header: &[u8; 512], path: &str) -> Result<Entry> {
    let expected = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let name = tar_string(&header[..100])?.trim_end_matches('/');
    ensure!(
        name == expected && tar_string(&header[345..500])?.is_empty(),
        "Unexpected image metadata archive name"
    );
    let checksum = u64::from_str_radix(tar_string(&header[148..156])?.trim(), 8)?;
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                *b as u64
            }
        })
        .sum();
    ensure!(
        checksum == sum && (&header[257..262] == b"ustar" || header[257..262] == [0; 5]),
        "Invalid image metadata archive header"
    );
    match header[156] {
        b'5' => Ok(Entry::Directory),
        b'0' | 0 => Ok(Entry::File),
        b'2' => {
            let target = tar_string(&header[157..257])?;
            ensure!(!target.is_empty(), "Empty image symlink");
            Ok(Entry::Link(target.into()))
        }
        _ => anyhow::bail!("Opaque image metadata archive entry; cannot prove a preview slot"),
    }
}
fn normalize(path: &str) -> Result<String> {
    ensure!(
        path.starts_with('/') && path.len() <= 4096 && !path.contains('\0'),
        "Invalid mount destination"
    );
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}
fn under(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || ancestor == "/"
        || path
            .strip_prefix(ancestor)
            .is_some_and(|s| s.starts_with('/'))
}
struct Namespace<'a> {
    runner: &'a ProcessRunner,
    id: String,
    cache: BTreeMap<String, Entry>,
    queries: &'a Cell<usize>,
    deadline: Instant,
}
impl Namespace<'_> {
    fn entry(&mut self, path: &str) -> Result<Entry> {
        if let Some(value) = self.cache.get(path) {
            return Ok(value.clone());
        }
        ensure!(
            self.queries.get() < BUDGET && Instant::now() < self.deadline,
            "Image namespace inspection exceeds query/deadline budget"
        );
        self.queries.set(self.queries.get() + 1);
        let entry = match self.runner.cp_metadata(
            &self.id,
            path,
            self.deadline.saturating_duration_since(Instant::now()),
        )? {
            CpMetadata::Missing => Entry::Missing,
            CpMetadata::Header(header) => header_entry(&header, path)?,
        };
        self.cache.insert(path.into(), entry.clone());
        Ok(entry)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Backing {
    Bind(PathBuf),
    Tmpfs,
    Opaque,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    target: String,
    tmpfs_rw: bool,
    backing: Backing,
    implicit: bool,
}
fn target_mount(value: &Value) -> Result<Mount> {
    Ok(Mount {
        target: value
            .get("target")
            .and_then(Value::as_str)
            .context("Unproved mount target")?
            .into(),
        tmpfs_rw: value["type"] == "tmpfs"
            && value.get("read_only").and_then(Value::as_bool) != Some(true),
        backing: match value["type"].as_str() {
            Some("bind") => Backing::Bind(
                value["source"]
                    .as_str()
                    .context("Unproved bind source")?
                    .into(),
            ),
            Some("tmpfs") => Backing::Tmpfs,
            _ => Backing::Opaque,
        },
        implicit: false,
    })
}
/// Never infer a backing volume's contents from a fresh image snapshot. Bind
/// ancestry is checked on the host without following any symlink.
fn prove_backing(
    mount: &Mount,
    relative: &[&str],
    queries: &Cell<usize>,
    deadline: Instant,
) -> Result<()> {
    match &mount.backing {
        Backing::Tmpfs => Ok(()),
        Backing::Opaque => {
            ensure!(
                relative.is_empty(),
                "Nested mount destination crosses an opaque persistent namespace; reserve a literal preview slot outside it"
            );
            Ok(())
        }
        Backing::Bind(source) => {
            ensure!(source.is_absolute(), "Unproved bind source ancestry");
            let mut path = source.clone();
            for (index, part) in std::iter::once(None)
                .chain(relative.iter().map(|s| Some(*s)))
                .enumerate()
            {
                ensure!(
                    queries.get() < BUDGET && Instant::now() < deadline,
                    "Backing namespace proof exceeds query/deadline budget"
                );
                queries.set(queries.get() + 1);
                if let Some(part) = part {
                    path.push(part);
                }
                let metadata = std::fs::symlink_metadata(&path).with_context(|| {
                    format!("Cannot prove nested mount ancestry at {}", path.display())
                })?;
                ensure!(
                    !metadata.is_symlink() && (index == relative.len() || metadata.is_dir()),
                    "Nested mount destination has source-backed symlink or non-directory ancestry"
                );
            }
            Ok(())
        }
    }
}
impl Namespace<'_> {
    fn mount_destination(&mut self, original: &str, previous: &[Mount]) -> Result<String> {
        ensure!(
            Instant::now() < self.deadline,
            "Mount namespace proof exceeded deadline"
        );
        let mut path = normalize(original)?;
        for _ in 0..16 {
            let parts: Vec<_> = path
                .trim_start_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            let mut prefix = String::new();
            let mut replacement = None;
            let mut missing = false;
            if let Some(mount) = previous.iter().find(|m| m.target == "/") {
                prove_backing(mount, &parts, self.queries, self.deadline)?;
                return Ok(path);
            }
            for (index, part) in parts.iter().enumerate() {
                prefix.push('/');
                prefix.push_str(part);
                if let Some(mount) = previous.iter().find(|m| m.target == prefix) {
                    prove_backing(mount, &parts[index + 1..], self.queries, self.deadline)?;
                    return Ok(path);
                }
                if missing {
                    continue;
                }
                match self.entry(&prefix)? {
                    Entry::Missing => missing = true,
                    Entry::Link(target) => {
                        let parent = prefix.rsplit_once('/').unwrap().0;
                        let target = if target.starts_with('/') {
                            target
                        } else {
                            format!("{parent}/{target}")
                        };
                        replacement = Some(normalize(&format!(
                            "{target}/{}",
                            parts[index + 1..].join("/")
                        ))?);
                        break;
                    }
                    Entry::File if index + 1 < parts.len() => {
                        anyhow::bail!("Mount destination has image non-directory ancestry")
                    }
                    _ => {}
                }
            }
            if let Some(next) = replacement {
                path = next;
            } else {
                return Ok(path);
            }
        }
        anyhow::bail!("Mount destination symlink expansion exceeds budget")
    }
    fn literal_slot(&mut self, slot: &str, mounts: &[Mount]) -> Result<()> {
        ensure!(
            !mounts.iter().any(|m| under(&m.target, slot)),
            "Preview slot overlaps a mount destination"
        );
        let backing = mounts
            .iter()
            .filter(|m| under(slot, &m.target))
            .max_by_key(|m| m.target.len());
        if let Some(backing) = backing {
            ensure!(
                backing.tmpfs_rw,
                "Preview slot is inside source-backed or read-only mount ancestry"
            );
        }
        let parts: Vec<_> = slot.trim_start_matches('/').split('/').collect();
        let mut prefix = String::new();
        if mounts.iter().any(|m| m.target == "/") {
            return Ok(());
        }
        for (index, part) in parts.iter().enumerate() {
            prefix.push('/');
            prefix.push_str(part);
            if mounts.iter().any(|m| m.target == prefix) {
                return Ok(());
            }
            match self.entry(&prefix)? {
                Entry::Link(_) => anyhow::bail!("Preview destination has image symlink ancestry"),
                Entry::File if index + 1 < parts.len() => {
                    anyhow::bail!("Preview destination has non-directory ancestry")
                }
                Entry::Directory if index + 1 == parts.len() => {
                    anyhow::bail!("Preview destination is a directory")
                }
                Entry::Missing => return Ok(()),
                _ => {}
            }
        }
        Ok(())
    }
}
fn model_mounts(
    model: &Value,
    name: &str,
    seen: &mut BTreeSet<String>,
    external: &BTreeMap<String, Value>,
) -> Result<Vec<Mount>> {
    ensure!(
        seen.insert(name.into()) && seen.len() <= 128,
        "Cyclic or oversized volumes_from closure"
    );
    let service = &model["services"][name];
    ensure!(service.is_object(), "Missing volumes_from provider {name}");
    let mut result = Vec::new();
    for mount in service["volumes"].as_array().into_iter().flatten() {
        result.push(target_mount(mount)?);
    }
    for key in ["configs", "secrets", "devices"] {
        for mount in service[key].as_array().into_iter().flatten() {
            let target = if let Some(raw) = mount.as_str() {
                if key == "devices" {
                    raw.split(':')
                        .nth(1)
                        .context("Unproved device destination")?
                        .to_owned()
                } else {
                    format!(
                        "/{}{}",
                        if key == "secrets" { "run/secrets/" } else { "" },
                        raw
                    )
                }
            } else {
                mount["target"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!(
                            "/{}{}",
                            if key == "secrets" { "run/secrets/" } else { "" },
                            mount["source"].as_str().unwrap_or("")
                        )
                    })
            };
            result.push(Mount {
                target,
                tmpfs_rw: false,
                backing: Backing::Opaque,
                implicit: false,
            });
        }
    }
    for raw in service["tmpfs"].as_array().into_iter().flatten() {
        let raw = raw.as_str().context("Unproved tmpfs destination")?;
        let (target, options) = raw.split_once(':').unwrap_or((raw, ""));
        result.push(Mount {
            target: target.into(),
            tmpfs_rw: !options.split(',').any(|s| s == "ro"),
            backing: Backing::Tmpfs,
            implicit: false,
        });
    }
    for raw in service["volumes_from"].as_array().into_iter().flatten() {
        let raw = raw.as_str().context("Invalid volumes_from reference")?;
        let (reference, ro) = raw
            .strip_suffix(":ro")
            .map(|s| (s, true))
            .or_else(|| raw.strip_suffix(":rw").map(|s| (s, false)))
            .unwrap_or((raw, false));
        let mut inherited = if let Some(id) = reference.strip_prefix("container:") {
            external
                .get(id)
                .context("Missing captured external volume provider")?["mounts"]
                .as_array()
                .context("Invalid provider mounts")?
                .iter()
                .map(|v| {
                    Ok(Mount {
                        target: v["Destination"]
                            .as_str()
                            .context("Invalid provider destination")?
                            .into(),
                        tmpfs_rw: false,
                        backing: if v["Type"] == "bind" {
                            Backing::Bind(
                                v["Source"]
                                    .as_str()
                                    .context("Unproved external bind source")?
                                    .into(),
                            )
                        } else {
                            Backing::Opaque
                        },
                        implicit: false,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            model_mounts(model, reference, seen, external)?
        };
        if ro {
            for mount in &mut inherited {
                mount.tmpfs_rw = false;
            }
        }
        inherited.extend(result);
        result = inherited;
    }
    seen.remove(name);
    Ok(result)
}

pub struct Proof {
    pub slots: BTreeMap<String, String>,
    pub images: BTreeMap<String, String>,
    pub external: BTreeMap<String, String>,
}
impl Proof {
    pub fn service_overlay(&self, model: &Value, service: &str) -> Result<String> {
        let Some(image) = self.images.get(service) else {
            return Ok(String::new());
        };
        let mut out = format!(
            "    image: {}\n    build: !reset null\n    pull_policy: never\n",
            serde_json::to_string(image)?
        );
        if let Some(platform) = model["services"][service].get("platform") {
            out += &format!("    platform: {}\n", serde_json::to_string(platform)?);
        }
        if let Some(references) = model["services"][service]["volumes_from"].as_array() {
            let mut pinned = Vec::new();
            for raw in references {
                let raw = raw.as_str().context("Invalid volume provider")?;
                if let Some(rest) = raw.strip_prefix("container:") {
                    let (name, mode) = rest
                        .rsplit_once(':')
                        .filter(|(_, m)| ["ro", "rw"].contains(m))
                        .unwrap_or((rest, ""));
                    let id = self
                        .external
                        .get(name)
                        .context("Uncaptured external provider")?;
                    pinned.push(format!(
                        "container:{id}{}",
                        if mode.is_empty() {
                            String::new()
                        } else {
                            format!(":{mode}")
                        }
                    ));
                } else {
                    pinned.push(raw.into());
                }
            }
            out += &format!(
                "    volumes_from: !override {}\n",
                serde_json::to_string(&pinned)?
            );
        }
        Ok(out)
    }
}
/// Resolve acquisition closure before the durable intent is installed.
pub fn closure(model: &Value, active: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    dependency_closure(model, active, true)
}
fn dependency_closure(
    model: &Value,
    active: &BTreeSet<String>,
    include_build: bool,
) -> Result<BTreeSet<String>> {
    let mut all = active.clone();
    loop {
        let before = all.len();
        for name in all.clone() {
            let service = &model["services"][&name];
            ensure!(service.is_object(), "Missing inspection service {name}");
            for raw in service["volumes_from"].as_array().into_iter().flatten() {
                let raw = raw.as_str().context("Invalid volumes_from")?;
                if !raw.starts_with("container:") {
                    all.insert(raw.trim_end_matches(":ro").trim_end_matches(":rw").into());
                }
            }
            for raw in service
                .pointer("/build/additional_contexts")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|m| m.values())
            {
                if include_build
                    && let Some(name) = raw.as_str().and_then(|s| s.strip_prefix("service:"))
                {
                    all.insert(name.into());
                }
            }
        }
        ensure!(
            all.len() <= 128,
            "Image acquisition closure exceeds service budget"
        );
        if before == all.len() {
            break;
        }
    }
    Ok(all)
}

pub fn prepare(
    ports: &mut super::ports::Ports,
    model: &Value,
    active: &BTreeSet<String>,
    frontends: &BTreeSet<String>,
    references: &BTreeMap<String, String>,
    ledger: &mut Ledger,
    runner: &ProcessRunner,
) -> Result<Proof> {
    let root_path = ports.owner.root.clone();
    let root = root_path.as_path();
    recover(root, ledger, runner)?;
    let all = closure(model, active)?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let mut helpers = BTreeMap::new();
    let mut services = serde_json::Map::new();
    let mut external = BTreeMap::new();
    let mut external_ids = BTreeMap::new();
    for name in &all {
        let original = &model["services"][name];
        for raw in original["volumes_from"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(rest) = raw.strip_prefix("container:") {
                let reference = rest.trim_end_matches(":ro").trim_end_matches(":rw");
                if !external.contains_key(reference) {
                    let output = runner.capture(Command::new("docker").args([
                        "container",
                        "inspect",
                        "--format",
                        "{{json .Id}}\n{{json .Mounts}}",
                        reference,
                    ]))?;
                    let lines = output
                        .text()?
                        .lines()
                        .map(serde_json::from_str)
                        .collect::<std::result::Result<Vec<Value>, _>>()?;
                    ensure!(lines.len() == 2, "Invalid captured external provider");
                    let id = lines[0].as_str().context("Invalid provider ID")?;
                    ensure!(
                        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                        "Invalid immutable provider ID"
                    );
                    external.insert(reference.to_owned(), json!({"id":id,"mounts":lines[1]}));
                    external_ids.insert(reference.into(), id.into());
                }
            }
        }
        let helper = InspectionHelper {
            name: format!("devkit-inspect-{}-{}", nonce, helpers.len()),
            token: nonce.clone(),
            role: if active.contains(name) {
                "runtime-image"
            } else {
                "build-context-image"
            }
            .into(),
            observed: None,
        };
        let mut service = json!({"image":references.get(name).context("Missing original effective image reference")?,"container_name":helper.name,
            "labels":{TOKEN:nonce,ROLE:helper.role,OWNER:ledger.owner.key},"entrypoint":["/devkit-inspection-never-start"],"network_mode":"none","read_only":true,"cap_drop":["ALL"],"restart":"no","scale":1});
        for key in ["build", "platform", "pull_policy"] {
            if let Some(v) = original.get(key) {
                service[key] = v.clone();
            }
        }
        services.insert(name.clone(), service);
        helpers.insert(name.clone(), helper);
    }
    let mut projection = json!({"services":services});
    if let Some(secrets) = model.get("secrets") {
        projection["secrets"] = secrets.clone();
    }
    escape_compose(&mut projection);
    let bytes = json_bytes(&projection)?;
    ports.possible_actors = true;
    ports.save()?;
    ledger.inspections.insert(
        nonce.clone(),
        InspectionIntent {
            nonce: nonce.clone(),
            model_sha256: digest(json_bytes(model)?),
            references: references.clone(),
            external: external_ids.clone(),
            helpers,
            creation_finished: false,
        },
    );
    ledger.save(root)?;
    super::epoch::anchor(root)?;
    let mut command = Command::new("docker");
    command
        .current_dir(root)
        .args([
            "compose",
            "--project-name",
            &format!("devkit-inspection-{}", nonce),
            "--project-directory",
        ])
        .arg(root)
        .args(["--file", "-", "create", "--pull", "missing"]);
    for name in active {
        command.arg(name);
    }
    let mut reaped = false;
    let result = runner.run_tracked(
        &mut command,
        OutputMode::TeeStderr(65536),
        Some(&bytes),
        &mut reaped,
    );
    ledger
        .inspections
        .get_mut(&nonce)
        .unwrap()
        .creation_finished = reaped;
    ledger.save(root)?;
    let proof = (|| -> Result<Proof> {
        result?.success("Image-only Compose acquisition")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let total_queries = Cell::new(0);
        let mut images = BTreeMap::new();
        let mut namespaces = BTreeMap::new();
        let mut image_mounts = BTreeMap::new();
        for name in &all {
            let helper = &ledger.inspections[&nonce].helpers[name];
            let snapshot = runner.inspection_snapshot(
                &owned(helper),
                &labels(&ledger.owner, helper),
                deadline,
            )?;
            let Some(snapshot) = snapshot else {
                ensure!(
                    !active.contains(name),
                    "Missing planned runtime image inspector"
                );
                continue;
            };
            if !active.contains(name) {
                ledger
                    .inspections
                    .get_mut(&nonce)
                    .unwrap()
                    .helpers
                    .get_mut(name)
                    .unwrap()
                    .observed = Some(snapshot);
                ledger.save(root)?;
                continue;
            }
            let mounts = snapshot
                .anonymous_volumes
                .keys()
                .map(|target| Mount {
                    target: target.clone(),
                    tmpfs_rw: false,
                    backing: Backing::Opaque,
                    implicit: true,
                })
                .collect::<Vec<_>>();
            let output = runner.capture(Command::new("docker").args([
                "image",
                "inspect",
                "--format",
                "{{json .Id}}\n{{json .Os}}\n{{json .Architecture}}\n{{json (index .Config \"Volumes\")}}",
                &snapshot.image,
            ]))?;
            let info = output
                .text()?
                .lines()
                .map(serde_json::from_str)
                .collect::<std::result::Result<Vec<Value>, _>>()?;
            ensure!(
                info.len() == 4
                    && info[0] == snapshot.image
                    && info[1] == "linux"
                    && info[2].as_str().is_some_and(|s| !s.is_empty()),
                "Unproved immutable image platform"
            );
            if let Some(platform) = model["services"][name]["platform"].as_str() {
                let parts: Vec<_> = platform.split('/').collect();
                ensure!(
                    parts.len() >= 2
                        && Some(parts[0]) == info[1].as_str()
                        && Some(parts[1]) == info[2].as_str(),
                    "Inspected image differs from requested platform"
                );
            }
            let expected: BTreeSet<_> = info[3]
                .as_object()
                .into_iter()
                .flat_map(|m| m.keys())
                .cloned()
                .collect();
            ensure!(
                expected == snapshot.anonymous_volumes.keys().cloned().collect(),
                "Inspector image volume inventory is incomplete"
            );
            images.insert(name.clone(), snapshot.image.clone());
            image_mounts.insert(name.clone(), mounts);
            namespaces.insert(
                name.clone(),
                Namespace {
                    runner,
                    id: snapshot.id.clone(),
                    cache: BTreeMap::new(),
                    queries: &total_queries,
                    deadline,
                },
            );
            ledger
                .inspections
                .get_mut(&nonce)
                .unwrap()
                .helpers
                .get_mut(name)
                .unwrap()
                .observed = Some(snapshot);
            ledger.save(root)?;
        }
        // Retained anonymous mounts are hazards even when a fresh inspector's copy is empty.
        let retained = super::ledger::observe(
            model["name"]
                .as_str()
                .context("Missing Compose project name")?,
            runner,
        )?;
        super::ledger::verify_owners(&retained, &ledger.owner)?;
        let previous = super::ports::Ports::read(root)?.context("Missing stand lease intent")?;
        ensure!(
            previous.owner == ledger.owner,
            "Stand inspection owner changed"
        );
        let mut slots = BTreeMap::new();
        for name in frontends {
            let mut mounts = model_mounts(model, name, &mut BTreeSet::new(), &external)?;
            for provider in dependency_closure(model, &BTreeSet::from([name.clone()]), false)? {
                mounts.extend(image_mounts[&provider].clone());
            }
            mounts.sort_by_key(|m| !m.implicit);
            let mut current = BTreeMap::new();
            for mount in mounts {
                current.insert(normalize(&mount.target)?, mount);
            }
            let mut mounts: Vec<_> = current.into_values().collect();
            for row in retained.iter().filter(|r| r.service == *name) {
                for mount in row.mounts.as_array().into_iter().flatten() {
                    let target = mount["Destination"]
                        .as_str()
                        .context("Unproved retained destination")?;
                    // A verified previous carrier is our control file, not an
                    // opaque persistent user namespace blocking its own slot.
                    if let Some(deployment) = previous.deployments.get(&row.deployment)
                        && let Some(file) = deployment.files.iter().find(|file| {
                            file.service == *name
                                && file.slot == target
                                && mount["Type"] == "bind"
                                && mount["Source"].as_str() == file.source.to_str()
                                && mount["RW"] == false
                        })
                    {
                        ensure!(
                            digest(json_bytes(deployment)?) == row.deployment,
                            "Applied deployment receipt changed"
                        );
                        file.validate(root, &ledger.owner)?;
                        ensure!(
                            deployment.images.get(name) == Some(&row.image),
                            "Retained carrier image differs from its recorded proof"
                        );
                        ensure!(
                            row.preview_env.get("DEVKIT_PREVIEW_HOSTS_FILE") == Some(&file.slot)
                                && row.preview_env.get("DEVKIT_PREVIEW_HOSTS_SHA256")
                                    == Some(&file.sha256),
                            "Retained carrier environment changed"
                        );
                        continue;
                    }
                    let tmpfs_rw = mount["Type"] == "tmpfs"
                        && mount["RW"] == true
                        && mounts
                            .iter()
                            .any(|current| current.target == target && current.tmpfs_rw);
                    let backing = if mount["Type"] == "bind" {
                        Backing::Bind(
                            mount["Source"]
                                .as_str()
                                .context("Unproved retained bind source")?
                                .into(),
                        )
                    } else if mount["Type"] == "tmpfs" {
                        Backing::Tmpfs
                    } else {
                        Backing::Opaque
                    };
                    ensure!(
                        !mounts.iter().any(|current| normalize(&current.target)
                            .is_ok_and(|p| p == target)
                            && current.backing != backing),
                        "Current and retained mount backing differ; stop the stand before changing its namespace"
                    );
                    mounts.push(Mount {
                        target: target.into(),
                        tmpfs_rw,
                        backing: if mount["Type"] == "bind" {
                            Backing::Bind(
                                mount["Source"]
                                    .as_str()
                                    .context("Unproved retained bind source")?
                                    .into(),
                            )
                        } else if mount["Type"] == "tmpfs" {
                            Backing::Tmpfs
                        } else {
                            Backing::Opaque
                        },
                        implicit: false,
                    });
                }
            }
            let ns = namespaces
                .get_mut(name)
                .context("Missing frontend image namespace")?;
            // Explicit runtime mounts override image-declared volume defaults.
            mounts.sort_by_key(|m| !m.implicit);
            let mut effective = BTreeMap::new();
            for mount in mounts {
                effective.insert(normalize(&mount.target)?, mount);
            }
            let mut ordered: Vec<_> = effective.into_values().collect();
            ordered.sort_by_key(|m| m.target.split('/').filter(|s| !s.is_empty()).count());
            let mut canonical: Vec<Mount> = Vec::new();
            for mount in ordered {
                let target = ns.mount_destination(&mount.target, &canonical)?;
                if let Some(previous) = canonical.iter().find(|m| m.target == target) {
                    ensure!(
                        previous.backing == mount.backing,
                        "Mount aliases have conflicting backing namespaces"
                    );
                    continue;
                }
                canonical.push(Mount { target, ..mount });
            }
            let mut selected = None;
            for slot in SLOTS {
                if ns.literal_slot(slot, &canonical).is_ok() {
                    selected = Some(slot.into());
                    break;
                }
            }
            slots.insert(name.clone(),selected.context("No provably safe preview metadata slot; reserve /run/devkit or /devkit outside project, volume and image symlink namespaces")?);
        }
        Ok(Proof {
            slots,
            images,
            external: external_ids,
        })
    })();
    // Keep the stopped helpers until the caller finishes the actual create phase.
    if proof.is_err() {
        let cleanup = recover(root, ledger, runner);
        if let Err(error) = cleanup {
            eprintln!("devkit: image inspection recovery retained: {error:#}");
        }
    }
    proof
}

pub fn verify_launch(
    model: &Value,
    ports: &super::ports::Ports,
    active: &BTreeSet<String>,
    proof: &Proof,
) -> Result<()> {
    for service in active {
        let actual = &model["services"][service];
        if let Some(image) = proof.images.get(service) {
            ensure!(
                actual["image"] == *image
                    && actual.get("build").is_none_or(Value::is_null)
                    && actual["pull_policy"] == "never",
                "Launch model escaped inspected image authority for {service}"
            );
        }
    }
    if let Some(deployment) = ports
        .desired_deployment
        .as_ref()
        .and_then(|id| ports.deployments.get(id))
    {
        for file in &deployment.files {
            file.validate(&ports.owner.root, &ports.owner)?;
            let service = &model["services"][&file.service];
            ensure!(
                service["environment"]["DEVKIT_PREVIEW_HOSTS"] == ""
                    && service["environment"]["DEVKIT_PREVIEW_HOSTS_FILE"] == file.slot
                    && service["environment"]["DEVKIT_PREVIEW_HOSTS_SHA256"] == file.sha256
                    && service["environment"]["DEVKIT_PREVIEW_URL"].as_str()
                        == file.primary_url.as_deref(),
                "Launch preview environment changed after normalization"
            );
            ensure!(
                service["volumes"].as_array().is_some_and(|mounts| mounts
                    .iter()
                    .filter(|m| m["target"] == file.slot)
                    .count()
                    == 1
                    && mounts.iter().any(|m| m["type"] == "bind"
                        && m["source"].as_str() == file.source.to_str()
                        && m["target"] == file.slot
                        && m["read_only"] == true
                        && m.pointer("/bind/create_host_path") == Some(&Value::Bool(false)))),
                "Launch metadata is not the proved read-only file bind"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn mounted_namespaces_shadow_image_paths_and_opaque_ancestry_never_proves_safe() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("child/nested")).unwrap();
        std::fs::create_dir(root.path().join("devkit")).unwrap();
        let runner = ProcessRunner::new().unwrap();
        let queries = Cell::new(0);
        let mut ns = Namespace {
            runner: &runner,
            id: "a".repeat(64),
            cache: BTreeMap::from([
                ("/workspace".into(), Entry::Directory),
                ("/workspace/child".into(), Entry::Link("/run".into())),
                ("/run".into(), Entry::Directory),
                ("/run/devkit".into(), Entry::Link("/workspace".into())),
                ("/bridge".into(), Entry::Link("/run".into())),
                ("/devkit".into(), Entry::Missing),
            ]),
            queries: &queries,
            deadline: Instant::now() + Duration::from_secs(3),
        };
        let bind = Mount {
            target: "/workspace".into(),
            tmpfs_rw: false,
            backing: Backing::Bind(root.path().into()),
            implicit: false,
        };
        assert_eq!(
            ns.mount_destination("/workspace/child/nested", std::slice::from_ref(&bind))
                .unwrap(),
            "/workspace/child/nested"
        );
        std::fs::remove_dir_all(root.path().join("child")).unwrap();
        symlink("/run", root.path().join("child")).unwrap();
        assert!(
            ns.mount_destination("/workspace/child/nested", &[bind])
                .is_err()
        );
        let opaque = Mount {
            target: "/store".into(),
            tmpfs_rw: false,
            backing: Backing::Opaque,
            implicit: false,
        };
        assert!(ns.mount_destination("/store/nested", &[opaque]).is_err());
        let outer = Mount {
            target: "/run".into(),
            tmpfs_rw: false,
            backing: Backing::Bind(root.path().into()),
            implicit: false,
        };
        let tmpfs = Mount {
            target: ns
                .mount_destination("/run/devkit", std::slice::from_ref(&outer))
                .unwrap(),
            tmpfs_rw: true,
            backing: Backing::Tmpfs,
            implicit: false,
        };
        ns.literal_slot(SLOTS[0], &[outer, tmpfs]).unwrap();
        let alias = Mount {
            target: ns.mount_destination("/bridge", &[]).unwrap(),
            tmpfs_rw: false,
            backing: Backing::Bind(root.path().into()),
            implicit: false,
        };
        assert_eq!(alias.target, "/run");
        assert!(
            ns.literal_slot(SLOTS[0], std::slice::from_ref(&alias))
                .is_err()
        );
        ns.literal_slot(SLOTS[1], &[alias]).unwrap();
    }
}
