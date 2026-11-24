//! Cargo locks are merged as graphs of fully qualified package identities. A
//! compact dependency such as `syn` only has meaning inside its source lock.
use crate::{
    Result,
    native::{CargoPackageId, CargoWorkspaceModel},
};
use anyhow::{Context, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

mod recipes;
mod reconcile;
mod resolver;
pub(crate) use recipes::matches_release_recipe_graph;
pub(crate) use reconcile::{reconcile, resolved_model};
pub use resolver::{CargoLockResolver, DockerCargoLockResolver};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLock {
    version: u32,
    package: Vec<RawPackage>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    patch: Patches,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Patches {
    #[serde(default)]
    unused: Vec<RawPackage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    replace: Option<String>,
}

fn source_v4(source: &str, version: u32, package_record: bool) -> Result<String> {
    if version == 3
        && source.starts_with("git+")
        && let Some((url, query)) = source.split_once('?')
    {
        // v3 wrote the Git ref literally, including '+', '%', '&' and '#'.
        // Only a package record ends in a precise commit. A dependency
        // reference contains just the source and the literal ref name.
        let (query, precise) = if package_record {
            query.rsplit_once('#').unwrap_or((query, ""))
        } else {
            (query, "")
        };
        let (kind, value) = query
            .split_once('=')
            .context("Malformed v3 Git source reference")?;
        ensure!(
            ["branch", "tag", "rev"].contains(&kind),
            "Unknown Git reference in {source}"
        );
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(kind, value)
            .finish();
        return Ok(if precise.is_empty() {
            format!("{url}?{query}")
        } else {
            format!("{url}?{query}#{precise}")
        });
    }
    Ok(source.to_owned())
}

pub(crate) fn reference_source(source: &str) -> &str {
    if source.starts_with("git+")
        && let Some((base, _)) = source.split_once('#')
    {
        base
    } else {
        source
    }
}

fn source_rank(source: Option<&str>) -> u8 {
    match source {
        Some(s) if s.starts_with("git+") => 0,
        None => 1,
        Some(s) if s.starts_with("registry+") => 2,
        Some(s) if s.starts_with("sparse+") => 3,
        Some(_) => 4,
    }
}

fn cargo_sort_key(id: &CargoPackageId) -> Result<(&str, Version, u8, Option<&str>)> {
    Ok((
        id.name.as_str(),
        Version::parse(&id.version)?,
        source_rank(id.source.as_deref()),
        id.source.as_deref(),
    ))
}

impl RawPackage {
    fn id(&self, version: u32) -> Result<CargoPackageId> {
        Version::parse(&self.version)
            .with_context(|| format!("Invalid version for {}", self.name))?;
        ensure!(
            !self.name.is_empty() && !self.name.chars().any(char::is_whitespace),
            "Invalid locked Cargo package name"
        );
        Ok(CargoPackageId {
            name: self.name.clone(),
            version: self.version.clone(),
            source: self
                .source
                .as_deref()
                .map(|s| source_v4(s, version, true))
                .transpose()?,
        })
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    checksum: Option<String>,
    pub(crate) dependencies: BTreeSet<CargoPackageId>,
    pub(crate) replacement: Option<CargoPackageId>,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct UnusedPatch {
    checksum: Option<String>,
    occurrences: usize,
}

impl UnusedPatch {
    fn merge_checksum(&mut self, checksum: Option<&str>) -> Result<()> {
        if let Some(checksum) = checksum {
            let known = self.checksum.get_or_insert_with(|| checksum.to_owned());
            ensure!(
                known == checksum,
                "Conflicting checksums for an unused Cargo patch"
            );
        }
        Ok(())
    }
}

type UnusedPatches = BTreeMap<CargoPackageId, UnusedPatch>;

#[derive(Default)]
struct PatchUsage {
    unused: UnusedPatches,
    selected: BTreeMap<CargoPackageId, Option<String>>,
}

fn add_unused(
    unused: &mut UnusedPatches,
    id: CargoPackageId,
    checksum: Option<&str>,
) -> Result<()> {
    let patch = unused.entry(id).or_default();
    patch.merge_checksum(checksum)?;
    patch.occurrences += 1;
    Ok(())
}

#[derive(Default, Clone, PartialEq, Eq)]
pub(crate) struct Lock {
    pub(crate) packages: BTreeMap<CargoPackageId, Record>,
    unused: UnusedPatches,
    metadata: BTreeMap<String, String>,
}

/// Index both concise and fully qualified references. Cargo's `name version`
/// points to the local record in a local/external name-and-version collision.
struct References {
    index: BTreeMap<String, Option<CargoPackageId>>,
}
impl References {
    fn new<'a>(ids: impl Iterator<Item = &'a CargoPackageId>) -> Self {
        let mut index = BTreeMap::new();
        let mut local = Vec::new();
        for id in ids {
            let nv = format!("{} {}", id.name, id.version);
            for key in [&id.name, &nv] {
                index
                    .entry(key.clone())
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(id.clone()));
            }
            if let Some(source) = &id.source {
                index
                    .entry(format!("{nv} ({})", reference_source(source)))
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(id.clone()));
            } else {
                local.push((nv, Some(id.clone())));
            }
        }
        index.extend(local);
        Self { index }
    }
    fn resolve(&self, reference: &str, version: u32) -> Result<CargoPackageId> {
        let key = if version == 3
            && let Some((name, source)) = reference.split_once(" (")
        {
            format!(
                "{name} ({})",
                source_v4(
                    source
                        .strip_suffix(')')
                        .context("Malformed lock reference")?,
                    version,
                    false,
                )?
            )
        } else {
            reference.to_owned()
        };
        self.index
            .get(&key)
            .and_then(Option::as_ref)
            .cloned()
            .with_context(|| {
                format!("Cargo lock dependency reference is missing or ambiguous: {reference}")
            })
    }
    fn compact(&self, id: &CargoPackageId) -> String {
        if self.index[&id.name].is_some() {
            return id.name.clone();
        }
        let nv = format!("{} {}", id.name, id.version);
        if self.index[&nv].as_ref() == Some(id) {
            nv
        } else if let Some(source) = &id.source {
            format!("{nv} ({})", reference_source(source))
        } else {
            nv
        }
    }
}

impl Lock {
    pub(crate) fn parse(data: &[u8]) -> Result<Self> {
        let raw: RawLock = toml::from_str(std::str::from_utf8(data)?)?;
        ensure!(
            matches!(raw.version, 3 | 4),
            "Expected Cargo.lock format 3 or 4"
        );
        let ids = raw
            .package
            .iter()
            .map(|p| p.id(raw.version))
            .collect::<Result<Vec<_>>>()?;
        let refs = References::new(ids.iter());
        let mut output = Self {
            metadata: raw.metadata,
            ..Self::default()
        };
        for (id, p) in ids.into_iter().zip(raw.package) {
            let record = Record {
                checksum: p.checksum,
                dependencies: p
                    .dependencies
                    .iter()
                    .map(|d| refs.resolve(d, raw.version))
                    .collect::<Result<_>>()?,
                replacement: p
                    .replace
                    .as_deref()
                    .map(|d| refs.resolve(d, raw.version))
                    .transpose()?,
            };
            ensure!(
                record.replacement.is_none() || record.dependencies.is_empty(),
                "Locked package {} has both replace and dependencies",
                id.name
            );
            ensure!(
                output.packages.insert(id.clone(), record).is_none(),
                "Duplicate Cargo.lock package {} {}",
                id.name,
                id.version
            );
        }
        for patch in raw.patch.unused {
            ensure!(
                patch.replace.is_none() && patch.dependencies.is_empty(),
                "Invalid unused Cargo patch"
            );
            let id = patch.id(raw.version)?;
            add_unused(&mut output.unused, id, patch.checksum.as_deref())?;
        }
        Ok(output)
    }

    pub(crate) fn bytes(&self) -> Result<Vec<u8>> {
        let references = References::new(self.packages.keys());
        let mut text = String::from(
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n",
        );
        let mut packages = self
            .packages
            .iter()
            .map(|(id, record)| Ok((cargo_sort_key(id)?, id, record)))
            .collect::<Result<Vec<_>>>()?;
        packages.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, id, record) in packages {
            text.push_str("\n[[package]]\n");
            render_id(&mut text, id, record.checksum.as_deref());
            if let Some(replace) = &record.replacement {
                field(&mut text, "replace", &references.compact(replace));
            }
            if !record.dependencies.is_empty() {
                text.push_str("dependencies = [\n");
                for reference in record
                    .dependencies
                    .iter()
                    .map(|d| references.compact(d))
                    .collect::<BTreeSet<_>>()
                {
                    text.push_str(&format!(" {},\n", quote(&reference)));
                }
                text.push_str("]\n");
            }
        }
        let mut unused = self
            .unused
            .iter()
            .map(|(id, patch)| Ok((cargo_sort_key(id)?, id, patch)))
            .collect::<Result<Vec<_>>>()?;
        unused.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, id, patch) in unused {
            for _ in 0..patch.occurrences {
                text.push_str("\n[[patch.unused]]\n");
                render_id(&mut text, id, patch.checksum.as_deref());
            }
        }
        if !self.metadata.is_empty() {
            #[derive(Serialize)]
            struct Metadata<'a> {
                metadata: &'a BTreeMap<String, String>,
            }
            text.push('\n');
            text.push_str(&toml::to_string(&Metadata {
                metadata: &self.metadata,
            })?);
        }
        Ok(text.into_bytes())
    }
}
fn quote(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}
fn field(out: &mut String, key: &str, value: &str) {
    out.push_str(&format!("{key} = {}\n", quote(value)));
}
fn render_id(out: &mut String, id: &CargoPackageId, checksum: Option<&str>) {
    field(out, "name", &id.name);
    field(out, "version", &id.version);
    if let Some(source) = &id.source {
        field(out, "source", source);
    }
    if let Some(checksum) = checksum {
        field(out, "checksum", checksum);
    }
}

