//! Translate accepted component workspaces in the transaction's private stage.
//! Cargo's selected package graph comes from the old locks, never a new solve.
use crate::{
    Result,
    cargo_lock::{CargoLockResolver, Lock, matches_release_recipe_graph, merge_legacy_graphs},
    files::{
        exists, identity, input_path, read_input_bytes, read_regular_file, relative_name,
        remove_file, write_atomic,
    },
    model::{CargoResolution, ProjectState},
    native::{
        CargoPackageId, CargoReplacementSpec, CargoWorkspaceModel, cargo_patch_origin,
        cargo_registries, legacy_cargo_inputs,
    },
    release::AdmittedRelease,
};
use anyhow::{Context, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value, value};

const ROOT_FILES: [&str; 4] = [
    "Cargo.toml",
    "Cargo.lock",
    "rustfmt.toml",
    "rust-toolchain.toml",
];

pub(crate) fn check_destinations(root: &Path) -> Result<()> {
    for name in ROOT_FILES {
        ensure!(
            !exists(&input_path(root, name)?)?,
            "Cargo migration destination already exists: {name}; move it aside before updating"
        );
    }
    Ok(())
}

fn read_manifest(root: &Path, dir: &str) -> Result<DocumentMut> {
    let path = input_path(root, format!("{dir}/Cargo.toml"))?;
    std::str::from_utf8(&read_input_bytes(&path)?)?
        .parse()
        .with_context(|| format!("Invalid Cargo manifest: {}", path.display()))
}

fn rebased(root: &Path, component: &str, path: &str) -> Result<String> {
    ensure!(
        !path.is_empty() && !Path::new(path).is_absolute(),
        "Expected a relative path in {component}/Cargo.toml: {path}"
    );
    relative_name(root, &input_path(root, Path::new(component).join(path))?)
}

fn replace_string(item: &mut Item, new: String) -> Result<()> {
    let old = item
        .as_value_mut()
        .context("Expected a Cargo manifest string")?;
    let mut next = Value::from(new);
    *next.decor_mut() = old.decor().clone();
    *old = next;
    Ok(())
}

fn rebase_field(root: &Path, dir: &str, field: &mut Item) -> Result<()> {
    let path = rebased(root, dir, field.as_str().context("Expected a Cargo path")?)?;
    replace_string(field, path)
}

fn rebase_declarations(root: &Path, dir: &str, declarations: &mut Item) -> Result<()> {
    for (_, declaration) in declarations
        .as_table_like_mut()
        .context("Expected Cargo dependency table")?
        .iter_mut()
    {
        if let Some(path) = declaration
            .as_table_like_mut()
            .and_then(|t| t.get_mut("path"))
        {
            rebase_field(root, dir, path)?;
        }
    }
    Ok(())
}

fn semantic(item: &Item) -> Result<toml::Value> {
    let mut doc = DocumentMut::new();
    doc.insert("value", item.clone());
    toml::from_str::<toml::Table>(&doc.to_string())?
        .remove("value")
        .context("Missing Cargo setting")
}

fn editable_table(item: &mut Item) -> Option<&mut Table> {
    if item.is_inline_table() {
        *item = Item::Table(std::mem::take(item).into_table().expect("inline table"));
    }
    item.as_table_mut()
}

// Merge named declarations (e.g. dependencies) at their key boundaries.
// A second definition of the same dependency must match as a complete value.
fn merge_setting(
    target: &mut Table,
    key: &str,
    source: &dyn TableLike,
    depth: usize,
    context: &str,
) -> Result<()> {
    let Some((source_key, incoming)) = source.get_key_value(key) else {
        return Ok(());
    };
    let Some(previous) = target.get_mut(key) else {
        target.insert_formatted(source_key, incoming.clone());
        return Ok(());
    };
    if depth > 0 {
        let destination = editable_table(previous)
            .with_context(|| format!("Expected [{key}] table in {context}"))?;
        let additions = incoming
            .as_table_like()
            .with_context(|| format!("Expected [{key}] table in {context}"))?;
        for (child, _) in additions.iter() {
            merge_setting(destination, child, additions, depth - 1, context)?;
        }
    } else {
        ensure!(
            semantic(previous)? == semantic(incoming)?,
            "Conflicting Cargo setting {key} in {context}; align it before updating"
        );
    }
    Ok(())
}

