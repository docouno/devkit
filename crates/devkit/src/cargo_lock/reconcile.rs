use super::{
    CargoLockResolver, CargoPackageId, Lock, MergeConflict, PatchUsage, Record, activation,
    add_unused, merge, parse_inputs,
};
use crate::{
    Result,
    files::{read_regular_file, safe_path, write_atomic},
    model::{CargoResolution, ProjectState},
    native::{CargoMember, CargoWorkspaceModel},
    release::AdmittedRelease,
};
use anyhow::{Context, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
};

impl CargoWorkspaceModel {
    pub(crate) fn patches_match_lock(&self, lock: &Lock) -> Result<bool> {
        Ok(self
            .patch_usage(lock)?
            .is_some_and(|usage| usage.unused == lock.unused))
    }

    /// Cargo records a patch as used whenever its exact package is selected,
    /// even as an ordinary workspace member. An unselected local patch has
    /// a known identity. An unselected remote patch needs Cargo to resolve it.
    pub(super) fn patch_usage(&self, lock: &Lock) -> Result<Option<PatchUsage>> {
        let mut usage = PatchUsage::default();
        for patch in &self.patches {
            if let Some(path) = &patch.target.path {
                let id = &self.local_packages[path].id;
                if let Some(record) = lock.packages.get(id)
                    && self.local_package(id)?.rel_dir == *path
                {
                    usage.selected.insert(id.clone(), record.checksum.clone());
                    continue;
                }
                // A new local override competing with a selected external
                // release may change the resolution. Keep the working lock
                // as Cargo's seed instead of guessing whether it will win.
                if !lock.unused.contains_key(id)
                    && lock
                        .packages
                        .keys()
                        .any(|other| other.source.is_some() && other.name == id.name)
                {
                    return Ok(None);
                }
                add_unused(&mut usage.unused, id.clone(), None)?;
            } else {
                let mut accounted = false;
                for (id, entry) in &lock.unused {
                    if self.patch_matches_id(patch, id)? && !lock.packages.contains_key(id) {
                        add_unused(&mut usage.unused, id.clone(), entry.checksum.as_deref())?;
                        accounted = true;
                    }
                }
                if !accounted {
                    for (id, record) in &lock.packages {
                        if self.patch_matches_id(patch, id)? {
                            usage.selected.insert(id.clone(), record.checksum.clone());
                            accounted = true;
                        }
                    }
                }
                if !accounted {
                    return Ok(None);
                }
            }
        }
        Ok(Some(usage))
    }
}

/// A static edit can rewire local paths. Choosing a new registry/Git version
/// requires Cargo, even when a similarly named package is elsewhere in the lock.
fn selected_external(
    model: &CargoWorkspaceModel,
    member: &CargoMember,
    workspace_member: bool,
    old: &Record,
) -> Result<Option<BTreeSet<CargoPackageId>>> {
    let mut selected = BTreeSet::new();
    for dep in &member.dependencies {
        if dep.path.is_some() || (dep.dev && !workspace_member) {
            continue;
        }
        let mut found = false;
        for id in &old.dependencies {
            if model.matches_dependency(dep, id)? {
                selected.insert(id.clone());
                found = true;
            }
        }
        if !found && (workspace_member || !dep.optional) {
            return Ok(None);
        }
    }
    if old
        .dependencies
        .iter()
        .any(|id| id.source.is_some() && !selected.contains(id))
    {
        return Ok(None);
    }
    Ok(Some(selected))
}

