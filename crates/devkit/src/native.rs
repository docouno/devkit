//! Admission of ecosystem manifests and the physical graph chosen by npm/Cargo.
//! This module never selects versions or hydrates missing artifact identities.
use crate::{
    Result,
    catalog::node_components,
    files::{
        digest, exists, identity, input_path, object, project_root, read_json, relative_name,
        safe_path, sorted_entries, text,
    },
    model::{FileIdentity, InputIdentitySet, ProjectState, Role, Toolchain},
};
use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::Path,
};

pub const DEPENDENCY_FIELDS: [&str; 4] = [
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "devDependencies",
];
pub type Fields = Map<String, Value>;
pub type Packages = BTreeMap<String, Fields>;

pub fn valid_dependency_name(name: &str) -> bool {
    if name.is_empty() || name.contains(['\\', '\0', '\n', '\r', '\t']) {
        return false;
    }
    if let Some(scoped) = name.strip_prefix('@') {
        scoped.split_once('/').is_some_and(|(scope, package)| {
            !scope.is_empty() && !package.is_empty() && !package.contains('/')
        })
    } else {
        !name.contains('/') && ![".", ".."].contains(&name)
    }
}
pub fn root_workspace_link(location: &str) -> bool {
    location
        .strip_prefix("node_modules/")
        .is_some_and(valid_dependency_name)
}

pub fn dependency_table(data: &Fields, field: &str) -> Result<Fields> {
    match data.get(field) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(value) => Ok(object(value, field)?.clone()),
    }
}

fn stable_bytes(path: &Path) -> Result<(Vec<u8>, FileIdentity)> {
    let before =
        identity(path)?.with_context(|| format!("Missing native input: {}", path.display()))?;
    let bytes = fs::read(path)?;
    ensure!(
        digest(&bytes) == before.sha256 && identity(path)? == Some(before.clone()),
        "Native input changed while reading: {}",
        path.display()
    );
    Ok((bytes, before))
}
fn manifest_snapshot(root: &Path, path: &str) -> Result<(Fields, FileIdentity)> {
    let name = if path.is_empty() {
        "package.json".into()
    } else {
        format!("{path}/package.json")
    };
    let (bytes, identity) = stable_bytes(&input_path(root, name)?)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    Ok((
        object(&value, &format!("native manifest {path}"))?.clone(),
        identity,
    ))
}
pub fn native_manifest(root: &Path, path: &str) -> Result<Fields> {
    Ok(manifest_snapshot(root, path)?.0)
}

pub fn assert_package_lock_mode(root: &Path) -> Result<()> {
    ensure!(
        identity(&input_path(root, "npm-shrinkwrap.json")?)?.is_none(),
        "Root npm-shrinkwrap.json conflicts with the accepted package-lock.json mode"
    );
    Ok(())
}

/// A single canonical SRI digest. Reject truncated hashes, noncanonical Base64,
/// unknown algorithms and multiple-token identities rather than guessing.
pub fn valid_integrity(value: &str) -> bool {
    let Some((algorithm, encoded)) = value.split_once('-') else {
        return false;
    };
    let expected = match algorithm {
        "sha1" => 20,
        "sha256" => 32,
        "sha384" => 48,
        "sha512" => 64,
        _ => return false,
    };
    let Ok(bytes) = STANDARD.decode(encoded) else {
        return false;
    };
    bytes.len() == expected && STANDARD.encode(bytes) == encoded
}

fn validate_lock_location(location: &str) -> Result<()> {
    ensure!(
        !location.starts_with('/')
            && !location.contains(['\\', '\0', '\n', '\r', '\t'])
            && !location.split('/').any(|part| part == ".." || part == "."),
        "External native lock path: {location}"
    );
    ensure!(
        location.is_empty() || !location.split('/').any(str::is_empty),
        "Invalid native lock path: {location}"
    );
    Ok(())
}

fn packages_snapshot(root: &Path) -> Result<(Packages, FileIdentity)> {
    assert_package_lock_mode(root)?;
    let (bytes, identity) = stable_bytes(&safe_path(root, "package-lock.json")?)?;
    let raw: Value = serde_json::from_slice(&bytes)?;
    let lock = object(&raw, "workspace lock")?;
    ensure!(
        lock.get("lockfileVersion") == Some(&json!(3)),
        "Expected native npm lockfileVersion 3"
    );
    let records = object(
        lock.get("packages").context("Missing lock packages")?,
        "lock packages",
    )?;
    let mut packages = BTreeMap::new();
    for (location, value) in records {
        validate_lock_location(location)?;
        let record = object(value, location)?.clone();
        if location.contains("node_modules/") && record.get("link") != Some(&Value::Bool(true)) {
            let resolved = record.get("resolved").and_then(Value::as_str);
            let integrity = record.get("integrity").and_then(Value::as_str);
            ensure!(
                resolved.is_some_and(|v| v.starts_with("https://")
                    || v.starts_with("http://")
                    || v.starts_with("file:"))
                    && integrity.is_some_and(valid_integrity),
                "Unprovable native artifact identity: {location}"
            );
        }
        packages.insert(location.clone(), record);
    }
    assert_package_lock_mode(root)?;
    Ok((packages, identity))
}
pub fn package_lock(root: &Path) -> Result<Packages> {
    Ok(packages_snapshot(root)?.0)
}

pub fn local_path(root: &Path, base: &str, value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && !value.contains('\0'),
        "Invalid native local dependency path"
    );
    let root = project_root(root)?;
    let target = crate::files::lexical(&root.join(base).join(value))?;
    ensure!(
        target != root,
        "Unsupported checkout-root local package location: registered package sources must live below the checkout (for example apps/ or packages/)"
    );
    let target = input_path(&root, target)?;
    relative_name(&root, &target)
}

pub fn resolve_lock_node(packages: &Packages, location: &str) -> Result<String> {
    let mut location = location.to_owned();
    let mut seen = BTreeSet::new();
    loop {
        let record = packages
            .get(&location)
            .with_context(|| format!("Native lock target is missing: {location}"))?;
        if record.get("link") != Some(&Value::Bool(true)) {
            return Ok(location);
        }
        ensure!(
            seen.insert(location.clone()),
            "Circular native link: {location}"
        );
        let target = text(
            record
                .get("resolved")
                .context("Missing native link target")?,
            "link target",
        )?;
        let target = target.strip_prefix("./").unwrap_or(target);
        validate_lock_location(target)?;
        location = target.to_owned();
    }
}

/// Follow the already chosen physical npm graph. There is no semver resolver.
pub fn locked_dependency(packages: &Packages, from: &str, name: &str) -> Result<Option<String>> {
    ensure!(
        valid_dependency_name(name),
        "Invalid dependency key: {name}"
    );
    let mut directory = from.to_owned();
    loop {
        let candidate = if directory.is_empty() {
            format!("node_modules/{name}")
        } else {
            format!("{directory}/node_modules/{name}")
        };
        if packages.contains_key(&candidate) {
            return Ok(Some(resolve_lock_node(packages, &candidate)?));
        }
        if directory.is_empty() {
            return Ok(None);
        }
        directory = directory
            .rsplit_once('/')
            .map_or_else(String::new, |(parent, _)| parent.to_owned());
    }
}