fn resolver(doc: &DocumentMut) -> Result<&str> {
    if let Some(item) = doc
        .get("workspace")
        .and_then(|w| w.get("resolver"))
        .or_else(|| doc.get("package").and_then(|p| p.get("resolver")))
    {
        return item
            .as_str()
            .context("Expected Cargo resolver version string");
    }
    let edition = doc.get("package").and_then(|p| p.get("edition"));
    let edition = match edition {
        Some(item) if item.get("workspace").and_then(Item::as_bool) == Some(true) => doc
            .get("workspace")
            .and_then(|w| w.get("package"))
            .and_then(|p| p.get("edition"))
            .and_then(Item::as_str),
        item => item.and_then(Item::as_str),
    };
    Ok(match edition {
        Some("2024") => "3",
        Some("2021") => "2",
        _ => "1",
    })
}

fn manifest_paths(root: &Path, dir: &str, item: Option<&Item>) -> Result<BTreeSet<String>> {
    let Some(item) = item else {
        return Ok(BTreeSet::new());
    };
    item.as_array()
        .context("Expected a Cargo workspace path array")?
        .iter()
        .map(|value| {
            let path = value
                .as_str()
                .context("Expected literal Cargo workspace path")?;
            ensure!(
                !path.contains(['*', '?', '[', ']']),
                "List exact workspace paths in {dir}/Cargo.toml: {path}"
            );
            rebased(root, dir, path)
        })
        .collect()
}

struct LegacyWorkspace {
    dir: String,
    manifest: DocumentMut,
    lock: Lock,
    members: BTreeSet<String>,
    fmt: Vec<u8>,
}

impl LegacyWorkspace {
    fn read(root: &Path, dir: String, members: BTreeSet<String>) -> Result<Self> {
        let manifest = read_manifest(root, &dir)?;
        let lock_path = format!("{dir}/Cargo.lock");
        let lock = Lock::parse(&read_input_bytes(&input_path(root, &lock_path)?)?)
            .with_context(|| format!("Invalid {lock_path}"))?;
        let fmt = read_input_bytes(&input_path(root, format!("{dir}/rustfmt.toml"))?)?;
        Ok(Self {
            dir,
            manifest,
            lock,
            members,
            fmt,
        })
    }
    fn path(&self) -> String {
        format!("{}/Cargo.toml", self.dir)
    }
}

fn check_patch_aliases(
    manifest: &DocumentMut,
    registries: &BTreeMap<String, String>,
) -> Result<()> {
    let Some(patches) = manifest.get("patch") else {
        return Ok(());
    };
    let mut by_origin = BTreeMap::new();
    for (source, rows) in patches
        .as_table_like()
        .context("Expected Cargo patch sources")?
        .iter()
    {
        let origin = cargo_patch_origin(source, registries)?;
        if let Some((previous, previous_rows)) = by_origin.insert(origin, (source, rows)) {
            // Cargo can shadow one of two tables for the same source within
            // a single manifest. Unioning them could activate a hidden patch.
            ensure!(
                semantic(previous_rows)? == semantic(rows)?,
                "Cargo patch sources {previous} and {source} refer to the same origin; combine their declarations and verify the lock on 0.5.x"
            );
        }
    }
    Ok(())
}

fn replacements(manifest: &DocumentMut) -> Result<BTreeMap<&str, CargoReplacementSpec>> {
    let Some(table) = manifest.get("replace") else {
        return Ok(BTreeMap::new());
    };
    table
        .as_table_like()
        .context("Expected Cargo replacements")?
        .iter()
        .map(|(key, _)| Ok((key, CargoReplacementSpec::parse(key)?)))
        .collect()
}

fn check_replacement_capture(workspaces: &[LegacyWorkspace]) -> Result<()> {
    let declarations = workspaces
        .iter()
        .map(|w| replacements(&w.manifest).with_context(|| w.path()))
        .collect::<Result<Vec<_>>>()?;
    for (owner, rules) in workspaces.iter().zip(&declarations) {
        for (key, spec) in rules {
            for (peer, existing) in workspaces.iter().zip(&declarations) {
                if existing.get(key) == Some(spec) {
                    continue;
                }
                for id in peer.lock.packages.keys() {
                    ensure!(
                        !spec.applies_to(id)?,
                        "Cargo replacement {} in {} would also apply to {}/Cargo.lock; align replacements on 0.5.x",
                        key,
                        owner.path(),
                        peer.dir
                    );
                }
            }
        }
    }
    Ok(())
}