fn local_record(
    model: &CargoWorkspaceModel,
    member: &CargoMember,
    workspace_member: bool,
    graph: &Lock,
) -> Result<Option<Record>> {
    let mut selected = if let Some(previous) = graph.packages.get(&member.id) {
        let Some(selected) = selected_external(model, member, workspace_member, previous)? else {
            return Ok(None);
        };
        selected
    } else if !member
        .dependencies
        .iter()
        .any(|dep| dep.path.is_none() && (workspace_member || (!dep.dev && !dep.optional)))
    {
        BTreeSet::new()
    } else {
        return Ok(None);
    };
    for dep in &member.dependencies {
        if dep.dev && !workspace_member {
            continue;
        }
        if let Some(path) = &dep.path {
            if !workspace_member && dep.optional && !model.local_packages.contains_key(path) {
                continue;
            }
            let target = model
                .local_packages
                .get(path)
                .with_context(|| format!("Missing local Cargo package {path}/Cargo.toml"))?;
            ensure!(
                model.matches_dependency(dep, &target.id)?,
                "Local package at {path}/Cargo.toml does not satisfy dependency {} of {}/Cargo.toml",
                dep.name,
                member.rel_dir
            );
            // Cargo locks all features of members. An excluded path crate
            // only contributes the optional edges selected by its dependants.
            if workspace_member
                || !dep.optional
                || graph
                    .packages
                    .get(&member.id)
                    .is_some_and(|record| record.dependencies.contains(&target.id))
            {
                selected.insert(target.id.clone());
            }
        }
    }
    if !model.matches_direct(member, workspace_member, &selected)? {
        return Ok(None);
    }
    Ok(Some(Record {
        dependencies: selected,
        ..Record::default()
    }))
}

fn local_edit(model: &CargoWorkspaceModel, graph: &Lock) -> Result<Option<Lock>> {
    let members: BTreeSet<_> = model
        .components
        .values()
        .flatten()
        .map(|member| &member.id)
        .collect();
    let mut output = Lock {
        metadata: graph.metadata.clone(),
        unused: graph.unused.clone(),
        ..Lock::default()
    };
    let mut queue: VecDeque<_> = members.iter().map(|id| (*id).clone()).collect();
    while let Some(id) = queue.pop_front() {
        if output.packages.contains_key(&id) {
            continue;
        }
        let record = if id.source.is_some() {
            graph
                .packages
                .get(&id)
                .context("Missing Cargo.lock dependency record")?
                .clone()
        } else {
            let member = model.local_package(&id)?;
            let Some(record) = local_record(model, member, members.contains(&id), graph)? else {
                return Ok(None);
            };
            record
        };
        queue.extend(
            record
                .dependencies
                .iter()
                .chain(&record.replacement)
                .cloned(),
        );
        output.packages.insert(id, record);
    }
    let Some(usage) = model.patch_usage(&output)? else {
        return Ok(None);
    };
    output.unused = usage.unused;
    Ok(Some(output))
}

fn reachable_from<'a>(
    lock: &'a Lock,
    starts: impl IntoIterator<Item = &'a CargoPackageId>,
) -> Result<BTreeSet<CargoPackageId>> {
    let mut reached = BTreeSet::new();
    let mut queue: VecDeque<_> = starts.into_iter().collect();
    while let Some(id) = queue.pop_front() {
        if !reached.insert(id.clone()) {
            continue;
        }
        let record = lock.packages.get(id).with_context(|| {
            format!(
                "Missing selected package {} {} in Cargo.lock",
                id.name, id.version
            )
        })?;
        queue.extend(record.dependencies.iter().chain(&record.replacement));
    }
    Ok(reached)
}

