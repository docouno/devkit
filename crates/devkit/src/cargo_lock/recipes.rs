//! Compose known resolutions. A Cargo.lock has no feature declarations for
//! its transitive packages, so equal package versions alone cannot establish
//! that combining two resolutions preserves their edges.
use super::{CargoPackageId, Lock, Record};
use crate::{
    Result,
    native::{CargoWorkspaceModel, cargo_template_recipe},
    release::AdmittedRelease,
};
use std::collections::{BTreeMap, BTreeSet};

fn external_edges(record: &Record) -> BTreeSet<CargoPackageId> {
    record
        .dependencies
        .iter()
        .filter(|id| id.source.is_some())
        .cloned()
        .collect()
}

pub(super) fn extend(
    model: &CargoWorkspaceModel,
    release: &AdmittedRelease,
    original: &Lock,
    added: &BTreeSet<String>,
    graph: &mut Lock,
) -> Result<bool> {
    // The caller has already established that all earlier manifests and
    // resolution policy are unchanged except for plain wires to inert members.
    if added.is_empty() {
        return Ok(true);
    }
    let mut witnesses = BTreeMap::new();
    for (component, members) in &model.components {
        if added.contains(component) {
            continue;
        }
        for member in members {
            if let (Some(recipe), Some(record)) = (
                model.member_recipes.get(&member.rel_dir),
                original.packages.get(&member.id),
            ) {
                witnesses.insert(recipe.clone(), external_edges(record));
            }
        }
    }
    let mut selections = Vec::new();
    for component in added {
        for member in model.members(component)? {
            if model.inert_members.contains(&member.rel_dir) {
                continue;
            }
            let Some(recipe) = model.member_recipes.get(&member.rel_dir) else {
                return Ok(false);
            };
            if !model.patches.is_empty() || !model.replacements.is_empty() {
                return Ok(false);
            }
            let Some(existing) = witnesses.get(recipe) else {
                // The release must witness the whole combination. Even
                // disjoint package IDs can compete for a native links name.
                return matches_release_recipe_graph(model, release, graph);
            };
            // This member requests the same external features and targets
            // as an accepted member, apart from paths to inert members.
            if let Some(seed) = graph.packages.get(&member.id)
                && external_edges(seed) != *existing
            {
                return Ok(false);
            }
            selections.push((member.id.clone(), existing.clone()));
        }
    }
    for (id, selected) in selections {
        graph
            .packages
            .entry(id)
            .or_default()
            .dependencies
            .extend(selected);
    }
    Ok(true)
}

/// The 0.5 API template requested the same features with exact versions.
/// Its 0.6 successor carries the same release lock and caret requirements.
fn legacy_exact_recipe(bytes: &[u8]) -> Result<String> {
    let mut manifest: toml::Table = toml::from_str(std::str::from_utf8(bytes)?)?;
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(deps) = manifest.get_mut(table).and_then(toml::Value::as_table_mut) else {
            continue;
        };
        for (_, spec) in deps.iter_mut() {
            let requirement = if spec.is_table() {
                spec.get_mut("version")
            } else {
                Some(spec)
            };
            if let Some(value) = requirement
                && let Some(version) = value.as_str()
                && semver::Version::parse(version).is_ok()
            {
                *value = toml::Value::String(format!("={version}"));
            }
        }
    }
    cargo_template_recipe(toml::to_string(&manifest)?.as_bytes())
}

/// Import an existing selection only if a release recipe supplies the entire
/// transitive graph, including all feature-dependent edges and checksums.
/// Pure local components may coexist with any number of copies of that recipe.
pub(crate) fn matches_release_recipe_graph(
    model: &CargoWorkspaceModel,
    release: &AdmittedRelease,
    graph: &Lock,
) -> Result<bool> {
    if !model.recipe_workspace || !graph.unused.is_empty() {
        return Ok(false);
    }
    let opaque: Vec<_> = model
        .components
        .values()
        .flatten()
        .filter(|m| !model.inert_members.contains(&m.rel_dir))
        .collect();
    if opaque.is_empty() {
        return Ok(false);
    }
    for template in ["rust-api", "rust-library"] {
        let manifest = release.bytes(&format!("templates/{template}/Cargo.toml"))?;
        let current = cargo_template_recipe(&manifest)?;
        let legacy = legacy_exact_recipe(&manifest)?;
        if !opaque.iter().all(|m| {
            model
                .member_recipes
                .get(&m.rel_dir)
                .is_some_and(|recipe| *recipe == current || *recipe == legacy)
        }) {
            continue;
        }
        let seed = Lock::parse(&release.bytes(&format!("templates/{template}/Cargo.lock"))?)?;
        let mut locals = seed.packages.iter().filter(|(id, _)| id.source.is_none());
        let Some((_, root)) = locals.next() else {
            continue;
        };
        if locals.next().is_some() || !seed.unused.is_empty() {
            continue;
        }
        let edges = external_edges(root);
        let members_match = opaque.iter().all(|m| {
            graph
                .packages
                .get(&m.id)
                .is_some_and(|record| external_edges(record) == edges)
        });
        if members_match
            && seed
                .packages
                .iter()
                .filter(|(id, _)| id.source.is_some())
                .eq(graph.packages.iter().filter(|(id, _)| id.source.is_some()))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