fn check_common_settings(workspaces: &[LegacyWorkspace]) -> Result<()> {
    let first = &workspaces[0];
    let first_fmt: toml::Table = toml::from_str(std::str::from_utf8(&first.fmt)?)?;
    let first_profile = first.manifest.get("profile").map(semantic).transpose()?;
    for next in &workspaces[1..] {
        let fmt: toml::Table = toml::from_str(std::str::from_utf8(&next.fmt)?)?;
        ensure!(
            fmt == first_fmt,
            "Conflicting formatter settings: {}/rustfmt.toml and {}/rustfmt.toml",
            first.dir,
            next.dir
        );
        ensure!(
            first_profile == next.manifest.get("profile").map(semantic).transpose()?,
            "Conflicting or asymmetric Cargo profiles: {} and {}",
            first.path(),
            next.path()
        );
        ensure!(
            resolver(&first.manifest)? == resolver(&next.manifest)?,
            "Conflicting Cargo resolver versions: {} and {}",
            first.path(),
            next.path()
        );
    }
    Ok(())
}

fn check_component_configs(root: &Path, dir: &str) -> Result<()> {
    let mut current = Some(Path::new(dir));
    while let Some(path) = current.filter(|p| !p.as_os_str().is_empty()) {
        for name in [".cargo/config", ".cargo/config.toml"] {
            let config = path.join(name);
            ensure!(
                !exists(&input_path(root, &config)?)?,
                "Move {} to the root .cargo/config.toml and rebase its relative paths before updating",
                config.display()
            );
        }
        current = path.parent();
    }
    Ok(())
}

fn rebase_package_workspace(
    root: &Path,
    owner: &str,
    dir: &str,
    package: &mut Item,
) -> Result<bool> {
    let Some(pointer) = package
        .as_table_like_mut()
        .and_then(|t| t.get_mut("workspace"))
    else {
        return Ok(false);
    };
    let old = pointer
        .as_str()
        .context("Expected package.workspace path")?;
    ensure!(
        rebased(root, dir, old)? == owner,
        "Unexpected package.workspace in {dir}/Cargo.toml; expected {owner}/Cargo.toml"
    );
    let relative = vec![".."; Path::new(dir).components().count()].join("/");
    replace_string(pointer, relative)?;
    Ok(true)
}

fn save_manifest(root: &Path, dir: &str, doc: &DocumentMut) -> Result<()> {
    let path = input_path(root, format!("{dir}/Cargo.toml"))?;
    let mode = identity(&path)?.context("Missing staged manifest")?.mode;
    write_atomic(&path, doc.to_string().as_bytes(), mode)
}

fn update_member(
    root: &Path,
    owner: &str,
    member: &str,
    doc: &mut DocumentMut,
    names: &mut BTreeMap<String, String>,
) -> Result<bool> {
    ensure!(
        Path::new(member).starts_with(owner),
        "Cargo member {member}/Cargo.toml is outside its component {owner}"
    );
    let package = doc
        .get_mut("package")
        .context("Cargo member has no package")?;
    let name = package
        .get("name")
        .and_then(Item::as_str)
        .context("Missing Cargo package name")?;
    if let Some(previous) = names.insert(name.to_owned(), member.to_owned()) {
        bail!(
            "Duplicate Cargo package name {name}: {previous}/Cargo.toml and {member}/Cargo.toml. On 0.5.x, choose a unique package name in one manifest, update Rust crate references, run make exec app=<name> write=1 network=bridge command='cargo update --workspace', then make check and repeat the update preview."
        );
    }
    rebase_package_workspace(root, owner, member, package)
}