/// New or reselected declarations (including feature edits) may require Cargo
/// to change a shared transitive version. Other packages keep their pinned
/// versions, even when a removed feature drops some dependencies of a package
/// that remains selected.
fn check_sync_selections(old: &Lock, new: &Lock, model: &CargoWorkspaceModel) -> Result<()> {
    let mut changed_roots = BTreeSet::new();
    for member in model.local_packages.values() {
        let Some(after) = new.packages.get(&member.id) else {
            continue;
        };
        if model.local_package(&member.id)?.rel_dir != member.rel_dir {
            continue;
        }
        let before = old.packages.get(&member.id);
        for dep in &member.dependencies {
            let mut previous_count = 0;
            if let Some(before) = before {
                for id in &before.dependencies {
                    if model.matches_dependency(dep, id)? {
                        previous_count += 1;
                    }
                }
            }
            if previous_count == 1 {
                continue;
            }
            for id in &after.dependencies {
                if model.matches_dependency(dep, id)? {
                    changed_roots.insert(id);
                }
            }
        }
    }
    // Any package whose active feature set gained a brand-new edge (not merely
    // replacing a dropped version of the same (name, source)) may legitimately
    // pull in an updated shared dependency.
    for (id, before) in &old.packages {
        let Some(after) = new.packages.get(id) else {
            continue;
        };
        let dropped_keys: BTreeSet<_> = before
            .dependencies
            .difference(&after.dependencies)
            .map(|dep| {
                (
                    dep.name.as_str(),
                    dep.source.as_deref().map(super::reference_source),
                )
            })
            .collect();
        for next in after.dependencies.difference(&before.dependencies) {
            let key = (
                next.name.as_str(),
                next.source.as_deref().map(super::reference_source),
            );
            if !dropped_keys.contains(&key) {
                changed_roots.insert(next);
            }
        }
    }
    let allowed = reachable_from(new, changed_roots)?;
    for (id, before) in &old.packages {
        let Some(after) = new.packages.get(id) else {
            continue;
        };
        ensure!(
            before.checksum == after.checksum,
            "Cargo resolution changed the checksum of {} {}",
            id.name,
            id.version
        );
        for next in after.dependencies.difference(&before.dependencies) {
            if allowed.contains(next) {
                continue;
            }
            for previous in before.dependencies.difference(&after.dependencies) {
                let same_source = previous.source.as_deref().map(super::reference_source)
                    == next.source.as_deref().map(super::reference_source);
                ensure!(
                    previous.name != next.name || !same_source,
                    "Cargo resolution changed the unrelated selection of {} {} for {} {}",
                    previous.name,
                    previous.version,
                    id.name,
                    id.version
                );
            }
        }
    }
    Ok(())
}

/// Feature unification can add and remove edges. A package which is still
/// selected in an incoming activation class keeps its staged pin and checksum,
/// unless it is a transitive companion dependency of a crate whose version was
/// unified to the live workspace's newer release (e.g. serde 1.0.230 -> serde_derive =1.0.230).
#[cfg(test)]
pub(super) fn check_seed_pins_for_test(
    seed: &Lock,
    resolved: &Lock,
    redirected_winners: &BTreeSet<CargoPackageId>,
) -> Result<()> {
    check_seed_pins(seed, resolved, redirected_winners)
}

fn check_seed_pins(
    seed: &Lock,
    resolved: &Lock,
    redirected_winners: &BTreeSet<CargoPackageId>,
) -> Result<()> {
    let redirected_transitive = if redirected_winners.is_empty() {
        BTreeSet::new()
    } else {
        let starts: Vec<_> = redirected_winners
            .iter()
            .filter_map(|winner| resolved.packages.get(winner))
            .flat_map(|record| record.dependencies.iter().chain(&record.replacement))
            .collect();
        reachable_from(resolved, starts)?
    };
    let mut pins = BTreeMap::new();
    for (id, record) in &seed.packages {
        if let Some(key) = activation(id)? {
            pins.insert(key, (id, &record.checksum));
        }
    }
    for (id, record) in &resolved.packages {
        if let Some(key) = activation(id)?
            && let Some((expected, checksum)) = pins.get(&key)
        {
            if *expected == id {
                ensure!(
                    *checksum == &record.checksum,
                    "Cargo resolution changed the staged pin or checksum for {} {}; align the component lockfiles explicitly",
                    expected.name,
                    expected.version
                );
            } else if !redirected_transitive.contains(id) {
                anyhow::bail!(
                    "Cargo resolution changed the staged pin or checksum for {} {}; align the component lockfiles explicitly",
                    expected.name,
                    expected.version
                );
            }
        }
    }
    Ok(())
}

fn check_preserved(
    old: &Lock,
    new: &Lock,
    model: &CargoWorkspaceModel,
    added: &BTreeSet<String>,
    state: &ProjectState,
) -> Result<()> {
    if added.is_empty() {
        return check_sync_selections(old, new, model);
    }
    // A registration may add features to a shared package, but it cannot
    // discard an existing member, edge, selected version, or checksum.
    for (id, previous) in &old.packages {
        let preserved = new.packages.get(id).is_some_and(|current| {
            previous.checksum == current.checksum
                && previous.replacement == current.replacement
                && previous.dependencies.is_subset(&current.dependencies)
        });
        if !preserved {
            let advice = match added
                .iter()
                .find(|path| state.components[*path].origin.template == "adopted")
            {
                Some(path) => format!(
                    "set aside dependency edits that require {path} and restore the accepted dependencies. \
                     To sync independent edits first, temporarily move {path} outside apps/ and packages/, \
                     save and remove its workspace.members and workspace.default-members entries from root Cargo.toml, \
                     run make sync, then restore the directory and the saved entries. \
                     Retry registration with all intended entries, then reapply the deferred edits and run make sync"
                ),
                None => "run make sync before adding the component".to_owned(),
            };
            bail!(
                "Adding a component would change the existing Cargo selection of {} {}; {advice}",
                id.name,
                id.version
            );
        }
    }
    Ok(())
}