/// Cargo's activation key uses the leftmost nonzero release number, including
/// `0.0.0` in the patch class. Pre-release and build qualifiers do not split it.
fn compatibility(ver: &Version) -> (u8, u64) {
    if ver.major != 0 {
        (2, ver.major)
    } else if ver.minor != 0 {
        (1, ver.minor)
    } else {
        (0, ver.patch)
    }
}

type Activation<'a> = (&'a str, &'a str, (u8, u64));

fn activation(id: &CargoPackageId) -> Result<Option<Activation<'_>>> {
    // Cargo.lock omits the source for path packages. Distinct paths may
    // select semver-compatible versions of the same crate.
    let Some(source) = id.source.as_deref() else {
        return Ok(None);
    };
    Ok(Some((
        &id.name,
        reference_source(source),
        compatibility(&Version::parse(&id.version)?),
    )))
}

enum MergeConflict {
    Versions(String),
    Dependencies(String),
}

struct Merged {
    seed: Lock,
    conflict: Option<MergeConflict>,
    redirected_winners: BTreeSet<CargoPackageId>,
}

fn merge(locks: &[(String, Lock)]) -> Result<Merged> {
    // The accepted root comes first. If another seed pins a different
    // release in its class, Cargo starts from the accepted version while
    // retaining unrelated incoming pins in the rest of the graph.
    let mut classes = BTreeMap::new();
    let mut revisions = BTreeMap::new();
    let mut checksums = BTreeMap::new();
    let mut conflict = None;
    let mut redirected = BTreeMap::new();
    let mut redirected_winners = BTreeSet::new();
    for (label, lock) in locks {
        for (id, record) in &lock.packages {
            if let Some((old_label, old)) = checksums.insert(id, (label, &record.checksum)) {
                ensure!(
                    old == &record.checksum,
                    "{old_label} and {label} disagree on the checksum for {} {}",
                    id.name,
                    id.version
                );
            }
            if let Some(source) = &id.source
                && source.starts_with("git+")
            {
                let key = reference_source(source);
                if let Some((old_label, old)) = revisions.insert(key, (label, source)) {
                    ensure!(
                        old == source,
                        "{old_label} and {label} pin different revisions of {key}: {old}, {source}"
                    );
                }
            }
            if let Some(key) = activation(id)? {
                let (old_label, winner) = *classes.entry(key).or_insert((label, id));
                if winner != id {
                    conflict.get_or_insert_with(|| MergeConflict::Versions(format!(
                        "{old_label} and {label} select different versions of {} in the same compatibility class: {} and {}",
                        id.name, winner.version, id.version
                    )));
                    redirected.insert(id, winner);
                    redirected_winners.insert(winner.clone());
                }
            }
        }
    }
    let selected = |id: &CargoPackageId| redirected.get(id).copied().unwrap_or(id).clone();
    let mut result = Lock::default();
    let mut origins = BTreeMap::new();
    for (label, lock) in locks {
        for (id, record) in &lock.packages {
            if redirected.contains_key(id) {
                continue;
            }
            let record = if redirected.is_empty() {
                record.clone()
            } else {
                Record {
                    checksum: record.checksum.clone(),
                    dependencies: record.dependencies.iter().map(selected).collect(),
                    replacement: record.replacement.as_ref().map(selected),
                }
            };
            if let Some(old) = result.packages.get_mut(id) {
                if *old != record {
                    conflict.get_or_insert_with(|| {
                        MergeConflict::Dependencies(format!(
                            "{} and {label} disagree on the selected dependencies for {} {}",
                            origins[id], id.name, id.version
                        ))
                    });
                    if old.replacement.is_none() && record.replacement.is_none() {
                        old.dependencies.extend(record.dependencies);
                    }
                }
            } else {
                result.packages.insert(id.clone(), record);
                origins.insert(id.clone(), label);
            }
        }
        for (key, v) in &lock.metadata {
            if let Some(old) = result.metadata.insert(key.clone(), v.clone()) {
                ensure!(
                    old == *v,
                    "Conflicting Cargo.lock metadata {key} in {label}"
                );
            }
        }
        for (id, patch) in &lock.unused {
            let combined = result.unused.entry(id.clone()).or_default();
            combined
                .merge_checksum(patch.checksum.as_deref())
                .with_context(|| format!("Conflicting unused patch {} in {label}", id.name))?;
            combined.occurrences = combined.occurrences.max(patch.occurrences);
        }
    }
    Ok(Merged {
        seed: result,
        conflict,
        redirected_winners,
    })
}