// Rebase each original array element in place so comments stay attached to
// the paths they describe. Implicit members and external helpers are appended.
fn write_workspace_paths(
    root: &Path,
    table: &mut Table,
    workspaces: &[LegacyWorkspace],
    key: &str,
    paths: &BTreeSet<String>,
    required: bool,
) -> Result<()> {
    let mut remaining = paths.clone();
    let mut output: Option<(toml_edit::Key, Array)> = None;
    for workspace in workspaces {
        let Some((original_key, item)) = workspace
            .manifest
            .get("workspace")
            .and_then(Item::as_table_like)
            .and_then(|t| t.get_key_value(key))
        else {
            continue;
        };
        let array = item
            .as_array()
            .context("Expected Cargo workspace path list")?;
        let (_, result) = output.get_or_insert_with(|| {
            let mut result = array.clone();
            result.clear();
            (original_key.clone(), result)
        });
        for entry in array.iter() {
            let path = rebased(
                root,
                &workspace.dir,
                entry.as_str().context("Expected a workspace path")?,
            )?;
            if remaining.remove(&path) {
                let mut value = Value::from(path);
                *value.decor_mut() = entry.decor().clone();
                result.push_formatted(value);
            }
        }
    }
    if required || output.is_some() {
        let (original_key, mut array) =
            output.unwrap_or_else(|| (toml_edit::Key::new(key), Array::new()));
        for path in remaining {
            array.push(path);
        }
        table.insert_formatted(&original_key, value(array));
    }
    Ok(())
}

fn merge_manifest(
    root: &Path,
    source: &mut LegacyWorkspace,
    output: &mut DocumentMut,
    context: &str,
) -> Result<()> {
    let component = &source.dir;
    let manifest = &mut source.manifest;
    if let Some(workspace) = manifest.get_mut("workspace") {
        let workspace = workspace
            .as_table_like_mut()
            .context("Expected [workspace] table")?;
        if let Some(deps) = workspace.get_mut("dependencies") {
            rebase_declarations(root, component, deps)?;
        }
        if let Some(package) = workspace.get_mut("package") {
            for key in ["license-file", "readme"] {
                if let Some(field) = package.as_table_like_mut().and_then(|p| p.get_mut(key)) {
                    if field.is_str() {
                        rebase_field(root, component, field)?;
                    } else if key == "readme" && field.as_bool() == Some(true) {
                        // Cargo resolves true to README.md at the defining workspace,
                        // even when another README filename exists in that directory.
                        replace_string(field, rebased(root, component, "README.md")?)?;
                    }
                }
            }
        }
        for key in ["members", "exclude", "default-members", "resolver"] {
            workspace.remove(key);
        }
        let target = output["workspace"]
            .as_table_mut()
            .context("Missing root workspace")?;
        for (key, _) in workspace.iter() {
            let depth = match key {
                "package" | "dependencies" | "metadata" => 1,
                // Every inheriting member receives the whole workspace.lints
                // policy. Unioning distinct lint names can change build results.
                _ => 0,
            };
            merge_setting(target, key, workspace, depth, context)?;
        }
    }
    if let Some(patch) = manifest.get_mut("patch") {
        for (_, rows) in patch
            .as_table_like_mut()
            .context("Expected [patch] table")?
            .iter_mut()
        {
            rebase_declarations(root, component, rows)?;
        }
    }
    if let Some(replace) = manifest.get_mut("replace") {
        rebase_declarations(root, component, replace)?;
    }
    for (key, depth) in [("patch", 2), ("replace", 1), ("profile", 0)] {
        merge_setting(
            output.as_table_mut(),
            key,
            manifest.as_table(),
            depth,
            context,
        )?;
        manifest.remove(key);
    }
    manifest.remove("workspace");
    if let Some(package) = manifest
        .get_mut("package")
        .and_then(Item::as_table_like_mut)
    {
        package.remove("resolver");
    }
    Ok(())
}

