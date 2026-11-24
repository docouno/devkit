//! The virtual Cargo workspace in a consumer checkout. Cargo resolves versions;
//! Devkit binds the selected graph to registered Make targets and source paths.
use super::*;
use crate::cargo_lock::Lock;
use semver::Version;
mod dependencies;
mod proof;
use dependencies::{CRATES_IO, CargoReplacement, PatchTables};
pub use dependencies::{CargoDeclaration, CargoPatch};
pub(crate) use dependencies::{CargoReplacementSpec, cargo_patch_origin};
pub(crate) use proof::cargo_template_recipe;

impl ReadContext<'_> {
    pub(super) fn dependency_tables(
        &mut self,
        dir: &str,
        table: &Fields,
    ) -> Result<[&'static str; 3]> {
        let preferred = |modern, old| {
            if table.contains_key(modern) {
                modern
            } else {
                old
            }
        };
        if let Some(key) = ["dev_dependencies", "build_dependencies"]
            .into_iter()
            .find(|k| table.contains_key(*k))
        {
            self.ensure_cargo_manifest(dir)?;
            let edition = self.cargo[dir]
                .get("package")
                .and_then(|p| p.get("edition"))
                .cloned();
            let modern = if edition
                .as_ref()
                .is_some_and(|v| v.get("workspace") == Some(&Value::Bool(true)))
            {
                let ws_dir = self.find_cargo_workspace_dir(dir)?;
                self.cargo[&ws_dir]
                    .get("workspace")
                    .and_then(|w| w.get("package"))
                    .and_then(|p| p.get("edition"))
                    .and_then(Value::as_str)
                    == Some("2024")
            } else {
                edition.as_ref().and_then(Value::as_str) == Some("2024")
            };
            ensure!(
                !modern,
                "{key} is unsupported in the 2024 edition at {dir}/Cargo.toml; use {}",
                key.replace('_', "-")
            );
        }
        Ok([
            "dependencies",
            preferred("dev-dependencies", "dev_dependencies"),
            preferred("build-dependencies", "build_dependencies"),
        ])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CargoWorkspaceKind {
    Component,
    Root,
}

impl CargoWorkspaceKind {
    pub fn for_version(version: &str) -> Result<Self> {
        Ok(if SemVer::parse(version)? >= SemVer::parse("0.6.0")? {
            Self::Root
        } else {
            Self::Component
        })
    }
    pub fn for_state(state: &ProjectState) -> Result<Self> {
        Self::for_version(&state.version)
    }
    pub(crate) fn root_with_rust(state: &ProjectState) -> Result<bool> {
        Ok(Self::for_state(state)? == Self::Root
            && state
                .components
                .values()
                .any(|c| c.toolchain == Toolchain::Rust))
    }
}

pub fn rust_component_owner<'a>(state: &'a ProjectState, path: &str) -> Option<&'a str> {
    component_owner(state, path)
        .filter(|owner| state.components[*owner].toolchain == Toolchain::Rust)
}

#[derive(Debug, Clone)]
pub struct CargoMember {
    pub rel_dir: String,
    pub id: CargoPackageId,
    pub dependencies: Vec<CargoDeclaration>,
}
impl CargoMember {
    pub fn fmt_package(&self) -> &str {
        &self.id.name
    }
    pub fn pkgid(&self) -> Result<String> {
        let path = Path::new("/workspace").join(&self.rel_dir);
        let mut url = url::Url::from_file_path(path)
            .map_err(|_| anyhow::anyhow!("Invalid Cargo package directory: {}", self.rel_dir))?;
        url.set_fragment(Some(&format!("{}@{}", self.id.name, self.id.version)));
        Ok(format!("path+{url}"))
    }
}