pub(crate) fn merge_legacy_graphs(locks: &[(String, Lock)]) -> Result<Lock> {
    ensure!(
        !locks.is_empty(),
        "Missing component Cargo.lock files for migration"
    );
    let Merged { seed, conflict, .. } = merge(locks)?;
    match conflict {
        Some(MergeConflict::Versions(error)) => {
            bail!("{error}; align the locked versions on 0.5.x before upgrading")
        }
        Some(MergeConflict::Dependencies(error)) => bail!("{error}"),
        None => Ok(seed),
    }
}

impl Lock {
    pub(crate) fn unused_patch(&self, id: &CargoPackageId) -> bool {
        self.unused.contains_key(id)
    }
    pub(crate) fn bind_merged_patches(&mut self, model: &CargoWorkspaceModel) -> Result<bool> {
        let usage = model
            .patch_usage(self)?
            .context("Cannot prove unused patches in the merged Cargo workspace")?;
        // A patch unused by one component can already be selected by another.
        // Only drop that annotation for its exact selected target and checksum.
        let preserved = usage.unused.iter().all(|(id, p)| {
            self.unused
                .get(id)
                .is_some_and(|old| old.checksum == p.checksum)
        }) && self.unused.iter().all(|(id, old)| {
            usage.unused.contains_key(id)
                || usage.selected.get(id).is_some_and(|selected| {
                    // Cargo can omit the checksum of an unused registry patch.
                    old.checksum.is_none() || old.checksum == *selected
                })
        });
        ensure!(
            preserved,
            "Merging Cargo workspaces would change the identities of unused patches"
        );
        let newly_used = self.unused.len() != usage.unused.len();
        self.unused = usage.unused;
        Ok(newly_used)
    }
}
#[cfg(test)]
fn merge_legacy_locks(inputs: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    merge_legacy_graphs(&parse_inputs(inputs)?)?.bytes()
}

fn parse_inputs(inputs: &[(String, Vec<u8>)]) -> Result<Vec<(String, Lock)>> {
    inputs
        .iter()
        .map(|(path, bytes)| {
            Ok((
                path.clone(),
                Lock::parse(bytes).with_context(|| format!("Invalid {path}"))?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests;