/// npm permits a missing optional dependency or optional peer. An
/// optionalDependencies entry also overrides the same key in dependencies.
/// Other direct, development and peer edges must have a physical lock target.
pub fn admitted_dependency_target(
    packages: &Packages,
    from: &str,
    manifest: &Fields,
    field: &str,
    name: &str,
) -> Result<Option<String>> {
    let target = locked_dependency(packages, from, name)?;
    let optional = field == "optionalDependencies"
        || (field == "dependencies"
            && dependency_table(manifest, "optionalDependencies")?.contains_key(name))
        || (field == "peerDependencies" && {
            let metadata = dependency_table(manifest, "peerDependenciesMeta")?;
            match metadata.get(name) {
                None | Some(Value::Null) => false,
                Some(value) => {
                    object(value, "peer metadata")?.get("optional") == Some(&Value::Bool(true))
                }
            }
        });
    ensure!(
        target.is_some() || optional,
        "Native lock omits dependency {from}:{name}"
    );
    Ok(target)
}

pub fn local_dependency_map(
    root: &Path,
    state: &ProjectState,
    path: &str,
) -> Result<BTreeMap<String, String>> {
    if state
        .components
        .get(path)
        .is_none_or(|c| c.toolchain != Toolchain::Node)
    {
        return Ok(BTreeMap::new());
    }
    let manifest = native_manifest(root, path)?;
    let packages = package_lock(root)?;
    let mut result = BTreeMap::new();
    for field in DEPENDENCY_FIELDS {
        for name in dependency_table(&manifest, field)?.keys() {
            if let Some(target) = locked_dependency(&packages, path, name)?
                && state.components.contains_key(&target)
            {
                result.insert(name.clone(), target);
            }
        }
    }
    Ok(result)
}

#[derive(Default, Clone)]
struct CargoInputs {
    dependencies: BTreeSet<String>,
    legacy_dependencies: BTreeSet<String>,
    members: BTreeSet<String>,
    overrides: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CargoPackageId {
    name: String,
    version: String,
    source: Option<String>,
}
#[derive(Clone)]
struct LockedCargoPackage {
    id: CargoPackageId,
    dependencies: Vec<String>,
    replacement: Option<String>,
}
#[derive(Clone)]
struct CargoLockGraph {
    packages: Vec<LockedCargoPackage>,
}
impl CargoLockGraph {
    fn package_ref(&self, reference: &str, replacement: bool) -> Result<usize> {
        let (identity, source) = if let Some((identity, source)) = reference.split_once(" (") {
            (
                identity,
                Some(
                    source
                        .strip_suffix(')')
                        .context("Invalid Cargo lock dependency source")?,
                ),
            )
        } else {
            (reference, None)
        };
        let parts: Vec<_> = identity.split_whitespace().collect();
        ensure!(
            (1..=2).contains(&parts.len()),
            "Unsupported Cargo lock dependency reference: {reference}"
        );
        // Cargo's version-qualified reference without a source denotes an
        // existing local lock record when one shares its name/version with a
        // source-qualified registry/Git record. Do not infer this from a nearby
        // manifest, and keep unversioned or remote-only ambiguity fail-closed.
        let compact_local = source.is_none()
            && parts.len() == 2
            && self.packages.iter().any(|package| {
                package.id.name == parts[0]
                    && package.id.version == parts[1]
                    && package.id.source.is_none()
            });
        let selected: Vec<_> = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| {
                package.id.name == parts[0]
                    && (parts.len() == 1 || package.id.version == parts[1])
                    && source.is_none_or(|source| package.id.source.as_deref() == Some(source))
                    && (!compact_local || package.id.source.is_none())
                    // A compact replacement ref denotes the local target;
                    // Cargo also retains the replaced registry record with
                    // the same name/version and an explicit source.
                    && (!replacement || source.is_some() || package.id.source.is_none())
            })
            .map(|(index, _)| index)
            .collect();
        ensure!(
            selected.len() == 1,
            "Cargo lock dependency reference is missing or ambiguous: {reference}; refresh the component Cargo.lock with pinned Cargo"
        );
        Ok(selected[0])
    }
    fn local_id(&self, identity: &CargoPackageId) -> Result<usize> {
        let selected: Vec<_> = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| &package.id == identity)
            .map(|(index, _)| index)
            .collect();
        ensure!(
            selected.len() == 1,
            "Cargo.lock does not uniquely identify workspace package {} {}; refresh the component Cargo.lock with pinned Cargo",
            identity.name,
            identity.version
        );
        Ok(selected[0])
    }
}
#[derive(Default)]
struct CargoLocalContext {
    paths: BTreeSet<String>,
    config_paths: BTreeSet<String>,
}
struct DerivedDependencies {
    projected: BTreeMap<String, Vec<String>>,
    effective: BTreeMap<String, Vec<String>>,
}

/// Raw facts which a saved admission must retain without reparsing native
/// manifests. Namespace inventory detects new directories; the negative
/// manifest identities detect native manifests added to an existing directory.
#[derive(Clone, Serialize)]
struct NativeStructure {
    namespaces: BTreeMap<String, Option<BTreeMap<String, String>>>,
    manifests: BTreeMap<String, Option<FileIdentity>>,
}
impl NativeStructure {
    fn namespace_inventory(
        root: &Path,
    ) -> Result<BTreeMap<String, Option<BTreeMap<String, String>>>> {
        let mut result = BTreeMap::new();
        for area in ["apps", "packages"] {
            let directory = input_path(root, area)?;
            let entries = if exists(&directory)? {
                let mut entries = BTreeMap::new();
                for entry in sorted_entries(&directory)? {
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("Non-UTF8 component name"))?;
                    let metadata = fs::symlink_metadata(entry.path())?;
                    let kind = if metadata.file_type().is_symlink() {
                        "symlink"
                    } else if metadata.is_dir() {
                        "directory"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    entries.insert(name, kind.to_owned());
                }
                Some(entries)
            } else {
                None
            };
            result.insert(area.to_owned(), entries);
        }
        Ok(result)
    }
    fn capture(root: &Path, state: &ProjectState) -> Result<Self> {
        let namespaces = Self::namespace_inventory(root)?;
        let mut candidates: BTreeSet<String> = state.components.keys().cloned().collect();
        for (area, entries) in &namespaces {
            if let Some(entries) = entries {
                for (name, kind) in entries {
                    if kind == "directory" || kind == "symlink" {
                        candidates.insert(format!("{area}/{name}"));
                    }
                }
            }
        }
        let mut manifests = BTreeMap::new();
        for path in candidates {
            for name in ["package.json", "Cargo.toml"] {
                let name = format!("{path}/{name}");
                manifests.insert(name.clone(), identity(&input_path(root, name)?)?);
            }
        }
        Ok(Self {
            namespaces,
            manifests,
        })
    }
    fn assert_registered(&self, state: &ProjectState) -> Result<()> {
        for (path, component) in &state.components {
            let node = self.manifests[&format!("{path}/package.json")].is_some();
            let rust = self.manifests[&format!("{path}/Cargo.toml")].is_some();
            ensure!(!(node && rust), "Ambiguous native manifests: {path}");
            ensure!(
                (component.toolchain == Toolchain::Node && node)
                    || (component.toolchain == Toolchain::Rust && rust),
                "Missing expected native {} manifest: {path}",
                component.toolchain
            );
        }
        for (name, identity) in &self.manifests {
            if identity.is_some() {
                let path = name.rsplit_once('/').context("Native manifest parent")?.0;
                ensure!(
                    state.components.contains_key(path),
                    "Unregistered native component: {path}"
                );
            }
        }
        Ok(())
    }
    fn recheck(&self, root: &Path) -> Result<()> {
        ensure!(
            Self::namespace_inventory(root)? == self.namespaces,
            "Native component namespace changed during admission"
        );
        Ok(())
    }
}