#[derive(Debug, Clone, Default)]
pub struct CargoWorkspaceModel {
    pub(crate) resolution_inputs: crate::model::CargoResolutionInputs,
    pub(crate) recipe_workspace: bool,
    pub(crate) simple_local_graph: bool,
    pub(crate) local_workspace_policy: bool,
    pub(super) default_members: BTreeSet<String>,
    pub(crate) inert_members: BTreeSet<String>,
    pub(crate) member_recipes: BTreeMap<String, String>,
    pub components: BTreeMap<String, Vec<CargoMember>>,
    /// All manifest candidates, including excluded local dependencies and unused patches.
    pub local_packages: BTreeMap<String, CargoMember>,
    pub patches: Vec<CargoPatch>,
    pub(crate) replacements: Vec<CargoReplacement>,
    shadow_manifests: BTreeSet<String>,
    conditional_paths: BTreeSet<String>,
    selected_witnesses: Option<BTreeSet<String>>,
    local_paths: BTreeMap<CargoPackageId, Option<String>>,
}
impl CargoWorkspaceModel {
    pub fn read(root: &Path, state: &ProjectState) -> Result<Self> {
        ensure!(
            CargoWorkspaceKind::for_state(state)? == CargoWorkspaceKind::Root,
            "Expected a root Cargo workspace"
        );
        ReadContext::new(root, NodeLockKind::for_state(state)?).configured_model(
            state,
            true,
            &mut BTreeMap::new(),
            Selection::default(),
        )
    }
    pub(crate) fn for_resolution(
        root: &Path,
        state: &ProjectState,
        lock: &Lock,
        witnesses: Option<&BTreeSet<String>>,
    ) -> Result<Self> {
        let mut context = ReadContext::new(root, NodeLockKind::for_state(state)?);
        let mut model = context.configured_model(
            state,
            true,
            &mut BTreeMap::new(),
            Selection {
                lock: Some(lock),
                witnesses,
            },
        )?;
        context.bind_resolution_inputs(state, &mut model)?;
        Ok(model)
    }
    pub(crate) fn needs_path_witnesses(&self) -> bool {
        !self.conditional_paths.is_empty() && self.selected_witnesses.is_none()
    }
    fn bind_local_paths(
        &mut self,
        direct: &BTreeSet<String>,
        mut candidates: BTreeMap<CargoPackageId, BTreeSet<String>>,
        selection: Selection<'_>,
    ) -> Result<()> {
        // A direct path or a workspace member has a known physical location.
        // Patches can share its lock ID while referring to an unused alternative.
        let mut explicit = BTreeMap::<_, BTreeSet<_>>::new();
        for path in direct {
            if let Some(member) = self.local_packages.get(path) {
                explicit
                    .entry(member.id.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
        candidates.extend(explicit);
        for member in self.components.values().flatten() {
            candidates.insert(member.id.clone(), BTreeSet::from([member.rel_dir.clone()]));
        }
        for (id, paths) in candidates {
            let path = if paths.len() == 1 {
                paths.first()
            } else if selection
                .lock
                .is_some_and(|lock| lock.packages.contains_key(&id))
            {
                self.conditional_paths.extend(paths.iter().cloned());
                let mut selected = paths
                    .iter()
                    .filter(|p| selection.witnesses.is_some_and(|w| w.contains(*p)));
                let first = selected.next();
                ensure!(
                    selected.next().is_none(),
                    "Multiple selected directories for Cargo package {} {}",
                    id.name,
                    id.version
                );
                first
            } else {
                None
            };
            self.local_paths.insert(id, path.cloned());
        }
        self.selected_witnesses = selection
            .witnesses
            .map(|w| w.intersection(&self.conditional_paths).cloned().collect());
        Ok(())
    }
    pub(crate) fn validate_lock(
        &self,
        state: &ProjectState,
        lock: &crate::cargo_lock::Lock,
    ) -> Result<()> {
        self.selected_edges(state, lock).map(drop)
    }
    pub(crate) fn local_package(&self, id: &CargoPackageId) -> Result<&CargoMember> {
        let path = self
            .local_paths
            .get(id)
            .with_context(|| {
                format!(
                    "Cargo.lock selects {} {} without a local manifest; run make sync",
                    id.name, id.version
                )
            })?
            .as_ref()
            .with_context(|| {
                format!(
                    "Ambiguous local Cargo target {} {} at multiple admitted paths",
                    id.name, id.version
                )
            })?;
        Ok(&self.local_packages[path])
    }
    pub fn members(&self, component: &str) -> Result<&[CargoMember]> {
        self.components
            .get(component)
            .map(Vec::as_slice)
            .with_context(|| {
                format!("No Cargo workspace members in registered Rust component {component}")
            })
    }
}

pub(crate) fn cargo_registries(
    root: &Path,
    state: &ProjectState,
) -> Result<BTreeMap<String, String>> {
    let mut context = ReadContext::new(root, NodeLockKind::for_state(state)?);
    context.root_config_inputs(&mut BTreeMap::new())?;
    context.registry_indexes()
}

pub(crate) fn cargo_proof_matches(root: &Path, state: &ProjectState) -> Result<bool> {
    let Some(proof) = &state.cargo_resolution else {
        return Ok(false);
    };
    let lock_path = input_path(root, "Cargo.lock")?;
    let Some((lock_bytes, _)) =
        read_regular_file(&lock_path, NATIVE_METADATA_MAX_BYTES, "root Cargo.lock")?
    else {
        return Ok(false);
    };
    if !proof.lock_matches(&lock_bytes) {
        return Ok(false);
    }
    let lock = Lock::parse(&lock_bytes)?;
    let mut context = ReadContext::new(root, NodeLockKind::for_state(state)?);
    let mut files = BTreeMap::new();
    let mut model = context.root_inputs(
        state,
        &mut files,
        true,
        Selection {
            lock: Some(&lock),
            witnesses: proof.inputs.local_selections.as_ref(),
        },
    )?;
    context.bind_resolution_inputs(state, &mut model)?;
    Ok(proof.matches(&model.resolution_inputs, &lock_bytes))
}

/// Cargo.lock identifies a local package by name and version, and gives each
/// parent its selected direct IDs. Only an ambiguity within that evidence
/// needs Cargo's manifest paths.
#[derive(Clone, Copy, Default)]
struct Selection<'a> {
    lock: Option<&'a Lock>,
    witnesses: Option<&'a BTreeSet<String>>,
}

impl Selection<'_> {
    fn override_active(
        &self,
        path: &str,
        id: &CargoPackageId,
        overrides: &BTreeMap<CargoPackageId, BTreeSet<String>>,
    ) -> bool {
        let Some(lock) = self.lock else {
            return false;
        };
        let Some(paths) = overrides.get(id) else {
            return false;
        };
        if !paths.contains(path) || !lock.packages.contains_key(id) {
            return false;
        }
        if paths.len() == 1 {
            return !lock.unused_patch(id);
        }
        // An unused patch has no source directory in Cargo.lock. Two origins
        // can select and retire different directories with the same package ID.
        self.witnesses
            .is_some_and(|selected| selected.contains(path))
    }

    fn path_active(
        &self,
        member: bool,
        id: &CargoPackageId,
        dep: &CargoDeclaration,
        declarations: &[CargoDeclaration],
        model: &mut CargoWorkspaceModel,
    ) -> Result<bool> {
        if member {
            return Ok(true);
        }
        if dep.dev {
            return Ok(false);
        }
        if !dep.optional {
            return Ok(true);
        }
        let Some(record) = self.lock.and_then(|l| l.packages.get(id)) else {
            return Ok(false);
        };
        let path = dep.path.as_ref().expect("path declaration");
        for child in record
            .dependencies
            .iter()
            .filter(|child| child.source.is_none())
        {
            if !dep.accepts_version(child)? {
                continue;
            }
            let mut competing_source = false;
            for other in declarations.iter().filter(|d| !d.dev && d.path != dep.path) {
                competing_source |= other.accepts_version(child)?;
            }
            if !competing_source {
                return Ok(true);
            }
            model.conditional_paths.insert(path.clone());
            return Ok(self.witnesses.is_some_and(|w| w.contains(path)));
        }
        Ok(false)
    }
}

impl ReadContext<'_> {
    pub(super) fn workspace_paths(&mut self, key: &str) -> Result<BTreeSet<String>> {
        let manifest = self.cargo_manifest("")?;
        ensure!(
            !manifest.contains_key("package"),
            "Root Cargo.toml must be a virtual workspace without [package]"
        );
        let workspace = object(
            manifest
                .get("workspace")
                .context("Root Cargo.toml must define [workspace]")?,
            "Cargo workspace",
        )?;
        self.workspace_directory_list("", workspace, key)
    }