// Cargo looks up a single patch table per canonical source. Use the first
// original spelling, merging declarations only after their paths are rebased.
fn coalesce_patch_origins(
    manifest: &mut DocumentMut,
    registries: &BTreeMap<String, String>,
    context: &str,
) -> Result<()> {
    let Some(patch) = manifest.get_mut("patch") else {
        return Ok(());
    };
    let mut origins = BTreeMap::new();
    let mut aliases = Vec::new();
    for (raw, _) in patch
        .as_table_like()
        .context("Expected [patch] table")?
        .iter()
    {
        let origin = cargo_patch_origin(raw, registries)?;
        let first = origins.entry(origin).or_insert_with(|| raw.to_owned());
        if first != raw {
            aliases.push((raw.to_owned(), first.clone()));
        }
    }
    if aliases.is_empty() {
        return Ok(());
    }
    let patches = editable_table(patch).context("Expected [patch] table")?;
    for (alias, first) in aliases {
        let additional = patches.remove(&alias).expect("existing patch source");
        let additional = additional
            .as_table_like()
            .context("Expected patch declarations")?;
        let target = patches.get_mut(&first).expect("existing patch source");
        let target = editable_table(target).context("Expected patch declarations")?;
        for (key, _) in additional.iter() {
            merge_setting(target, key, additional, 0, context).with_context(|| {
                format!("While merging Cargo patch sources {first} and {alias}")
            })?;
        }
    }
    Ok(())
}

// The validated lock covers every direct selection. For an initial proof,
// also rule out features, links and alternative candidates that could change
// the composition when two formerly separate workspaces resolve together.
fn plain_declarations(table: Option<&Item>) -> bool {
    table.is_none_or(|table| {
        table.as_table_like().is_some_and(|rows| {
            rows.iter().all(|(_, spec)| {
                spec.is_str()
                    || spec.as_table_like().is_some_and(|spec| {
                        spec.iter().all(|(key, _)| {
                            matches!(
                                key,
                                "version" | "path" | "workspace" | "package" | "registry"
                            )
                        })
                    })
            })
        })
    })
}