/// One admission owns this cache. Derivation and input closure share parsed
/// manifests and a single physical lock graph.
struct ReadContext<'a> {
    root: &'a Path,
    packages: Option<Packages>,
    manifests: BTreeMap<String, Fields>,
    cargo: BTreeMap<String, Fields>,
    cargo_configs: BTreeMap<String, Fields>,
    cargo_locks: BTreeMap<String, CargoLockGraph>,
    node_inputs: Option<InputIdentitySet>,
    read_identities: BTreeMap<String, FileIdentity>,
}
impl<'a> ReadContext<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            packages: None,
            manifests: BTreeMap::new(),
            cargo: BTreeMap::new(),
            cargo_configs: BTreeMap::new(),
            cargo_locks: BTreeMap::new(),
            node_inputs: None,
            read_identities: BTreeMap::new(),
        }
    }
    fn packages(&mut self) -> Result<&Packages> {
        if self.packages.is_none() {
            let (packages, identity) = packages_snapshot(self.root)?;
            self.packages = Some(packages);
            self.read_identities
                .insert("package-lock.json".into(), identity);
        }
        Ok(self.packages.as_ref().expect("loaded packages"))
    }
    fn manifest(&mut self, path: &str) -> Result<Fields> {
        if !self.manifests.contains_key(path) {
            let (manifest, identity) = manifest_snapshot(self.root, path)?;
            self.manifests.insert(path.to_owned(), manifest);
            self.read_identities
                .insert(prefixed(path, "package.json"), identity);
        }
        Ok(self.manifests[path].clone())
    }
    fn cargo_manifest(&mut self, path: &str) -> Result<Fields> {
        if !self.cargo.contains_key(path) {
            let name = prefixed(path, "Cargo.toml");
            let (bytes, identity) = stable_bytes(&input_path(self.root, &name)?)?;
            let parsed: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)
                .with_context(|| format!("Invalid Cargo manifest: {path}"))?;
            let value = serde_json::to_value(parsed)?;
            self.cargo
                .insert(path.to_owned(), object(&value, "Cargo manifest")?.clone());
            self.read_identities.insert(name, identity);
        }
        Ok(self.cargo[path].clone())
    }
    fn cargo_inputs(&mut self, path: &str) -> Result<CargoInputs> {
        let manifest = self.cargo_manifest(path)?;
        let mut result = CargoInputs::default();
        self.collect_cargo_tables(path, &manifest, &mut result, true)?;
        for target in dependency_table(&manifest, "target")?.values() {
            self.collect_cargo_tables(path, object(target, "Cargo target")?, &mut result, true)?;
        }
        let workspace = dependency_table(&manifest, "workspace")?;
        self.collect_cargo_tables(path, &workspace, &mut result, false)?;
        self.collect_cargo_overrides(path, &manifest, &mut result.overrides)?;
        if let Some(members) = workspace.get("members") {
            for member in members
                .as_array()
                .context("Cargo workspace members must be an array")?
            {
                let value = text(member, "Cargo workspace member")?;
                ensure!(
                    !value.contains(['?', '*', '[', ']']),
                    "Cargo workspace member globs require explicit registration"
                );
                result
                    .members
                    .insert(cargo_local_path(self.root, path, value)?);
            }
        }
        Ok(result)
    }
    fn collect_cargo_tables(
        &mut self,
        base: &str,
        data: &Fields,
        result: &mut CargoInputs,
        inherited: bool,
    ) -> Result<()> {
        for field in ["dependencies", "dev-dependencies", "build-dependencies"] {
            for (name, dependency) in dependency_table(data, field)? {
                let Some(table) = dependency.as_object() else {
                    continue;
                };
                reject_local_cargo_git(table)?;
                if let Some(path) = table.get("path") {
                    let path =
                        cargo_local_path(self.root, base, text(path, "Cargo dependency path")?)?;
                    result.dependencies.insert(path.clone());
                    result.legacy_dependencies.insert(path);
                } else if inherited && table.get("workspace") == Some(&Value::Bool(true)) {
                    let (workspace_root, workspace) = self.find_cargo_workspace(base)?;
                    let dependencies = dependency_table(&workspace, "dependencies")?;
                    let inherited = dependencies
                        .get(&name)
                        .with_context(|| format!("Missing inherited Cargo dependency: {name}"))?;
                    if let Some(declaration) = inherited.as_object() {
                        reject_local_cargo_git(declaration)?;
                    }
                    if let Some(path) = inherited.as_object().and_then(|d| d.get("path")) {
                        result.dependencies.insert(cargo_local_path(
                            self.root,
                            &workspace_root,
                            text(path, "inherited Cargo path")?,
                        )?);
                    }
                }
            }
        }
        Ok(())
    }
    fn collect_cargo_overrides(
        &self,
        base: &str,
        data: &Fields,
        targets: &mut BTreeSet<String>,
    ) -> Result<()> {
        for source in dependency_table(data, "patch")?.values() {
            self.collect_cargo_override_entries(
                base,
                object(source, "Cargo patch source")?,
                targets,
            )?;
        }
        self.collect_cargo_override_entries(base, &dependency_table(data, "replace")?, targets)
    }
    fn collect_cargo_override_entries(
        &self,
        base: &str,
        entries: &Fields,
        targets: &mut BTreeSet<String>,
    ) -> Result<()> {
        for value in entries.values() {
            let Some(dependency) = value.as_object() else {
                continue;
            };
            reject_local_cargo_git(dependency)?;
            if let Some(path) = dependency.get("path") {
                targets.insert(cargo_local_path(
                    self.root,
                    base,
                    text(path, "Cargo override path")?,
                )?);
            }
        }
        Ok(())
    }
    fn cargo_config(&mut self, name: &str) -> Result<Fields> {
        if !self.cargo_configs.contains_key(name) {
            let (bytes, identity) = stable_bytes(&input_path(self.root, name)?)?;
            let parsed: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)
                .with_context(|| format!("Invalid Cargo config: {name}"))?;
            let value = serde_json::to_value(parsed)?;
            self.cargo_configs
                .insert(name.to_owned(), object(&value, "Cargo config")?.clone());
            self.read_identities.insert(name.to_owned(), identity);
        }
        Ok(self.cargo_configs[name].clone())
    }
    fn cargo_config_inputs(
        &mut self,
        name: &str,
        files: &mut BTreeMap<String, Option<FileIdentity>>,
        seen: &mut BTreeSet<String>,
        active: &mut BTreeSet<String>,
        targets: &mut BTreeSet<String>,
        config_paths: &mut BTreeSet<String>,
    ) -> Result<()> {
        if seen.contains(name) {
            return Ok(());
        }
        ensure!(
            active.insert(name.to_owned()),
            "Cargo config include cycle: {name}"
        );
        record(self.root, files, name, true)?;
        let data = self.cargo_config(name)?;
        let path = input_path(self.root, name)?;
        let directory = path.parent().context("Cargo config parent")?;
        if let Some(includes) = data.get("include") {
            for include in includes
                .as_array()
                .context("Cargo config include must be an array")?
            {
                let (reference, optional) = if let Some(value) = include.as_str() {
                    (value, false)
                } else {
                    let include = object(include, "Cargo config include")?;
                    ensure!(
                        include
                            .keys()
                            .all(|key| ["path", "optional"].contains(&key.as_str())),
                        "Unsupported Cargo config include fields: {name}"
                    );
                    let optional = include
                        .get("optional")
                        .map(|v| {
                            v.as_bool()
                                .context("Cargo include optional must be boolean")
                        })
                        .transpose()?
                        .unwrap_or(false);
                    (
                        text(
                            include
                                .get("path")
                                .context("Missing Cargo config include path")?,
                            "Cargo config include path",
                        )?,
                        optional,
                    )
                };
                ensure!(
                    reference.ends_with(".toml"),
                    "Cargo config include must name a .toml file: {reference}"
                );
                let target = input_path(self.root, directory.join(reference))?;
                let target_name = relative_name(&project_root(self.root)?, &target)?;
                record(self.root, files, &target_name, !optional)?;
                if identity(&target)?.is_some() {
                    self.cargo_config_inputs(
                        &target_name,
                        files,
                        seen,
                        active,
                        targets,
                        config_paths,
                    )?;
                }
            }
        }
        // Cargo resolves file-defined values relative to two levels above the
        // defining config file, including --config/include files.
        let base = directory
            .parent()
            .context("Cargo config value base")?
            .to_str()
            .context("Non-UTF8 Cargo config base")?;
        if let Some(paths) = data.get("paths") {
            for value in paths
                .as_array()
                .context("Cargo config paths must be an array")?
            {
                let target =
                    cargo_local_path(self.root, base, text(value, "Cargo config override path")?)?;
                config_paths.insert(target.clone());
                targets.insert(target);
            }
        }
        self.collect_cargo_overrides(base, &data, targets)?;
        for source in dependency_table(&data, "source")?.values() {
            let source = object(source, "Cargo source")?;
            for field in ["directory", "local-registry"] {
                ensure!(
                    !source.contains_key(field),
                    "Unsupported local Cargo source store: source.{field}; use a confined path or patch dependency"
                );
            }
            reject_local_cargo_git(source)?;
            ensure!(
                !source
                    .get("registry")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.starts_with("file:")),
                "Unsupported local Cargo registry source: source.registry"
            );
        }
        for registry in dependency_table(&data, "registries")?.values() {
            let registry = object(registry, "Cargo registry")?;
            ensure!(
                !registry
                    .get("index")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.starts_with("file:")),
                "Unsupported local Cargo registry source: registries.index"
            );
        }
        ensure!(
            !dependency_table(&data, "resolver")?.contains_key("lockfile-path"),
            "Unsupported Cargo resolver.lockfile-path: the accepted native lock is Cargo.lock"
        );
        active.remove(name);
        seen.insert(name.to_owned());
        Ok(())
    }
    fn find_cargo_workspace(&mut self, base: &str) -> Result<(String, Fields)> {
        let own = self.cargo_manifest(base)?;
        let package = dependency_table(&own, "package")?;
        if let Some(workspace) = package.get("workspace") {
            // A Cargo workspace may live at the checkout root. Constrain the
            // manifest file rather than treating the root directory as an input.
            let manifest_path = input_path(
                self.root,
                Path::new(base)
                    .join(text(workspace, "Cargo package.workspace")?)
                    .join("Cargo.toml"),
            )?;
            let target = relative_name(
                &project_root(self.root)?,
                manifest_path.parent().context("Cargo workspace parent")?,
            )?;
            let manifest = self.cargo_manifest(&target)?;
            return Ok((target, dependency_table(&manifest, "workspace")?));
        }
        let mut directory = base.to_owned();
        loop {
            let path = input_path(self.root, prefixed(&directory, "Cargo.toml"))?;
            if exists(&path)? {
                let manifest = self.cargo_manifest(&directory)?;
                if let Some(workspace) = manifest.get("workspace") {
                    return Ok((directory, object(workspace, "Cargo workspace")?.clone()));
                }
            }
            if directory.is_empty() {
                break;
            }
            directory = parent_name(&directory);
        }
        bail!("Cargo workspace dependency has no project-local workspace: {base}")
    }
    fn cargo_lock_graph(&mut self, component: &str) -> Result<CargoLockGraph> {
        if !self.cargo_locks.contains_key(component) {
            let name = prefixed(component, "Cargo.lock");
            let (bytes, identity) = stable_bytes(&input_path(self.root, &name)?)?;
            let parsed: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)?;
            let raw = serde_json::to_value(parsed)?;
            let data = object(&raw, "Cargo lock")?;
            ensure!(
                matches!(data.get("version").and_then(Value::as_u64), Some(3 | 4)),
                "Selected Cargo component graph requires component Cargo.lock version 3 or 4"
            );
            let mut packages = Vec::new();
            let mut seen = BTreeSet::new();
            for package in data.get("package").and_then(Value::as_array).context("Cargo.lock cannot prove the selected component graph without package records; refresh it with pinned Cargo")? {
                let package = object(package, "locked Cargo package")?;
                let id = CargoPackageId {
                    name: text(package.get("name").context("Missing locked Cargo package name")?, "locked Cargo package name")?.to_owned(),
                    version: text(package.get("version").context("Missing locked Cargo version")?, "locked Cargo version")?.to_owned(),
                    source: package.get("source").map(|value| text(value,"locked Cargo source").map(str::to_owned)).transpose()?,
                };
                ensure!(seen.insert(id.clone()), "Duplicate Cargo lock package identity: {} {}",id.name,id.version);
                let dependencies = package.get("dependencies").map(|value| value.as_array().context("Cargo lock dependencies must be an array")).transpose()?.into_iter().flatten()
                    .map(|value| text(value,"Cargo lock dependency").map(str::to_owned)).collect::<Result<Vec<_>>>()?;
                let replacement = package.get("replace").map(|value| text(value,"Cargo lock replacement").map(str::to_owned)).transpose()?;
                packages.push(LockedCargoPackage {id,dependencies,replacement});
            }
            self.cargo_locks
                .insert(component.to_owned(), CargoLockGraph { packages });
            self.read_identities.insert(name, identity);
        }
        Ok(self.cargo_locks[component].clone())
    }
    fn cargo_package_id(&mut self, path: &str) -> Result<Option<CargoPackageId>> {
        let data = self.cargo_manifest(path)?;
        let Some(package) = data.get("package") else {
            return Ok(None);
        };
        let package = object(package, "Cargo package identity")?;
        let name = text(
            package.get("name").context("Missing Cargo package name")?,
            "Cargo package name",
        )?
        .to_owned();
        let version = match package.get("version") {
            None => "0.0.0".to_owned(),
            Some(Value::String(version)) if !version.is_empty() => version.clone(),
            Some(value)
                if value.as_object().and_then(|v| v.get("workspace"))
                    == Some(&Value::Bool(true)) =>
            {
                let (_, workspace) = self.find_cargo_workspace(path)?;
                let inherited = dependency_table(&workspace, "package")?;
                text(
                    inherited
                        .get("version")
                        .context("Missing workspace.package.version")?,
                    "workspace package version",
                )?
                .to_owned()
            }
            _ => bail!("Unsupported Cargo package version declaration: {path}"),
        };
        Ok(Some(CargoPackageId {
            name,
            version,
            source: None,
        }))
    }
    fn cargo_workspace_member_paths(&mut self, component: &str) -> Result<BTreeSet<String>> {
        let mut queue = VecDeque::from([component.to_owned()]);
        let mut members = BTreeSet::new();
        while let Some(path) = queue.pop_front() {
            if !members.insert(path.clone()) {
                continue;
            }
            queue.extend(self.cargo_inputs(&path)?.members);
        }
        Ok(members)
    }
    fn cargo_workspace_packages(&mut self, component: &str) -> Result<BTreeSet<String>> {
        let manifest = self.cargo_manifest(component)?;
        if !manifest.contains_key("workspace") {
            ensure!(
                !dependency_table(&manifest, "package")?.contains_key("workspace"),
                "Selected Cargo component graph requires component-root workspace/lock authority; inherited workspace context does not change the accepted Cargo.lock location"
            );
            let mut ancestor = parent_name(component);
            loop {
                if exists(&input_path(self.root, prefixed(&ancestor, "Cargo.toml"))?)? {
                    ensure!(
                        !self.cargo_manifest(&ancestor)?.contains_key("workspace"),
                        "Selected Cargo component graph requires component-root workspace/lock authority; refresh the component layout explicitly"
                    );
                }
                if ancestor.is_empty() {
                    break;
                }
                ancestor = parent_name(&ancestor);
            }
        }
        self.cargo_workspace_member_paths(component)
    }
    fn cargo_selected_edges(
        &mut self,
        component: &str,
        state: &ProjectState,
        local: &CargoLocalContext,
        deps: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !local
            .paths
            .iter()
            .any(|path| component_owner(state, path).is_some_and(|owner| owner != component))
        {
            return Ok(());
        }
        let lock = self.cargo_lock_graph(component)?;
        let workspace = self.cargo_workspace_packages(component)?;
        let mut candidates: BTreeMap<CargoPackageId, BTreeSet<String>> = BTreeMap::new();
        for path in &local.paths {
            if let Some(id) = self.cargo_package_id(path)? {
                candidates.entry(id).or_default().insert(path.clone());
            }
        }
        let mut pinned = BTreeMap::new();
        let mut queue = VecDeque::new();
        for path in workspace {
            if let Some(id) = self.cargo_package_id(&path)? {
                let index = lock.local_id(&id)?;
                ensure!(
                    pinned.insert(index, path.clone()).is_none(),
                    "Ambiguous local Cargo workspace package identity: {} {}",
                    id.name,
                    id.version
                );
                queue.push_back(index);
            }
        }
        ensure!(
            !queue.is_empty(),
            "Component Cargo.lock has no identifiable workspace package roots: {component}"
        );
        let mut seen = BTreeSet::new();
        while let Some(index) = queue.pop_front() {
            if !seen.insert(index) {
                continue;
            }
            let package = &lock.packages[index];
            for (reference, replacement) in package
                .dependencies
                .iter()
                .map(|reference| (reference, false))
                .chain(
                    package
                        .replacement
                        .iter()
                        .map(|reference| (reference, true)),
                )
            {
                let target = lock.package_ref(reference, replacement)?;
                let id = &lock.packages[target].id;
                // paths overrides can replace code while the lock preserves a
                // registry source. Do not silently relabel such a record.
                for path in &local.config_paths {
                    if id.source.is_some()
                        && component_owner(state, path).is_some()
                        && self
                            .cargo_package_id(path)?
                            .as_ref()
                            .is_some_and(|candidate| {
                                candidate.name == id.name && candidate.version == id.version
                            })
                    {
                        bail!(
                            "Cannot prove registered Cargo paths override {path} from Cargo.lock; use an explicit path/patch dependency or remove that paths override"
                        );
                    }
                }
                if id.source.is_none() {
                    let target_path = if let Some(path) = pinned.get(&target) {
                        path.clone()
                    } else {
                        let paths=candidates.get(id).with_context(||format!("Cargo.lock selected a local package without an admitted manifest candidate: {} {}",id.name,id.version))?;
                        ensure!(
                            paths.len() == 1,
                            "Ambiguous local Cargo target {} {}: multiple admitted paths; make the chosen local source explicit",
                            id.name,
                            id.version
                        );
                        paths.iter().next().expect("one local candidate").clone()
                    };
                    if let Some(owner) = component_owner(state, &target_path)
                        && owner != component
                    {
                        register_edge(state, component, owner.to_owned(), deps)?;
                    }
                }
                queue.push_back(target);
            }
        }
        Ok(())
    }
    fn derive(&mut self, state: &ProjectState) -> Result<DerivedDependencies> {
        if !node_components(state).is_empty() {
            self.packages()?;
        }
        let mut projected = BTreeMap::new();
        let mut effective = BTreeMap::new();
        for (path, component) in &state.components {
            let mut deps = BTreeSet::new();
            if component.toolchain == Toolchain::Node {
                let manifest = self.manifest(path)?;
                for field in DEPENDENCY_FIELDS {
                    for name in dependency_table(&manifest, field)?.keys() {
                        if let Some(target) = locked_dependency(self.packages()?, path, name)? {
                            register_edge(state, path, target, &mut deps)?;
                        }
                    }
                }
            } else {
                let native = self.cargo_inputs(path)?;
                let mut files = BTreeMap::new();
                let local =
                    self.cargo_local_context(VecDeque::from([path.clone()]), state, &mut files)?;
                self.cargo_selected_edges(path, state, &local, &mut deps)?;
                if matches!(state.version.as_str(), "0.2.0" | "0.2.1") {
                    let mut legacy = BTreeSet::new();
                    for target in native.legacy_dependencies {
                        register_edge(state, path, target, &mut legacy)?;
                    }
                    projected.insert(path.clone(), legacy.into_iter().collect());
                }
            }
            let deps: Vec<_> = deps.into_iter().collect();
            projected
                .entry(path.clone())
                .or_insert_with(|| deps.clone());
            effective.insert(path.clone(), deps);
        }
        assert_acyclic(&effective)?;
        Ok(DerivedDependencies {
            projected,
            effective,
        })
    }
    fn assert_npm_targets(&mut self) -> Result<()> {
        let packages = self.packages()?.clone();
        for (location, record) in &packages {
            if record.get("link") == Some(&Value::Bool(true)) {
                resolve_lock_node(&packages, location)?;
                continue;
            }
            let data = if !location.contains("node_modules/")
                && exists(&input_path(
                    self.root,
                    Path::new(location).join("package.json"),
                )?)? {
                self.manifest(location)?
            } else {
                record.clone()
            };
            for field in DEPENDENCY_FIELDS {
                for (name, spec) in dependency_table(&data, field)? {
                    text(&spec, &format!("{location}:{name}"))?;
                    admitted_dependency_target(&packages, location, &data, field, &name)?;
                }
            }
        }
        Ok(())
    }
    fn cargo_local_context(
        &mut self,
        mut rust_queue: VecDeque<String>,
        state: &ProjectState,
        files: &mut BTreeMap<String, Option<FileIdentity>>,
    ) -> Result<CargoLocalContext> {
        let roots: BTreeSet<_> = rust_queue.iter().cloned().collect();
        let mut result = CargoLocalContext::default();
        let mut rust_seen = BTreeSet::new();
        let mut seen_configs = BTreeSet::new();
        let mut active_configs = BTreeSet::new();
        while let Some(path) = rust_queue.pop_front() {
            if !rust_seen.insert(path.clone()) {
                continue;
            }
            record(self.root, files, &prefixed(&path, "Cargo.toml"), true)?;
            for name in ["Cargo.lock", "rust-toolchain.toml", "rustfmt.toml"] {
                record(
                    self.root,
                    files,
                    &prefixed(&path, name),
                    state.components.contains_key(&path),
                )?;
            }
            // Cargo discovers config in cwd ancestors. Include absence as well as
            // presence, so adding a new config cannot reuse a previous receipt.
            let mut directory = path.clone();
            let mut override_targets = BTreeSet::new();
            let mut config_paths = BTreeSet::new();
            loop {
                for name in [".cargo/config.toml", ".cargo/config", ".rustfmt.toml"] {
                    record(self.root, files, &prefixed(&directory, name), false)?;
                }
                let legacy_config = prefixed(&directory, ".cargo/config");
                let config = if files[&legacy_config].is_some() {
                    legacy_config
                } else {
                    prefixed(&directory, ".cargo/config.toml")
                };
                if files[&config].is_some() {
                    self.cargo_config_inputs(
                        &config,
                        files,
                        &mut seen_configs,
                        &mut active_configs,
                        &mut override_targets,
                        &mut config_paths,
                    )?;
                }
                if directory != path {
                    record(self.root, files, &prefixed(&directory, "Cargo.toml"), false)?;
                    if files[&prefixed(&directory, "Cargo.toml")].is_some() {
                        let ancestor = self.cargo_manifest(&directory)?;
                        self.collect_cargo_overrides(&directory, &ancestor, &mut override_targets)?;
                    }
                    record(self.root, files, &prefixed(&directory, "Cargo.lock"), false)?;
                    record(
                        self.root,
                        files,
                        &prefixed(&directory, "rustfmt.toml"),
                        false,
                    )?;
                }
                if directory.is_empty() {
                    break;
                }
                directory = parent_name(&directory);
            }
            let native = self.cargo_inputs(&path)?;
            if roots.contains(&path) {
                result.config_paths.extend(config_paths);
            }
            result.paths.insert(path);
            rust_queue.extend(native.dependencies);
            rust_queue.extend(native.members);
            rust_queue.extend(native.overrides);
            rust_queue.extend(override_targets);
        }
        Ok(result)
    }
    fn closure(&mut self, state: &ProjectState) -> Result<InputIdentitySet> {
        let structure = NativeStructure::capture(self.root, state)?;
        self.closure_with_structure(state, &structure)
    }
    fn node_closure(&mut self, state: &ProjectState) -> Result<InputIdentitySet> {
        let mut files = BTreeMap::new();
        let mut directories = BTreeSet::new();
        let mut queue: VecDeque<String> = node_components(state)
            .into_iter()
            .map(|(p, _)| p.clone())
            .collect();
        if !queue.is_empty() {
            queue.push_front(String::new());
            record(self.root, &mut files, "package-lock.json", true)?;
            let root = self.root;
            for (location, item) in self.packages()? {
                if !location.is_empty()
                    && !location.contains("node_modules/")
                    && exists(&input_path(root, Path::new(location).join("package.json"))?)?
                {
                    queue.push_back(location.clone());
                }
                if item.get("link") == Some(&Value::Bool(true)) {
                    queue.push_back(local_path(
                        root,
                        "",
                        text(
                            item.get("resolved").context("Missing link target")?,
                            "link target",
                        )?,
                    )?);
                } else if let Some(spec) = item.get("resolved").and_then(Value::as_str) {
                    local_dependency(root, "", spec, &mut queue, &mut files)?;
                }
            }
        }
        while let Some(path) = queue.pop_front() {
            if !directories.insert(path.clone()) {
                continue;
            }
            record(
                self.root,
                &mut files,
                &prefixed(&path, "package.json"),
                true,
            )?;
            for file in ["package-lock.json", "npm-shrinkwrap.json", ".npmrc"] {
                record(self.root, &mut files, &prefixed(&path, file), false)?;
            }
            let manifest = self.manifest(&path)?;
            for field in DEPENDENCY_FIELDS {
                for spec in dependency_table(&manifest, field)?.values() {
                    local_dependency(
                        self.root,
                        &path,
                        text(spec, "dependency spec")?,
                        &mut queue,
                        &mut files,
                    )?;
                }
            }
        }
        let directories: Vec<_> = directories.into_iter().collect();
        let digest = digest(serde_json::to_vec(&json!({
            "domain": "devkit-node-inputs-v1", "files": files, "directories": directories
        }))?);
        Ok(InputIdentitySet {
            schema_version: 1,
            files,
            directories,
            digest,
        })
    }
    fn closure_with_structure(
        &mut self,
        state: &ProjectState,
        structure: &NativeStructure,
    ) -> Result<InputIdentitySet> {
        let node = self.node_closure(state)?;
        let mut files = structure.manifests.clone();
        files.extend(node.files.clone());
        let directories = node.directories.clone();
        self.node_inputs = Some(node);
        let rust_queue = state
            .components
            .iter()
            .filter(|(_, component)| component.toolchain == Toolchain::Rust)
            .map(|(path, _)| path.clone())
            .collect();
        self.cargo_local_context(rust_queue, state, &mut files)?;
        // Bind workspace parent manifests read to resolve inherited dependencies.
        for path in self.cargo.keys() {
            record(self.root, &mut files, &prefixed(path, "Cargo.toml"), true)?;
        }
        // Parsed data and raw identities must describe the same input stage.
        // In particular, do not pair a cached old manifest with a newly read
        // identity if a user edits it during admission.
        for (name, expected) in &self.read_identities {
            ensure!(
                files.get(name) == Some(&Some(expected.clone()))
                    && identity(&input_path(self.root, name)?)? == Some(expected.clone()),
                "Native input changed during admission: {name}"
            );
        }
        for (name, expected) in &structure.manifests {
            ensure!(
                files.get(name) == Some(expected)
                    && &identity(&input_path(self.root, name)?)? == expected,
                "Native structural input changed during admission: {name}"
            );
        }
        structure.recheck(self.root)?;
        let digest = digest(serde_json::to_vec(
            &json!({"domain":"devkit-native-inputs-v3-structural", "files":files, "directories":directories, "namespaces":structure.namespaces}),
        )?);
        Ok(InputIdentitySet {
            schema_version: 1,
            files,
            directories,
            digest,
        })
    }
}