    fn workspace_directory_list(
        &self,
        base: &str,
        workspace: &Fields,
        key: &str,
    ) -> Result<BTreeSet<String>> {
        let mut paths = BTreeSet::new();
        if let Some(entries) = workspace.get(key) {
            for entry in entries
                .as_array()
                .with_context(|| format!("workspace.{key} must be an array"))?
            {
                let value = text(entry, "Cargo workspace path")?;
                ensure!(
                    !value.contains(['?', '*', '[', ']']),
                    "Use explicit paths in workspace.{key} instead of the glob {value}"
                );
                paths.insert(cargo_local_path(self.root, base, value)?);
            }
        }
        Ok(paths)
    }

    pub(super) fn legacy_members(&mut self, component: &str) -> Result<BTreeSet<String>> {
        let manifest = self.cargo_manifest(component)?;
        let Some(workspace) = manifest.get("workspace") else {
            return Ok(BTreeSet::from([component.to_string()]));
        };
        let workspace = object(workspace, "Cargo workspace")?;
        let exclude = self.workspace_directory_list(component, workspace, "exclude")?;
        let mut queue: VecDeque<_> = self
            .workspace_directory_list(component, workspace, "members")?
            .into_iter()
            .collect();
        if manifest.contains_key("package") {
            queue.push_back(component.to_string());
        }
        let mut members = BTreeSet::new();
        while let Some(path) = queue.pop_front() {
            if !members.insert(path.clone()) {
                continue;
            }
            let manifest = self.cargo_manifest(&path)?;
            ensure!(
                manifest.contains_key("package"),
                "Missing [package] in {path}/Cargo.toml"
            );
            let mut inputs = CargoInputs::default();
            self.collect_cargo_tables(&path, &manifest, &mut inputs, true)?;
            for table in dependency_table(&manifest, "target")?.values() {
                self.collect_cargo_tables(
                    &path,
                    object(table, "Cargo target")?,
                    &mut inputs,
                    true,
                )?;
            }
            for target in inputs.dependencies {
                if Path::new(&target).starts_with(component)
                    && !exclude.iter().any(|e| Path::new(&target).starts_with(e))
                {
                    queue.push_back(target);
                }
            }
        }
        Ok(members)
    }