fn local_import_proven(
    root: &Path,
    model: &CargoWorkspaceModel,
    graph: &Lock,
    workspace: &Item,
) -> Result<bool> {
    if !model.replacements.is_empty()
        || model
            .patches
            .iter()
            .any(|patch| patch.target.path.is_none())
        || graph.packages.keys().any(|id| id.source.is_some())
    {
        return Ok(false);
    }
    if model.simple_local_graph {
        return Ok(true);
    }
    // Also admit plain registry declarations pinned to a local patch and
    // inherited path declarations. Limit this extra case to dependency leaves.
    if !plain_declarations(workspace.get("dependencies"))
        || graph.packages.values().any(|record| {
            record
                .dependencies
                .iter()
                .any(|id| !graph.packages[id].dependencies.is_empty())
        })
    {
        return Ok(false);
    }
    let mut targets = BTreeSet::new();
    if model
        .patches
        .iter()
        .any(|p| !targets.insert((&p.origin, &p.target.name)))
    {
        return Ok(false);
    }
    for config in [".cargo/config", ".cargo/config.toml"] {
        let path = input_path(root, config)?;
        if exists(&path)? {
            let doc: DocumentMut = std::str::from_utf8(&read_input_bytes(&path)?)?.parse()?;
            if doc.iter().any(|(key, _)| key != "registries") {
                return Ok(false);
            }
        }
    }
    for member in model.local_packages.values() {
        let doc = read_manifest(root, &member.rel_dir)?;
        if doc.get("package").is_some_and(|p| p.get("links").is_some())
            || [
                "features",
                "cargo-features",
                "target",
                "build-dependencies",
                "build_dependencies",
                "dev-dependencies",
                "dev_dependencies",
            ]
            .iter()
            .any(|key| doc.contains_key(key))
            || !plain_declarations(doc.get("dependencies"))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn merged_local_sources(
    selected: BTreeSet<(CargoPackageId, String)>,
) -> Result<BTreeMap<CargoPackageId, String>> {
    let mut merged = BTreeMap::new();
    for (id, dir) in selected {
        if let Some(previous) = merged.get(&id) {
            bail!(
                "Component Cargo.lock files select different directories for {} {}: {previous}/Cargo.toml and {dir}/Cargo.toml; align the local sources on 0.5.x before updating",
                id.name,
                id.version
            );
        }
        merged.insert(id, dir);
    }
    Ok(merged)
}

pub(crate) fn migrate(
    root: &Path,
    old: &ProjectState,
    next: &ProjectState,
    release: &AdmittedRelease,
    lock_resolver: &dyn CargoLockResolver,
    offline: bool,
) -> Result<(BTreeSet<String>, CargoResolution)> {
    check_destinations(root)?;
    let legacy = legacy_cargo_inputs(root, old)?;
    let mut workspaces = legacy
        .groups
        .into_iter()
        .map(|(dir, members)| LegacyWorkspace::read(root, dir, members))
        .collect::<Result<Vec<_>>>()?;
    ensure!(!workspaces.is_empty(), "No Rust components to migrate");
    check_common_settings(&workspaces)?;
    let registries = cargo_registries(root, old)?;
    for w in &workspaces {
        check_patch_aliases(&w.manifest, &registries).with_context(|| w.path())?;
    }
    check_replacement_capture(&workspaces)?;

    let context = workspaces
        .iter()
        .map(LegacyWorkspace::path)
        .collect::<Vec<_>>()
        .join(", ");
    let component_dirs: BTreeSet<_> = workspaces.iter().map(|w| w.dir.clone()).collect();
    let members: BTreeSet<_> = workspaces
        .iter()
        .flat_map(|w| w.members.iter().cloned())
        .collect();
    let mut excludes = BTreeSet::new();
    for dir in legacy
        .paths
        .difference(&members)
        .filter(|p| !component_dirs.contains(*p))
    {
        ensure!(
            !old.components
                .keys()
                .any(|owner| Path::new(dir).starts_with(owner)),
            "Cargo path dependency {dir} is inside a registered component; declare it as a workspace member before updating"
        );
        excludes.insert(dir.clone());
    }
    let mut defaults = BTreeSet::new();
    for workspace in &workspaces {
        let settings = workspace.manifest.get("workspace");
        excludes.extend(manifest_paths(
            root,
            &workspace.dir,
            settings.and_then(|w| w.get("exclude")),
        )?);
        let own_defaults = settings.and_then(|w| w.get("default-members"));
        if own_defaults.is_some() {
            defaults.extend(manifest_paths(root, &workspace.dir, own_defaults)?);
        } else if workspace.manifest.contains_key("package") {
            defaults.insert(workspace.dir.clone());
        } else {
            defaults.extend(workspace.members.iter().cloned());
        }
    }
    ensure!(
        defaults.is_subset(&members),
        "Unknown Cargo default-members in {context}"
    );
    for exclude in &excludes {
        ensure!(
            !old.components
                .keys()
                .any(|owner| Path::new(exclude).starts_with(owner)
                    || Path::new(owner).starts_with(exclude)),
            "Cargo workspace.exclude path {exclude} intersects a registered component in {context}"
        );
    }

    let mut table = workspaces
        .iter()
        .find_map(|w| w.manifest.get("workspace").and_then(Item::as_table))
        .cloned()
        .unwrap_or_default();
    table.clear();
    table.set_implicit(false);
    if let Some((key, item)) = workspaces.iter().find_map(|w| {
        w.manifest
            .get("workspace")
            .and_then(Item::as_table_like)
            .and_then(|t| t.get_key_value("resolver"))
    }) {
        table.insert_formatted(key, item.clone());
    } else {
        table.insert("resolver", value(resolver(&workspaces[0].manifest)?));
    }
    write_workspace_paths(root, &mut table, &workspaces, "members", &members, true)?;
    write_workspace_paths(
        root,
        &mut table,
        &workspaces,
        "default-members",
        &defaults,
        defaults != members,
    )?;
    write_workspace_paths(
        root,
        &mut table,
        &workspaces,
        "exclude",
        &excludes,
        !excludes.is_empty(),
    )?;
    let mut output = DocumentMut::new();
    output.insert("workspace", Item::Table(table));
    let fmt = workspaces[0].fmt.clone();
    let mut names = BTreeMap::new();
    let mut locks = Vec::new();
    let mut paths = ROOT_FILES
        .iter()
        .map(|p| p.to_string())
        .collect::<BTreeSet<_>>();
    for workspace in &mut workspaces {
        check_component_configs(root, &workspace.dir)?;
        for member in &workspace.members {
            check_component_configs(root, member)?;
            if member == &workspace.dir {
                update_member(
                    root,
                    &workspace.dir,
                    member,
                    &mut workspace.manifest,
                    &mut names,
                )?;
            } else {
                let mut doc = read_manifest(root, member)?;
                if update_member(root, &workspace.dir, member, &mut doc, &mut names)? {
                    save_manifest(root, member, &doc)?;
                    paths.insert(format!("{member}/Cargo.toml"));
                }
            }
        }
        merge_manifest(root, workspace, &mut output, &context)?;
        let name = workspace.path();
        if workspace.manifest.contains_key("package") {
            save_manifest(root, &workspace.dir, &workspace.manifest)?;
        } else {
            ensure!(
                workspace.manifest.is_empty(),
                "Cannot remove virtual manifest {name} with unmigrated settings; review it before updating"
            );
            remove_file(&input_path(root, &name)?)?;
        }
        paths.insert(name);
        locks.push((
            format!("{}/Cargo.lock", workspace.dir),
            std::mem::take(&mut workspace.lock),
        ));
        for name in ["Cargo.lock", "rust-toolchain.toml", "rustfmt.toml"] {
            let path = format!("{}/{name}", workspace.dir);
            remove_file(&input_path(root, &path)?)?;
            paths.insert(path);
        }
    }
    ensure!(
        !(output.contains_key("patch") && output.contains_key("replace")),
        "Cargo cannot combine [patch] and [replace] from {context}; align the overrides before updating"
    );
    coalesce_patch_origins(&mut output, &registries, &context)?;
    write_atomic(
        &input_path(root, "Cargo.toml")?,
        output.to_string().as_bytes(),
        0o644,
    )?;
    let selected_sources = merged_local_sources(legacy.selected)?;
    let mut graph = merge_legacy_graphs(&locks)?;
    let lock_path = input_path(root, "Cargo.lock")?;
    // Local Cargo.lock IDs omit their directory. Classify each merged patch
    // against the package actually selected in its old component workspace.
    let directories = selected_sources.values().cloned().collect();
    let mut model = CargoWorkspaceModel::for_resolution(root, next, &graph, Some(&directories))?;
    let newly_used = graph.bind_merged_patches(&model)?;
    let lock = graph.bytes()?;
    write_atomic(&lock_path, &lock, 0o644)?;
    // A formerly unused override may have path dependencies of its own.
    // Build their closure and request any witnesses against the normalized lock.
    if newly_used || model.resolution_inputs.local_selections.is_some() {
        model =
            crate::cargo_lock::resolved_model(root, next, &graph, release, lock_resolver, offline)?;
    }
    for (id, original) in &selected_sources {
        let selected = &model.local_package(id)?.rel_dir;
        ensure!(
            selected == original,
            "Merged Cargo.lock selects {selected}/Cargo.toml for {} {} instead of {original}/Cargo.toml",
            id.name,
            id.version
        );
    }
    // Patches add version candidates to a source. Check the preserved package
    // IDs and edges under the merged declarations; external selections with
    // patches also require Cargo's locked validation below.
    model.validate_lock(next, &graph).with_context(|| {
        format!("Merged Cargo.lock conflicts with the manifests from {context}")
    })?;
    let proven = model.local_workspace_policy
        && (local_import_proven(root, &model, &graph, &output["workspace"])?
            || matches_release_recipe_graph(&model, release, &graph)?);
    if !proven {
        lock_resolver.validate_locked(root, release, offline).with_context(|| {
            let sources = locks.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>().join(", ");
            format!("Pinned Cargo could not validate the merged Cargo.lock from {sources} against the root Cargo.toml with --locked. Check the original locks on 0.5.x and retry with the pinned image and a populated Cargo cache or network access")
        })?;
        let unchanged = read_regular_file(&lock_path, 32 * 1024 * 1024, "staged Cargo.lock")?
            .is_some_and(|(bytes, id)| bytes == lock && id.mode == 0o644);
        ensure!(
            unchanged,
            "Cargo validation changed the staged Cargo.lock for {context}; the selected locks must be preserved"
        );
    }
    write_atomic(&input_path(root, "rustfmt.toml")?, &fmt, 0o644)?;
    Ok((paths, model.resolution_inputs.proof(&lock)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(dir: &str, manifest: &str) -> LegacyWorkspace {
        LegacyWorkspace {
            dir: dir.to_owned(),
            manifest: manifest.parse().unwrap(),
            members: BTreeSet::new(),
            lock: Lock::default(),
            fmt: Vec::new(),
        }
    }

    #[test]
    fn patch_aliases_respect_named_registries_and_single_manifest_shadowing() -> Result<()> {
        let raw = "vendor-index";
        let url = "https://example.test/index";
        let registries = BTreeMap::from([(raw.to_owned(), url.to_owned())]);
        let inline = format!(
            "patch = {{ '{url}' = {{ one = {{ path='vendor/one' }} }}, {raw} = {{ two = {{path='vendor/two'}} }} }}"
        );
        let mut doc: DocumentMut = inline.parse()?;
        assert!(check_patch_aliases(&doc, &registries).is_err());
        coalesce_patch_origins(
            &mut doc,
            &registries,
            "apps/left/Cargo.toml, apps/right/Cargo.toml",
        )?;
        let output = doc.to_string().parse::<toml::Table>()?;
        let sources = output["patch"].as_table().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[url]["one"]["path"].as_str(), Some("vendor/one"));
        assert_eq!(sources[url]["two"]["path"].as_str(), Some("vendor/two"));
        Ok(())
    }

    #[test]
    fn moving_replacements_respects_the_original_source_and_version() -> Result<()> {
        let source = "registry+https://github.com/rust-lang/crates.io-index";
        let manifest = "[replace]\n'external:1.0.0' = { path='../../vendor/external' }\n";
        for (origin, version, captured) in [
            ("registry+https://registry.example.test", "1.0.0", false),
            (source, "2.0.0", false),
            (source, "1.0.0", true),
        ] {
            let mut workspaces = [legacy("apps/left", manifest), legacy("apps/right", "")];
            let id = crate::native::CargoPackageId {
                name: "external".into(),
                version: version.into(),
                source: Some(origin.into()),
            };
            workspaces[1].lock.packages.insert(id, Default::default());
            let result = check_replacement_capture(&workspaces);
            if captured {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains("apps/left/Cargo.toml")
                        && error.contains("apps/right/Cargo.lock"),
                    "{error}"
                );
            } else {
                result?;
            }
        }
        Ok(())
    }

    #[test]
    fn inherited_declarations_are_compared_after_rebasing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut first = legacy(
            "apps/left",
            "[workspace.dependencies]\nshared={path='../../vendor/shared'} # first definition\n",
        );
        let mut second = legacy(
            "apps/right",
            "[workspace.dependencies.shared]\npath='../../vendor/shared'\n",
        );
        let mut output: DocumentMut = "[workspace]\n".parse()?;
        merge_manifest(temp.path(), &mut first, &mut output, "apps/left/Cargo.toml")?;
        merge_manifest(
            temp.path(),
            &mut second,
            &mut output,
            "apps/left/Cargo.toml, apps/right/Cargo.toml",
        )?;
        assert_eq!(
            output["workspace"]["dependencies"]["shared"]["path"].as_str(),
            Some("vendor/shared")
        );
        assert!(output.to_string().contains("# first definition"));
        let mut different = legacy(
            "apps/third",
            "[workspace.dependencies]\nshared={path='vendor/shared'}\n",
        );
        let error = merge_manifest(
            temp.path(),
            &mut different,
            &mut output,
            "apps/left/Cargo.toml, apps/third/Cargo.toml",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("shared")
                && error.contains("apps/left/Cargo.toml")
                && error.contains("apps/third/Cargo.toml"),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn the_old_resolver_must_agree_across_workspaces() -> Result<()> {
        let implicit = "[workspace.package]\nedition='2021'\n[package]\nedition.workspace=true\n";
        let compatible = [
            legacy("apps/left", implicit),
            legacy("apps/right", "[workspace]\nresolver='2'\n"),
        ];
        check_common_settings(&compatible)?;
        let different = [
            legacy("apps/left", implicit),
            legacy("apps/right", "[package]\nedition='2024'\n"),
        ];
        let error = check_common_settings(&different).unwrap_err().to_string();
        assert!(
            error.contains("resolver")
                && error.contains("apps/left/Cargo.toml")
                && error.contains("apps/right/Cargo.toml"),
            "{error}"
        );
        Ok(())
    }
}