fn prefixed(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}
fn cargo_local_path(root: &Path, base: &str, value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && !value.contains('\0'),
        "Invalid Cargo local path"
    );
    let root = project_root(root)?;
    let manifest = input_path(&root, Path::new(base).join(value).join("Cargo.toml"))?;
    ensure!(
        manifest.parent() != Some(root.as_path()),
        "Unsupported checkout-root local package location: registered package sources must live below the checkout (for example apps/ or packages/)"
    );
    relative_name(
        &root,
        manifest.parent().context("Cargo local manifest parent")?,
    )
}
fn reject_local_cargo_git(dependency: &Fields) -> Result<()> {
    ensure!(
        !dependency
            .get("git")
            .and_then(Value::as_str)
            .is_some_and(|v| v.starts_with("file:")),
        "Unsupported local Cargo git source: use a confined path dependency or patch"
    );
    Ok(())
}
fn parent_name(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(parent, _)| parent.to_owned())
}
fn record(
    root: &Path,
    files: &mut BTreeMap<String, Option<crate::model::FileIdentity>>,
    name: &str,
    required: bool,
) -> Result<()> {
    let path = input_path(root, name)?;
    let value = identity(&path)?;
    ensure!(!required || value.is_some(), "Missing native input: {name}");
    files.insert(relative_name(&project_root(root)?, &path)?, value);
    Ok(())
}
fn local_dependency(
    root: &Path,
    base: &str,
    spec: &str,
    queue: &mut VecDeque<String>,
    files: &mut BTreeMap<String, Option<crate::model::FileIdentity>>,
) -> Result<()> {
    let Some(spec) = spec.strip_prefix("file:") else {
        return Ok(());
    };
    let name = local_path(root, base, spec)?;
    let target = input_path(root, &name)?;
    ensure!(exists(&target)?, "Missing local dependency: {name}");
    if fs::symlink_metadata(&target)?.is_dir() {
        queue.push_back(name);
    } else if name.ends_with(".tgz") || name.ends_with(".tar.gz") {
        record(root, files, &name, true)?;
    } else {
        bail!("Unsupported local dependency: {name}");
    }
    Ok(())
}
fn register_edge(
    state: &ProjectState,
    from: &str,
    target: String,
    deps: &mut BTreeSet<String>,
) -> Result<()> {
    if let Some(component) = state.components.get(&target) {
        ensure!(
            component.role == Role::Library,
            "Application dependency is forbidden: {from} -> {target}"
        );
        if target != from {
            deps.insert(target);
        }
    }
    Ok(())
}
fn component_owner<'a>(state: &'a ProjectState, path: &str) -> Option<&'a str> {
    state
        .components
        .keys()
        .find(|component| path == component.as_str() || path.starts_with(&format!("{component}/")))
        .map(String::as_str)
}
fn assert_acyclic(deps: &BTreeMap<String, Vec<String>>) -> Result<()> {
    fn visit(
        path: &str,
        deps: &BTreeMap<String, Vec<String>>,
        active: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<()> {
        ensure!(!active.contains(path), "Component dependency cycle: {path}");
        if done.contains(path) {
            return Ok(());
        }
        active.insert(path.to_owned());
        if let Some(edges) = deps.get(path) {
            for edge in edges {
                visit(edge, deps, active, done)?;
            }
        }
        active.remove(path);
        done.insert(path.to_owned());
        Ok(())
    }
    let (mut active, mut done) = (BTreeSet::new(), BTreeSet::new());
    for path in deps.keys() {
        visit(path, deps, &mut active, &mut done)?;
    }
    Ok(())
}

pub fn derive_component_dependencies(
    root: &Path,
    state: &ProjectState,
) -> Result<BTreeMap<String, Vec<String>>> {
    Ok(ReadContext::new(root).derive(state)?.projected)
}
pub fn collect_native_input_closure(root: &Path, state: &ProjectState) -> Result<InputIdentitySet> {
    ReadContext::new(root).closure(state)
}
pub fn native_input_digest(root: &Path, state: &ProjectState) -> Result<String> {
    Ok(collect_native_input_closure(root, state)?.digest)
}

pub fn assert_component_manifests(root: &Path, state: &ProjectState) -> Result<()> {
    for (path, component) in &state.components {
        let node = identity(&input_path(root, format!("{path}/package.json"))?)?.is_some();
        let rust = identity(&input_path(root, format!("{path}/Cargo.toml"))?)?.is_some();
        ensure!(!(node && rust), "Ambiguous native manifests: {path}");
        ensure!(
            (component.toolchain == Toolchain::Node && node)
                || (component.toolchain == Toolchain::Rust && rust),
            "Missing expected native {} manifest: {path}",
            component.toolchain
        );
    }
    Ok(())
}
pub fn assert_tool_profiles(root: &Path, state: &ProjectState) -> Result<()> {
    let value: Value = read_json(&safe_path(root, ".devkit/tooling-profiles.json")?)?;
    let payload = object(&value, "accepted tooling profiles")?;
    ensure!(
        payload.get("schemaVersion") == Some(&json!(1))
            && payload.get("version") == Some(&json!(state.version))
            && payload.get("packageMode") == Some(&json!(state.package_mode)),
        "Accepted tooling profile metadata differs from project"
    );
    let profiles = object(
        payload.get("profiles").context("Missing profiles")?,
        "tooling profiles",
    )?;
    for (path, component) in &state.components {
        let profile = profiles
            .get(component.tooling_profile.as_str())
            .context("Missing tooling profile")?;
        ensure!(
            profile == &serde_json::to_value(&component.tool_dependencies)?,
            "Accepted tooling profile differs from component: {path}"
        );
    }
    Ok(())
}

pub struct NativeAdmission {
    pub inputs: InputIdentitySet,
    /// Exact Node subset produced by the same parsed context, before Cargo and
    /// namespace identities are combined into the full safety snapshot.
    pub node_inputs: InputIdentitySet,
    pub dependencies: BTreeMap<String, Vec<String>>,
    /// Complete admitted graph. Public 0.2 projections retain their original
    /// literal-path shape, while role/cycle safety always checks this graph.
    pub effective_dependencies: BTreeMap<String, Vec<String>>,
    structure: NativeStructure,
}
impl NativeAdmission {
    pub fn recheck(&self, root: &Path) -> Result<()> {
        assert_input_identities(root, &self.inputs)?;
        self.structure.recheck(root)
    }
}
pub fn assert_input_identities(root: &Path, inputs: &InputIdentitySet) -> Result<()> {
    ensure!(
        inputs.schema_version == 1,
        "Unsupported native input identity schema"
    );
    for (name, expected) in &inputs.files {
        ensure!(
            &identity(&input_path(root, name)?)? == expected,
            "Native input changed during admission: {name}"
        );
    }
    Ok(())
}
pub fn assert_native_components(root: &Path, state: &ProjectState) -> Result<()> {
    admit_native_components(root, state).map(|_| ())
}
pub fn admit_native_components(root: &Path, state: &ProjectState) -> Result<NativeAdmission> {
    let structure = NativeStructure::capture(root, state)?;
    structure.assert_registered(state)?;
    assert_tool_profiles(root, state)?;
    let mut context = ReadContext::new(root);
    let nodes = node_components(state);
    if !nodes.is_empty() {
        let manifest = context.manifest("")?;
        let expected = json!(nodes.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>());
        ensure!(
            manifest.get("workspaces") == Some(&expected),
            "Root npm workspace membership differs from registered components"
        );
        let packages = context.packages()?;
        let locked = packages.get("").context("Native lock omits root package")?;
        for field in DEPENDENCY_FIELDS {
            ensure!(
                dependency_table(&manifest, field)? == dependency_table(locked, field)?,
                "Root native lock is stale: {field}"
            );
        }
        for field in ["name", "version"] {
            ensure!(
                manifest.get(field) == locked.get(field),
                "Native package identity projection is stale: root"
            );
        }
    }
    for (path, component) in &state.components {
        if component.toolchain == Toolchain::Rust {
            for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
                ensure!(
                    identity(&input_path(root, format!("{path}/{name}"))?)?.is_some(),
                    "Missing Rust native input: {path}/{name}"
                );
            }
            continue;
        }
        let manifest = context.manifest(path)?;
        let tools = dependency_table(&manifest, "devDependencies")?;
        let packages = context.packages()?;
        let locked = packages
            .get(path)
            .with_context(|| format!("Native lock omits workspace: {path}"))?;
        for field in ["name", "version"] {
            ensure!(
                manifest.get(field) == locked.get(field),
                "Native package identity projection is stale: {path}"
            );
        }
        let name = text(
            manifest
                .get("name")
                .context("Missing workspace package name")?,
            "workspace package name",
        )?;
        let link = format!("node_modules/{name}");
        ensure!(
            packages.get(&link).and_then(|p| p.get("link")) == Some(&Value::Bool(true))
                && resolve_lock_node(packages, &link)? == *path,
            "Native package identity projection is stale: {path}"
        );
        for (location, value) in packages {
            if root_workspace_link(location)
                && value.get("link") == Some(&Value::Bool(true))
                && resolve_lock_node(packages, location)? == *path
            {
                ensure!(
                    location == &link,
                    "Native package identity projection is stale: {path}"
                );
            }
        }
        for field in DEPENDENCY_FIELDS {
            ensure!(
                dependency_table(&manifest, field)? == dependency_table(locked, field)?,
                "Native lock is stale: {path}:{field}"
            );
        }
        for (name, spec) in &component.tool_dependencies {
            ensure!(
                tools.get(name) == Some(&Value::String(spec.clone())),
                "Tool dependency conflict: {path}/package.json devDependencies.{name}"
            );
        }
        let scripts = object(
            manifest.get("scripts").context("Missing native scripts")?,
            "native scripts",
        )?;
        for command in ["check", "fix", "test", "build"]
            .into_iter()
            .chain((component.role == Role::App).then_some("dev"))
        {
            text(
                scripts
                    .get(command)
                    .with_context(|| format!("Missing native script {path}:{command}"))?,
                &format!("native script {path}:{command}"),
            )?;
        }
        ensure!(
            component.role != Role::Library || manifest.contains_key("exports"),
            "Library must define public exports: {path}"
        );
    }
    let dependencies = context.derive(state)?;
    for (path, actual) in &dependencies.projected {
        ensure!(
            actual == &state.components[path].dependencies,
            "Native dependency projection is stale: {path}; run the accepted devkit update"
        );
    }
    if !nodes.is_empty() {
        context.assert_npm_targets()?;
    }
    let inputs = context.closure_with_structure(state, &structure)?;
    assert_npmrc_compatibility(root, &inputs.directories)?;
    assert_input_identities(root, &inputs)?;
    let node_inputs = context
        .node_inputs
        .take()
        .context("Missing Node input snapshot")?;
    Ok(NativeAdmission {
        inputs,
        node_inputs,
        dependencies: dependencies.projected,
        effective_dependencies: dependencies.effective,
        structure,
    })
}