    fn declarations(
        &mut self,
        path: &str,
        manifest: &Fields,
        registries: &BTreeMap<String, String>,
    ) -> Result<Vec<CargoDeclaration>> {
        let targets = dependency_table(manifest, "target")?;
        let mut tables = vec![manifest];
        for target in targets.values() {
            tables.push(object(target, "Cargo target")?);
        }
        let mut decls = Vec::new();
        for table in tables {
            for field in self.dependency_tables(path, table)? {
                for (alias, value) in dependency_table(table, field)? {
                    let mut decl = if value.get("workspace") == Some(&Value::Bool(true)) {
                        let base = self.find_cargo_workspace_dir(path)?;
                        let inherited = self.cargo[&base]
                            .get("workspace")
                            .and_then(Value::as_object)
                            .and_then(|w| w.get("dependencies"))
                            .and_then(Value::as_object)
                            .and_then(|deps| deps.get(&alias))
                            .with_context(|| {
                                format!("Missing inherited Cargo dependency {alias} in {path}/Cargo.toml")
                            })?;
                        self.declaration(&base, &alias, inherited, registries)?
                    } else {
                        self.declaration(path, &alias, &value, registries)?
                    };
                    decl.optional = value
                        .get("optional")
                        .and_then(Value::as_bool)
                        .unwrap_or(decl.optional);
                    decl.dev = field.starts_with("dev");
                    decls.push(decl);
                }
            }
        }
        Ok(decls)
    }