/// A new lock may activate a previously dormant path. Rebuild the manifest
/// closure from the new graph before issuing the resolution receipt.
pub(crate) fn resolved_model(
    root: &Path,
    state: &ProjectState,
    lock: &Lock,
    release: &AdmittedRelease,
    resolver: &dyn CargoLockResolver,
    offline: bool,
) -> Result<CargoWorkspaceModel> {
    let model = CargoWorkspaceModel::for_resolution(root, state, lock, None)?;
    if !model.needs_path_witnesses() {
        return Ok(model);
    }
    let selected = resolver.selected_local_paths(root, release, offline)?;
    CargoWorkspaceModel::for_resolution(root, state, lock, Some(&selected))
}

pub(crate) fn reconcile(
    root: &Path,
    state: &ProjectState,
    release: &AdmittedRelease,
    seeds: &[(String, Vec<u8>)],
    resolver: &dyn CargoLockResolver,
    offline: bool,
    added: &BTreeSet<String>,
) -> Result<CargoResolution> {
    let path = safe_path(root, "Cargo.lock")?;
    let original = read_regular_file(&path, 32 * 1024 * 1024, "Cargo.lock")?;
    let previous_bytes = original.as_ref().map(|(bytes, _)| bytes.as_slice());
    let proof = state.cargo_resolution.as_ref();
    let original_lock = previous_bytes
        .map(Lock::parse)
        .transpose()
        .context("Invalid root Cargo.lock")?
        .unwrap_or_default();
    // A pending edit can disable a formerly selected, now absent or malformed
    // optional manifest. Use a lock-independent preflight in that case and let
    // Cargo supply the exact new selection before validating the staged lock.
    let model = CargoWorkspaceModel::for_resolution(
        root,
        state,
        &original_lock,
        proof.and_then(|p| p.inputs.local_selections.as_ref()),
    )
    .or_else(|error| {
        if original_lock.packages.is_empty() {
            Err(error)
        } else {
            CargoWorkspaceModel::for_resolution(root, state, &Lock::default(), None)
        }
    })?;
    if added.is_empty()
        && !model.needs_path_witnesses()
        && let (Some(bytes), Some(proof)) = (previous_bytes, proof)
        && proof.matches(&model.resolution_inputs, bytes)
    {
        model.validate_lock(state, &original_lock)?;
        return Ok(proof.clone());
    }

    let mut locks = parse_inputs(seeds)?;
    if original.is_some() {
        locks.insert(0, ("Cargo.lock".into(), original_lock.clone()));
    }
    let merged = merge(&locks)?;
    if let Some(MergeConflict::Versions(error)) = &merged.conflict {
        ensure!(
            !offline,
            "{error}; align the component Cargo.lock versions or rerun this operation with network access"
        );
    }
    let mut graph = merged.seed;
    let redirected_winners = merged.redirected_winners;
    let mode = original.as_ref().map_or(0o644, |(_, id)| id.mode);
    if merged.conflict.is_none() {
        let unchanged_inputs = previous_bytes.zip(proof).is_some_and(|(bytes, proof)| {
            proof.lock_matches(bytes)
                && proof.constructive_inputs_match(&model.resolution_inputs, added)
        });
        let initial_workspace = previous_bytes.is_none()
            && proof.is_none()
            && model
                .components
                .keys()
                .all(|component| added.contains(component));
        let baseline = unchanged_inputs || initial_workspace;
        let constructive = model.local_workspace_policy
            && ((model.simple_local_graph
                && model.patches.iter().all(|p| p.target.path.is_some())
                && model.replacements.is_empty())
                || (baseline
                    && super::recipes::extend(
                        &model,
                        release,
                        &original_lock,
                        added,
                        &mut graph,
                    )?));
        if constructive
            && !model.needs_path_witnesses()
            && let Some(edited) = local_edit(&model, &graph)?
        {
            model.validate_lock(state, &edited)?;
            check_preserved(&original_lock, &edited, &model, added, state)?;
            if !seeds.is_empty() {
                check_seed_pins(&graph, &edited, &redirected_winners)?;
            }
            if edited == original_lock
                && let Some(bytes) = previous_bytes
            {
                return Ok(model.resolution_inputs.proof(bytes));
            }
            let bytes = edited.bytes()?;
            write_atomic(&path, &bytes, mode)?;
            return Ok(model.resolution_inputs.proof(&bytes));
        }
    }
    // If an existing Cargo.lock already satisfies the staged workspace offline
    // (e.g. after `cargo add` in exec --write or a cross-version proof refresh),
    // verify it read-only before invoking `cargo update --workspace`.
    if added.is_empty()
        && seeds.is_empty()
        && let Some(bytes) = previous_bytes
        && resolver.validate_locked(root, release, true).is_ok()
    {
        let (after_bytes, _) = read_regular_file(&path, 32 * 1024 * 1024, "Cargo.lock")?
            .context("Cargo.lock disappeared during locked validation")?;
        if after_bytes == bytes {
            let model = resolved_model(root, state, &original_lock, release, resolver, offline)?;
            model.validate_lock(state, &original_lock)?;
            return Ok(model.resolution_inputs.proof(&after_bytes));
        }
    }
    // The seed is a set of preferences for Cargo. Feature edges or class
    // choices that disagreed across inputs still require a real resolution.
    if !graph.packages.is_empty() && (original.is_none() || graph != original_lock) {
        write_atomic(&path, &graph.bytes()?, mode)?;
    }
    let staged_before = staging_signature_without_lock(root, state)?;
    resolver
        .resolve_cargo(root, release, offline)
        .context("Could not reconcile Cargo.lock. Check constraints and retry with a populated Cargo cache or network access")?;
    ensure!(
        staging_signature_without_lock(root, state)? == staged_before,
        "Cargo resolver unexpectedly modified files outside Cargo.lock"
    );
    let (bytes, _) = read_regular_file(&path, 32 * 1024 * 1024, "Cargo.lock")?
        .context("Cargo resolver did not produce Cargo.lock")?;
    let resolved = Lock::parse(&bytes)?;
    let model = resolved_model(root, state, &resolved, release, resolver, offline)?;
    model
        .validate_lock(state, &resolved)
        .context("Cargo resolver produced a lock inconsistent with the staged manifests")?;
    check_preserved(&original_lock, &resolved, &model, added, state)?;
    if !seeds.is_empty() {
        check_seed_pins(&graph, &resolved, &redirected_winners)?;
    }
    Ok(model.resolution_inputs.proof(&bytes))
}

fn staging_signature_without_lock(root: &Path, state: &ProjectState) -> Result<String> {
    use crate::files::{digest, identity, relative_name, sorted_entries};
    use std::fs;
    fn visit(
        root: &Path,
        dir: &Path,
        state: &ProjectState,
        entries: &mut Vec<String>,
    ) -> Result<()> {
        for e in sorted_entries(dir)? {
            let p = e.path();
            let n = relative_name(root, &p)?;
            if n == "Cargo.lock" || crate::state::snapshot_excluded_at(root, state, &n) {
                continue;
            }
            let m = fs::symlink_metadata(&p)?;
            if m.file_type().is_symlink() {
                entries.push(format!("L:{n}:{}", fs::read_link(&p)?.to_string_lossy()));
            } else if m.is_dir() {
                entries.push(format!("D:{n}"));
                visit(root, &p, state, entries)?;
            } else if m.is_file() {
                let id = identity(&p)?.context("Staged file disappeared")?;
                entries.push(format!("F:{n}:{}:{:o}", id.sha256, id.mode));
            } else {
                anyhow::bail!("Unsupported staged entry created during Cargo resolution: {n}");
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(root, root, state, &mut entries)?;
    Ok(digest(entries.join("\n")))
}