pub fn assert_npmrc_compatibility(root: &Path, directories: &[String]) -> Result<()> {
    fn read(root: &Path, path: &Path) -> Result<BTreeMap<String, String>> {
        if identity(path)?.is_none() {
            return Ok(BTreeMap::new());
        }
        let mut result = BTreeMap::new();
        for line in fs::read_to_string(path)?.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .with_context(|| format!("Unprovable npm configuration: {}", path.display()))?;
            let key = key.trim();
            ensure!(
                !key.is_empty()
                    && !key.chars().any(char::is_whitespace)
                    && !line.contains(['$', '[', ']']),
                "Unprovable npm configuration: {}",
                relative_name(&project_root(root)?, path)?
            );
            result.insert(key.to_owned(), value.trim().to_owned());
        }
        Ok(result)
    }
    let base = read(root, &input_path(root, ".npmrc")?)?;
    let defaults = BTreeMap::from([
        ("install-links", "false"),
        ("strict-peer-deps", "true"),
        ("legacy-peer-deps", "false"),
        ("force", "false"),
        ("fund", "false"),
        ("audit", "false"),
        ("update-notifier", "false"),
        ("install-strategy", "hoisted"),
    ]);
    for directory in directories.iter().filter(|d| !d.is_empty()) {
        for (key, value) in read(root, &input_path(root, format!("{directory}/.npmrc"))?)? {
            let accepted = base
                .get(&key)
                .map(String::as_str)
                .or_else(|| defaults.get(key.as_str()).copied());
            ensure!(
                Some(value.as_str()) == accepted,
                "npm configuration changes in workspace context: {directory}/.npmrc ({key})"
            );
        }
    }
    Ok(())
}