    fn root_model(
        &mut self,
        state: &ProjectState,
        strict: bool,
        extra_paths: &BTreeSet<String>,
        selection: Selection<'_>,
    ) -> Result<CargoWorkspaceModel> {
        let members = self.workspace_paths("members")?;
        let excluded = self.workspace_paths("exclude")?;
        self.root_members = Some(members.clone());
        for exclude in &excluded {
            let exclude = Path::new(exclude);
            for path in members.iter().chain(state.components.keys()) {
                ensure!(
                    !Path::new(path).starts_with(exclude) && !exclude.starts_with(path),
                    "workspace.exclude {} overlaps {path}",
                    exclude.display()
                );
            }
        }
        let root = self.cargo_inputs("")?;
        let mut queue: VecDeque<_> = members
            .iter()
            .chain(root.dependencies.iter())
            .chain(root.overrides.iter())
            .chain(extra_paths)
            .cloned()
            .collect();
        let registries = self.registry_indexes()?;
        let root_manifest = self.cargo_manifest("")?;
        let mut patches = BTreeMap::new();
        let mut manifest_patches = PatchTables::default();
        manifest_patches.merge("", &root_manifest)?;
        manifest_patches.apply(self, &registries, &mut patches)?;

        let mut config_patches = PatchTables::default();
        for config in &self.cargo_config_order {
            config_patches.merge(
                &parent_name(&parent_name(config)),
                &self.cargo_configs[config],
            )?;
        }
        config_patches.apply(self, &registries, &mut patches)?;
        let replacements = dependency_table(&root_manifest, "replace")?
            .iter()
            .map(|(key, value)| CargoReplacement::new(key, value, self, &registries))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            patches.is_empty() || replacements.is_empty(),
            "Cargo cannot combine [patch] and [replace] in one workspace"
        );
        let mut model = CargoWorkspaceModel {
            patches: patches.into_values().collect(),
            replacements,
            ..Default::default()
        };
        let mut direct_paths = members.clone();
        if !strict {
            // Snapshot a missing workspace entry's direct manifest so sync can
            // safely restore membership and register can adopt new components.
            for (path, c) in &state.components {
                if c.toolchain == Toolchain::Rust
                    && exists(&input_path(self.root, prefixed(path, "Cargo.toml"))?)?
                {
                    self.root_members
                        .as_mut()
                        .expect("root membership initialized")
                        .insert(path.clone());
                    direct_paths.insert(path.clone());
                    queue.push_back(path.clone());
                }
            }
        }
        let override_paths: BTreeSet<String> = model
            .patches
            .iter()
            .filter_map(|p| p.target.path.clone())
            .chain(
                model
                    .replacements
                    .iter()
                    .filter_map(|r| r.target.path.clone()),
            )
            .collect();
        let mut overrides = BTreeMap::<_, BTreeSet<_>>::new();
        for path in override_paths {
            let id = self
                .cargo_package_id(&path)
                .with_context(|| format!("Cannot read Cargo override at {path}/Cargo.toml"))?
                .with_context(|| {
                    format!("Missing package in Cargo override at {path}/Cargo.toml")
                })?;
            overrides.entry(id).or_default().insert(path);
        }
        let mut names = BTreeMap::new();
        let mut seen = BTreeSet::new();
        while let Some(path) = queue.pop_front() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let owner = component_owner(state, &path);
            ensure!(
                owner.is_none_or(|p| state.components[p].toolchain == Toolchain::Rust),
                "Cargo manifest inside a registered Node component: {path}/Cargo.toml"
            );
            let member = members.contains(&path);
            if !strict && owner.is_some() {
                self.root_members
                    .as_mut()
                    .expect("workspace membership initialized")
                    .insert(path.clone());
            }
            if member && strict {
                ensure!(
                    owner.is_some(),
                    "Cargo workspace member has no registered Rust component: {path}/Cargo.toml"
                );
            }
            let manifest_path = prefixed(&path, "Cargo.toml");
            if !exists(&input_path(self.root, &manifest_path)?)? && !member {
                model.shadow_manifests.insert(manifest_path);
                continue;
            }
            let manifest = self.cargo_manifest(&path)?;
            if member {
                ensure!(
                    !manifest.contains_key("workspace"),
                    "Nested Cargo workspace at {path}/Cargo.toml"
                );
            }
            let package = dependency_table(&manifest, "package")?;
            if package.contains_key("workspace") {
                let authority = self.find_cargo_workspace_dir(&path)?;
                ensure!(
                    !member || authority.is_empty(),
                    "package.workspace in {path}/Cargo.toml must point to the root Cargo.toml"
                );
            } else if !member
                && (package
                    .values()
                    .any(|v| v.get("workspace") == Some(&Value::Bool(true)))
                    || manifest.get("lints").and_then(|v| v.get("workspace"))
                        == Some(&Value::Bool(true)))
            {
                self.find_cargo_workspace_dir(&path)?;
            }
            let id = self
                .cargo_package_id(&path)?
                .with_context(|| format!("Missing [package] in {path}/Cargo.toml"))?;
            Version::parse(&id.version)
                .with_context(|| format!("Invalid package version in {path}/Cargo.toml"))?;
            let dependencies = self.declarations(&path, &manifest, &registries)?;
            let selected =
                direct_paths.contains(&path) || selection.override_active(&path, &id, &overrides);
            for dep in &dependencies {
                if let Some(target) = &dep.path {
                    if member && strict && !members.contains(target) {
                        ensure!(
                            excluded.iter().any(|e| Path::new(target).starts_with(e)),
                            "Implicit Cargo member {target} referenced by {path}/Cargo.toml; list it explicitly in workspace.members or add the external helper to workspace.exclude"
                        );
                    }
                    if selected
                        && selection.path_active(member, &id, dep, &dependencies, &mut model)?
                    {
                        if direct_paths.insert(target.clone()) {
                            // A candidate may already have been inspected before a
                            // selected dependency brought its children into scope.
                            seen.remove(target);
                        }
                        queue.push_back(target.clone());
                    } else {
                        model
                            .shadow_manifests
                            .insert(prefixed(target, "Cargo.toml"));
                    }
                }
            }
            // Member overrides have no effect in a Cargo workspace. Rejecting
            // them also avoids recording an inactive path as a selected patch.
            if member {
                ensure!(
                    !manifest.contains_key("patch") && !manifest.contains_key("replace"),
                    "Move Cargo overrides from {path}/Cargo.toml to the root Cargo.toml"
                );
            }
            let entry = CargoMember {
                rel_dir: path.clone(),
                id,
                dependencies,
            };
            if member && let Some(owner) = owner {
                if let Some(previous) = names.insert(entry.id.name.clone(), path.clone()) {
                    bail!(
                        "Duplicate workspace package {} in {previous}/Cargo.toml and {path}/Cargo.toml",
                        entry.id.name
                    );
                }
                model
                    .components
                    .entry(owner.to_owned())
                    .or_default()
                    .push(entry.clone());
            }
            model.local_packages.insert(path, entry);
        }
        if strict {
            for (path, c) in &state.components {
                if c.toolchain == Toolchain::Rust {
                    model.members(path)?;
                }
            }
        }
        model.bind_local_paths(&direct_paths, overrides, selection)?;
        let mut patch_ids = BTreeSet::new();
        for patch in &model.patches {
            if let Some(path) = &patch.target.path {
                ensure!(
                    patch_ids.insert((&patch.origin, &model.local_packages[path].id)),
                    "Multiple Cargo patches select {} {} in the same source",
                    model.local_packages[path].id.name,
                    model.local_packages[path].id.version
                );
                ensure!(
                    model.patch_matches_id(patch, &model.local_packages[path].id)?,
                    "Cargo patch {} does not match the package at {path}/Cargo.toml",
                    patch.target.name
                );
            }
        }
        Ok(model)
    }

    fn registry_indexes(&self) -> Result<BTreeMap<String, String>> {
        let mut registries = BTreeMap::from([("crates-io".to_owned(), CRATES_IO.to_owned())]);
        for config in &self.cargo_config_order {
            for (name, registry) in dependency_table(&self.cargo_configs[config], "registries")? {
                if let Some(index) = registry.get("index").and_then(Value::as_str) {
                    registries.insert(name, index.to_owned());
                }
            }
        }
        Ok(registries)
    }
    fn root_config_inputs(
        &mut self,
        files: &mut BTreeMap<String, Option<FileIdentity>>,
    ) -> Result<BTreeSet<String>> {
        let mut config_targets = BTreeSet::new();
        for name in [".cargo/config", ".cargo/config.toml"] {
            record(self.root, files, name, false)?;
        }
        if let Some(config) = [".cargo/config", ".cargo/config.toml"]
            .into_iter()
            .find(|n| files[*n].is_some())
        {
            self.cargo_config_inputs(
                config,
                files,
                &mut BTreeSet::new(),
                &mut BTreeSet::new(),
                &mut config_targets,
                0,
            )?;
        }
        Ok(config_targets)
    }
    fn configured_model(
        &mut self,
        state: &ProjectState,
        strict: bool,
        files: &mut BTreeMap<String, Option<FileIdentity>>,
        selection: Selection<'_>,
    ) -> Result<CargoWorkspaceModel> {
        let config_targets = self.root_config_inputs(files)?;
        let mut model = self.root_model(state, strict, &config_targets, selection)?;
        let workspace = dependency_table(&self.cargo[""], "workspace")?;
        model.default_members = self.workspace_directory_list("", &workspace, "default-members")?;
        let members = self
            .root_members
            .as_ref()
            .expect("root membership initialized");
        for path in model.default_members.difference(members) {
            ensure!(
                !strict && !crate::state::snapshot_excluded_at(self.root, state, path),
                "Cargo workspace.default-members selects {path}, which is not a member of the root workspace"
            );
        }
        self.cargo_defaults.clone_from(&model.default_members);
        Ok(model)
    }

    /// Capture the Cargo inputs independently of Node write directories.
    fn root_inputs(
        &mut self,
        state: &ProjectState,
        files: &mut BTreeMap<String, Option<FileIdentity>>,
        strict: bool,
        selection: Selection<'_>,
    ) -> Result<CargoWorkspaceModel> {
        for name in ["rust-toolchain.toml", "rustfmt.toml"] {
            record(self.root, files, name, true)?;
        }
        if !strict {
            record(self.root, files, "Cargo.lock", false)?;
        }
        for name in ["rust-toolchain", ".rustfmt.toml"] {
            record(self.root, files, name, false)?;
            ensure!(
                files[name].is_none(),
                "Competing root Rust configuration: {name}"
            );
        }
        let model = self.configured_model(state, strict, files, selection)?;
        for path in &model.shadow_manifests {
            record(self.root, files, path, false)?;
        }
        let mut dirs = BTreeSet::new();
        for path in state
            .components
            .iter()
            .filter(|(_, c)| c.toolchain == Toolchain::Rust)
            .map(|(path, _)| path)
            .chain(model.components.values().flatten().map(|m| &m.rel_dir))
        {
            let mut dir = path.clone();
            while !dir.is_empty() && dirs.insert(dir.clone()) {
                dir = parent_name(&dir);
            }
        }
        for dir in dirs {
            let name = prefixed(&dir, "Cargo.toml");
            let nested_workspace = if let Some(manifest) = self.cargo.get(&dir) {
                manifest.contains_key("workspace")
            } else {
                // An ancestor outside the resolver's input set can still
                // shadow the workspace. Track its identity for admission.
                let ancestor = self.read_cargo_manifest(&name)?;
                let nested = ancestor
                    .as_ref()
                    .is_some_and(|(m, _)| m.contains_key("workspace"));
                files.insert(name.clone(), ancestor.map(|(_, id)| id));
                nested
            };
            ensure!(!nested_workspace, "Nested Cargo workspace at {name}");
            for name in [
                "Cargo.lock",
                "rust-toolchain.toml",
                "rust-toolchain",
                "rustfmt.toml",
                ".rustfmt.toml",
                ".cargo/config",
                ".cargo/config.toml",
            ] {
                let name = prefixed(&dir, name);
                record(self.root, files, &name, false)?;
                ensure!(
                    files[&name].is_none(),
                    "Competing Cargo workspace input: {name}"
                );
            }
        }
        Ok(model)
    }

    pub(super) fn capture_root_cargo(
        &mut self,
        state: &ProjectState,
        files: &mut BTreeMap<String, Option<FileIdentity>>,
        strict: bool,
    ) -> Result<()> {
        if let Some(captured) = self.root_cargo_files.take() {
            files.extend(captured);
        } else {
            let lock = read_regular_file(
                &input_path(self.root, "Cargo.lock")?,
                NATIVE_METADATA_MAX_BYTES,
                "root Cargo.lock",
            )?
            .map(|(b, _)| Lock::parse(&b))
            .transpose()?;
            let selection = Selection {
                lock: lock.as_ref(),
                witnesses: state
                    .cargo_resolution
                    .as_ref()
                    .and_then(|p| p.inputs.local_selections.as_ref()),
            };
            if let Err(error) = self.root_inputs(state, files, strict, selection) {
                if strict || lock.is_none() {
                    return Err(error);
                }
                // Mutation preflight may observe a previously active optional
                // manifest being removed. Snapshot its file and recheck the
                // staged selection after resolution.
                self.root_inputs(state, files, false, Selection::default())?;
            }
        }
        Ok(())
    }

    pub(super) fn root_cargo_graph(
        &mut self,
        state: &ProjectState,
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut files = BTreeMap::new();
        let lock_file = input_path(self.root, "Cargo.lock")?;
        let (bytes, id) = read_regular_file(&lock_file, NATIVE_METADATA_MAX_BYTES, "root Cargo.lock")?
            .context("Missing root Cargo.lock; restore it or run make exec toolchain=rust write=1 network=bridge command='cargo generate-lockfile'")?;
        let lock = crate::cargo_lock::Lock::parse(&bytes)
            .context("Invalid root Cargo.lock; restore it from Git or remove it and run make exec toolchain=rust write=1 network=bridge command='cargo generate-lockfile', then make sync")?;
        let model = self.root_inputs(
            state,
            &mut files,
            true,
            Selection {
                lock: Some(&lock),
                witnesses: state
                    .cargo_resolution
                    .as_ref()
                    .and_then(|p| p.inputs.local_selections.as_ref()),
            },
        )?;
        files.insert("Cargo.lock".into(), Some(id.clone()));
        if let Some(proof) = &state.cargo_resolution {
            ensure!(
                proof.version == 1
                    && proof.lock == id.sha256
                    && proof.inputs.digest == self.fingerprint_resolution(state, &model)?.0.digest,
                "Cargo resolution inputs or Cargo.lock changed; run make sync to confirm the workspace lock"
            );
        }
        let graph = model.selected_edges(state, &lock)?;
        self.read_identities.insert("Cargo.lock".into(), id);
        self.root_cargo_files = Some(files);
        self.cargo_workspace = Some(model);
        Ok(graph)
    }
}

impl CargoWorkspaceModel {
    fn selected_edges(
        &self,
        state: &ProjectState,
        lock: &crate::cargo_lock::Lock,
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        ensure!(
            !self.needs_path_witnesses(),
            "Cargo.lock contains ambiguous local paths; run make sync to record the directories selected by Cargo"
        );
        self.check_overrides(lock)?;
        ensure!(
            self.patches_match_lock(lock)?,
            "Cargo.lock patches differ from the Cargo workspace; run make sync"
        );
        let workspace_ids: BTreeSet<_> =
            self.components.values().flatten().map(|m| &m.id).collect();
        // Pass 1: single O(P + E) reachability walk from all workspace members.
        let mut reached = BTreeSet::new();
        let mut all_queue = VecDeque::new();
        for members in self.components.values() {
            for member in members {
                ensure!(
                    lock.packages.contains_key(&member.id),
                    "Cargo.lock is missing {} {} at {}/Cargo.toml; run make sync",
                    member.id.name,
                    member.id.version,
                    member.rel_dir
                );
                all_queue.push_back(&member.id);
            }
        }
        let mut has_external_to_local = false;
        while let Some(id) = all_queue.pop_front() {
            if !reached.insert(id) {
                continue;
            }
            let package = &lock.packages[id];
            if id.source.is_none() {
                let selected = self.local_package(id)?;
                let member = workspace_ids.contains(id);
                ensure!(
                    self.matches_direct(selected, member, &package.dependencies)?,
                    "Cargo.lock differs from {}/Cargo.toml: missing or no longer declared dependency; run make sync",
                    selected.rel_dir
                );
            } else if package
                .dependencies
                .iter()
                .chain(&package.replacement)
                .any(|child| child.source.is_none())
            {
                has_external_to_local = true;
            }
            all_queue.extend(package.dependencies.iter().chain(&package.replacement));
        }
        for id in lock.packages.keys() {
            ensure!(
                reached.contains(id),
                "Cargo.lock has an unreachable package {} {}; run make sync",
                id.name,
                id.version
            );
        }
        // Pass 2: derive per-component Make DAG edges. Without external->local
        // patch/replace edges, external subtrees cannot reach any local crate,
        // so each component walks only its local package subgraph (O(C * L)).
        let mut graph = BTreeMap::new();
        for (component, members) in &self.components {
            let mut queue: VecDeque<_> = members.iter().map(|m| &m.id).collect();
            let mut visited = BTreeSet::new();
            let mut deps = BTreeSet::new();
            while let Some(id) = queue.pop_front() {
                if !visited.insert(id) {
                    continue;
                }
                if id.source.is_none() {
                    let path = &self.local_package(id)?.rel_dir;
                    if let Some(owner) = rust_component_owner(state, path)
                        && owner != component
                    {
                        register_edge(state, component, owner.to_owned(), &mut deps)?;
                    }
                }
                let package = &lock.packages[id];
                for child in package.dependencies.iter().chain(&package.replacement) {
                    if has_external_to_local || child.source.is_none() {
                        queue.push_back(child);
                    }
                }
            }
            graph.insert(component.clone(), deps);
        }
        Ok(graph)
    }
}
