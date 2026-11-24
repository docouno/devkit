//! Product-rooted graph preservation over ecosystem lock graphs.
use crate::{
    Result,
    files::{exists, input_path, object, text, write_json},
    model::{ProjectState, Toolchain},
    native::{
        DEPENDENCY_FIELDS, Fields, NodeLockKind, Packages, admitted_dependency_target,
        dependency_table, native_manifest, package_lock, parse_npm_dependency_spec,
        resolve_lock_node, root_workspace_link,
    },
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub field: String,
    pub name: String,
    pub spec: String,
    pub to: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub identity: Fields,
    pub edges: Vec<Edge>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeGraph {
    pub nodes: BTreeMap<String, Node>,
    pub roots: BTreeMap<String, Vec<Edge>>,
}
#[derive(Clone, Debug)]
pub struct ProductGraph {
    pub graph: NativeGraph,
    pub roots: BTreeMap<String, Vec<Edge>>,
}

fn normalized_local(value: &str) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                if parts.last().is_some_and(|s| s != "..") {
                    parts.pop();
                } else {
                    parts.push("..".into());
                }
            }
            Component::Normal(name) => {
                parts.push(name.to_str().context("Non-UTF8 lock path")?.to_owned())
            }
            _ => anyhow::bail!("External native lock path: {value}"),
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}
pub(crate) fn joined_local(base: &str, value: &str) -> Result<String> {
    ensure!(
        !value.starts_with('/'),
        "External native lock path: {value}"
    );
    normalized_local(&if base.is_empty() {
        value.to_owned()
    } else {
        format!("{base}/{value}")
    })
}
fn artifact_identity(location: &str, data: &Fields) -> Result<Fields> {
    let mut result = Fields::new();
    if !location.contains("node_modules/") {
        result.insert("local".into(), Value::String(location.to_owned()));
        for key in ["name", "version"] {
            if let Some(value) = data.get(key) {
                result.insert(key.into(), value.clone());
            }
        }
        return Ok(result);
    }
    for key in [
        "name",
        "version",
        "resolved",
        "integrity",
        "os",
        "cpu",
        "libc",
        "optional",
        "devOptional",
        "peer",
        "inBundle",
    ] {
        if let Some(value) = data.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    if let Some(value) = result
        .get("resolved")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix("file:"))
    {
        let normalized = format!("file:{}", normalized_local(value)?);
        result.insert("resolved".into(), Value::String(normalized));
    }
    Ok(result)
}

pub fn read_native_graph(root: &Path, kind: NodeLockKind) -> Result<NativeGraph> {
    if kind == NodeLockKind::Pnpm {
        return crate::pnpm::Lock::read(root)?.native_graph(root);
    }
    read_npm_graph(root)
}

fn read_npm_graph(root: &Path) -> Result<NativeGraph> {
    let packages = package_lock(root)?;
    let mut nodes = BTreeMap::new();
    for (location, raw) in &packages {
        if raw.get("link") == Some(&Value::Bool(true)) {
            continue;
        }
        let data = if !location.contains("node_modules/")
            && exists(&input_path(root, Path::new(location).join("package.json"))?)?
        {
            native_manifest(root, location)?
        } else {
            raw.clone()
        };
        let mut edges = Vec::new();
        for field in DEPENDENCY_FIELDS {
            for (name, value) in dependency_table(&data, field)? {
                let spec = text(&value, &format!("{location}:{name}"))?;
                parse_npm_dependency_spec(spec).with_context(|| {
                    format!("Unsupported dependency for preserved graph: {location}:{name}")
                })?;
                let to =
                    admitted_dependency_target(&packages, location, &data, field, &name, spec)?;
                edges.push(Edge {
                    field: field.to_owned(),
                    name,
                    spec: spec.to_owned(),
                    to,
                });
            }
        }
        nodes.insert(
            location.clone(),
            Node {
                identity: artifact_identity(location, &data)?,
                edges,
            },
        );
    }
    let roots = nodes
        .iter()
        .filter(|(location, _)| !location.contains("node_modules/"))
        .map(|(location, node)| (location.clone(), node.edges.clone()))
        .collect();
    Ok(NativeGraph { nodes, roots })
}

pub fn capture_product_graph(root: &Path, state: &ProjectState) -> Result<ProductGraph> {
    let graph = read_native_graph(root, NodeLockKind::for_state(state)?)?;
    Ok(product_graph(graph, state))
}

fn product_graph(graph: NativeGraph, state: &ProjectState) -> ProductGraph {
    let mut roots = BTreeMap::new();
    for (path, edges) in &graph.roots {
        if !path.is_empty() && !state.components.contains_key(path) {
            continue;
        }
        let owned = state.components.get(path).map(|c| &c.tool_dependencies);
        roots.insert(
            path.clone(),
            edges
                .iter()
                .filter(|edge| {
                    edge.field != "devDependencies"
                        || !owned.is_some_and(|tools| tools.contains_key(&edge.name))
                })
                .cloned()
                .collect(),
        );
    }
    ProductGraph { graph, roots }
}

/// Capture the v3 baseline independently of conversion so a faulty importer
/// cannot silently redefine the product graph that an upgrade must preserve.
pub fn read_legacy_npm_product_graph(root: &Path, state: &ProjectState) -> Result<ProductGraph> {
    let packages = package_lock(root)?;
    let mut product = product_graph(read_npm_graph(root)?, state);
    for (location, node) in &mut product.graph.nodes {
        if location.is_empty() || state.components.contains_key(location) {
            continue;
        }
        let raw = packages
            .get(location)
            .context("Legacy npm graph node is absent from its lock")?;
        let local = !location.contains("node_modules/");
        let mut identity = if local {
            node.identity.clone()
        } else {
            Fields::new()
        };
        if !local {
            identity.insert(
                "name".into(),
                json!(node_package_name(location, &node.identity)),
            );
            for field in ["version", "integrity"] {
                if let Some(value) = raw.get(field) {
                    identity.insert(field.into(), value.clone());
                }
            }
            if let Some(file) = raw
                .get("resolved")
                .and_then(Value::as_str)
                .filter(|v| v.starts_with("file:"))
            {
                identity.insert("resolved".into(), json!(file));
            }
            identity.insert(
                "optional".into(),
                json!(raw.get("optional") == Some(&Value::Bool(true))),
            );
        }
        node.identity = identity;
        let optional = dependency_table(raw, "optionalDependencies")?;
        let peer_meta = dependency_table(raw, "peerDependenciesMeta")?;
        let mut edges = BTreeMap::new();
        for edge in &node.edges {
            if edge.field == "devDependencies" || edge.to.is_none() {
                continue;
            }
            if edge.field == "dependencies" && optional.contains_key(&edge.name) {
                continue;
            }
            let is_optional = edge.field == "optionalDependencies"
                || (edge.field == "peerDependencies"
                    && peer_meta.get(&edge.name).and_then(|v| v.get("optional"))
                        == Some(&Value::Bool(true)));
            let field = if is_optional {
                "optionalDependencies"
            } else {
                "dependencies"
            };
            edges.insert(
                (field.to_owned(), edge.name.clone()),
                Edge {
                    field: field.into(),
                    name: edge.name.clone(),
                    spec: String::new(),
                    to: edge.to.clone(),
                },
            );
        }
        node.edges = edges.into_values().collect();
    }
    for (path, edges) in &mut product.graph.roots {
        if path.is_empty() || !state.components.contains_key(path) {
            continue;
        }
        let manifest = native_manifest(root, path)?;
        for edge in edges.iter_mut() {
            if edge.field == "peerDependencies" {
                continue;
            }
            if let Some(target) = edge
                .to
                .as_ref()
                .filter(|t| state.components.contains_key(*t))
                && !matches!(
                    parse_npm_dependency_spec(&edge.spec)?,
                    crate::native::NpmDepSpec::Alias { .. }
                )
            {
                let target_manifest = native_manifest(root, target)?;
                let public = manifest.get("private") == Some(&Value::Bool(false))
                    && target_manifest.get("private") == Some(&Value::Bool(false));
                edge.spec = if public && edge.field != "devDependencies" {
                    "workspace:^"
                } else {
                    "workspace:*"
                }
                .into();
            }
        }
        if let Some(node) = product.graph.nodes.get_mut(path) {
            node.edges = edges.clone();
        }
    }
    for (path, edges) in &mut product.roots {
        let current = &product.graph.roots[path];
        for edge in edges {
            if let Some(updated) = current
                .iter()
                .find(|e| e.field == edge.field && e.name == edge.name)
            {
                *edge = updated.clone();
            }
        }
    }
    Ok(product)
}

pub fn assert_product_graphs_preserved(before: &ProductGraph, after: &NativeGraph) -> Result<()> {
    struct Comparison<'a> {
        before: &'a NativeGraph,
        after: &'a NativeGraph,
        mapping: BTreeMap<String, String>,
        seen: BTreeSet<(String, String)>,
    }
    impl Comparison<'_> {
        fn edge(&mut self, old: &Edge, next: Option<&Edge>, label: &str) -> Result<()> {
            let next = next.with_context(|| format!("Product dependency conflict: {label}"))?;
            ensure!(
                old.field == next.field
                    && old.name == next.name
                    && old.spec == next.spec
                    && old.to.is_none() == next.to.is_none(),
                "Product dependency conflict: {label}"
            );
            let (Some(old_target), Some(next_target)) = (&old.to, &next.to) else {
                return Ok(());
            };
            if let Some(mapped) = self.mapping.get(old_target) {
                ensure!(
                    mapped == crate::pnpm::strip_peer_suffixes(next_target),
                    "Product shared dependency split: {label}"
                );
            }
            self.mapping.insert(
                old_target.clone(),
                crate::pnpm::strip_peer_suffixes(next_target).to_owned(),
            );
            if !self.seen.insert((old_target.clone(), next_target.clone())) {
                return Ok(());
            }
            let a = self
                .before
                .nodes
                .get(old_target)
                .with_context(|| format!("Product artifact disappeared: {label}"))?;
            let b = self
                .after
                .nodes
                .get(next_target)
                .with_context(|| format!("Product artifact disappeared: {label}"))?;
            ensure!(
                a.identity == b.identity,
                "Product artifact changed: {label}"
            );
            let aa: Vec<_> = a
                .edges
                .iter()
                .filter(|edge| edge.field != "devDependencies")
                .collect();
            let bb: Vec<_> = b
                .edges
                .iter()
                .filter(|edge| edge.field != "devDependencies")
                .collect();
            ensure!(
                aa.len() == bb.len(),
                "Product dependency topology changed: {label}"
            );
            for edge in aa {
                self.edge(
                    edge,
                    bb.iter()
                        .copied()
                        .find(|item| item.field == edge.field && item.name == edge.name),
                    &format!("{label} -> {}", edge.name),
                )?;
            }
            Ok(())
        }
    }
    for (root, edges) in &before.roots {
        // Preserve sharing within a product root while allowing npm to relocate
        // or deduplicate identical artifacts between independent roots.
        let mut comparison = Comparison {
            before: &before.graph,
            after,
            mapping: BTreeMap::new(),
            seen: BTreeSet::new(),
        };
        let final_edges = after
            .roots
            .get(root)
            .with_context(|| format!("Product native root disappeared: {root}"))?;
        for edge in edges {
            comparison.edge(
                edge,
                final_edges
                    .iter()
                    .find(|next| next.field == edge.field && next.name == edge.name),
                &format!(
                    "{}:{}",
                    if root.is_empty() { "root" } else { root },
                    edge.name
                ),
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct LockSeed {
    pub prefix: String,
    pub lock: Value,
}
pub fn seed_workspace_lock(root: &Path, state: &ProjectState, seeds: &[LockSeed]) -> Result<()> {
    seed_workspace_lock_with_reconciliation(root, state, seeds, false)
}

pub(crate) fn seed_workspace_lock_with_reconciliation(
    root: &Path,
    state: &ProjectState,
    seeds: &[LockSeed],
    reconcile: bool,
) -> Result<()> {
    seed_workspace_lock_preserving(root, state, seeds, None, reconcile)
}

pub(crate) fn seed_workspace_lock_preserving(
    root: &Path,
    state: &ProjectState,
    seeds: &[LockSeed],
    existing_lock: Option<crate::pnpm::Lock>,
    reconcile: bool,
) -> Result<()> {
    if crate::files::SemVer::parse(&state.version)? < crate::files::SemVer::parse("0.5.0")? {
        return seed_legacy_workspace_lock(root, state, seeds);
    }
    let mut lock = crate::pnpm::Lock::default();
    let mut protected_snapshots = BTreeSet::new();
    for seed in seeds {
        let legacy = seed.lock.get("lockfileVersion") == Some(&json!(3));
        let mut source = if legacy {
            let mut packages = Packages::new();
            for (key, value) in object(
                seed.lock
                    .get("packages")
                    .context("Missing legacy seed packages")?,
                "legacy packages",
            )? {
                let target = joined_local(&seed.prefix, key)?;
                let target = if target == "." { String::new() } else { target };
                let mut record = object(value, "legacy record")?.clone();
                if record.get("link") == Some(&Value::Bool(true)) {
                    let resolved = text(
                        record.get("resolved").context("Missing legacy link")?,
                        "legacy link",
                    )?;
                    record.insert(
                        "resolved".into(),
                        json!(joined_local(&seed.prefix, resolved)?),
                    );
                } else if let Some(file) = record
                    .get("resolved")
                    .and_then(Value::as_str)
                    .and_then(|s| s.strip_prefix("file:"))
                {
                    record.insert(
                        "resolved".into(),
                        json!(format!("file:{}", joined_local(&seed.prefix, file)?)),
                    );
                }
                packages.insert(target, record);
            }
            let protected = crate::pnpm::legacy_product_locations(root, state, &packages)?;
            crate::pnpm::import_legacy_npm_lock_seed_preserving(
                root, &packages, &protected, reconcile,
            )?
        } else {
            crate::pnpm::Lock::from_value(&seed.lock)?
        };
        if !legacy && !seed.prefix.is_empty() {
            source.rebase_local_paths(&seed.prefix)?;
        }
        source.importers = source
            .importers
            .into_iter()
            .map(|(id, importer)| {
                let target = if legacy || seed.prefix.is_empty() {
                    id
                } else if id == "." {
                    seed.prefix.clone()
                } else {
                    format!("{}/{id}", seed.prefix)
                };
                (target, importer)
            })
            .collect();
        let source_protected = protected_snapshot_keys(root, state, &source)?;
        lock.settings.extend(source.settings);
        lock.extra.extend(source.extra);
        for (key, value) in source.packages {
            if let Some(previous) = lock.packages.get_mut(&key) {
                ensure!(
                    previous.get("resolution").and_then(|v| v.get("integrity"))
                        == value.get("resolution").and_then(|v| v.get("integrity")),
                    "Conflicting pnpm package seed: {key}"
                );
                previous.extend(value);
            } else {
                lock.packages.insert(key, value);
            }
        }
        for (key, value) in source.snapshots {
            if let Some(previous) = lock.snapshots.get_mut(&key) {
                crate::pnpm::merge_snapshot(
                    previous,
                    value,
                    protected_snapshots.contains(&key),
                    source_protected.contains(&key),
                    reconcile,
                    &key,
                )?;
            } else {
                lock.snapshots.insert(key, value);
            }
        }
        protected_snapshots.extend(source_protected);
        lock.importers.extend(source.importers);
    }
    crate::pnpm::refresh_importers(root, state, &mut lock)?;
    crate::files::remove_file(&input_path(root, "package-lock.json")?)?;
    // pnpm and the Rust emitter format the same v9 lock differently. Preserve
    // the project's bytes, including comments, when the seed adds no changes.
    let path = input_path(root, "pnpm-lock.yaml")?;
    let regular_non_exec = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 == 0
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if regular_non_exec {
        let unchanged = match existing_lock {
            Some(current) => current == lock,
            None => crate::pnpm::read_yaml::<crate::pnpm::Lock>(root, "pnpm-lock.yaml")? == lock,
        };
        if unchanged {
            return Ok(());
        }
    }
    lock.write(root)
}

fn protected_snapshot_keys(
    root: &Path,
    state: &ProjectState,
    lock: &crate::pnpm::Lock,
) -> Result<BTreeSet<String>> {
    enum Pending {
        Importer(String, bool),
        Snapshot(String),
    }
    fn enqueue(
        root: &Path,
        id: &str,
        name: &str,
        reference: &str,
        pending: &mut Vec<Pending>,
    ) -> Result<()> {
        if let Some(path) = reference.strip_prefix("link:") {
            let target = crate::pnpm::importer_id(&crate::native::local_path(
                root,
                crate::pnpm::root_path(id),
                path,
            )?)
            .to_owned();
            pending.push(Pending::Importer(target, false));
        } else {
            pending.push(Pending::Snapshot(crate::pnpm::snapshot_key(
                name, reference,
            )?));
        }
        Ok(())
    }
    let mut pending = lock
        .importers
        .keys()
        .filter(|id| id.as_str() == "." || state.components.contains_key(id.as_str()))
        .map(|id| Pending::Importer(id.clone(), true))
        .collect::<Vec<_>>();
    let mut visited_importers = BTreeMap::new();
    let mut protected = BTreeSet::new();
    while let Some(item) = pending.pop() {
        match item {
            Pending::Importer(id, include_dev) => {
                if visited_importers
                    .get(&id)
                    .is_some_and(|old: &bool| *old || !include_dev)
                {
                    continue;
                }
                visited_importers.insert(id.clone(), include_dev);
                let path = crate::pnpm::root_path(&id);
                let Some(importer) = lock.importers.get(&id) else {
                    // A template can link to another registered component whose
                    // importer is supplied by a separate seed.
                    ensure!(
                        state.components.contains_key(path),
                        "Missing product importer in lock seed"
                    );
                    continue;
                };
                for field in crate::pnpm::PHYSICAL_FIELDS {
                    for (name, entry) in importer.field(field) {
                        if field == "devDependencies"
                            && (!include_dev
                                || state
                                    .components
                                    .get(path)
                                    .is_some_and(|c| c.tool_dependencies.contains_key(name)))
                        {
                            continue;
                        }
                        enqueue(root, &id, name, &entry.version, &mut pending)?;
                    }
                }
                let manifest = native_manifest(root, path)?;
                for name in dependency_table(&manifest, "peerDependencies")?.keys() {
                    if let Some(entry) = importer
                        .dependency(name)
                        .or_else(|| lock.importers.get(".").and_then(|i| i.dependency(name)))
                    {
                        let from = if importer.dependency(name).is_some() {
                            &id
                        } else {
                            "."
                        };
                        enqueue(root, from, name, &entry.version, &mut pending)?;
                    }
                }
            }
            Pending::Snapshot(key) => {
                if !protected.insert(key.clone()) {
                    continue;
                }
                let snapshot = lock
                    .snapshots
                    .get(&key)
                    .context("Missing product snapshot in lock seed")?;
                for field in ["dependencies", "optionalDependencies"] {
                    for (name, reference) in dependency_table(snapshot, field)? {
                        enqueue(
                            root,
                            ".",
                            &name,
                            text(&reference, "Product snapshot reference")?,
                            &mut pending,
                        )?;
                    }
                }
            }
        }
    }
    Ok(protected)
}

fn seed_legacy_workspace_lock(root: &Path, state: &ProjectState, seeds: &[LockSeed]) -> Result<()> {
    let manifest = native_manifest(root, "")?;
    let mut packages: Packages = BTreeMap::from([(String::new(), manifest.clone())]);
    for seed in seeds {
        let lock = object(&seed.lock, "native lock seed")?;
        ensure!(
            lock.get("lockfileVersion") == Some(&json!(3)),
            "Migration supports native npm lockfileVersion 3"
        );
        let records = object(
            lock.get("packages").context("Missing seed packages")?,
            "seed packages",
        )?;
        for (key, value) in records {
            let target = joined_local(&seed.prefix, key)?;
            ensure!(
                target != ".." && !target.starts_with("../"),
                "External native lock path: {target}"
            );
            let mut record = object(value, key)?.clone();
            if record.get("link") == Some(&Value::Bool(true)) {
                let resolved = text(
                    record.get("resolved").context("Missing link target")?,
                    "link target",
                )?;
                record.insert(
                    "resolved".into(),
                    Value::String(joined_local(&seed.prefix, resolved)?),
                );
            } else if let Some(resolved) = record
                .get("resolved")
                .and_then(Value::as_str)
                .and_then(|v| v.strip_prefix("file:"))
            {
                let resolved = format!("file:{}", joined_local(&seed.prefix, resolved)?);
                record.insert("resolved".into(), Value::String(resolved));
            }
            if target == "." {
                continue;
            }
            if let Some(existing) = packages.get_mut(&target) {
                if existing != &record {
                    ensure!(
                        ["version", "integrity", "resolved"]
                            .iter()
                            .all(|field| existing.get(*field) == record.get(*field)),
                        "Conflicting native lock seed: {target}"
                    );
                    existing.extend(record);
                }
            } else {
                packages.insert(target, record);
            }
        }
    }
    for (path, component) in &state.components {
        if component.toolchain != Toolchain::Node {
            continue;
        }
        let current = native_manifest(root, path)?;
        let name = text(
            current
                .get("name")
                .context("Missing workspace package name")?,
            "workspace package name",
        )?
        .to_owned();
        let link = format!("node_modules/{name}");
        packages.retain(|location, value| {
            !(root_workspace_link(location)
                && value.get("link") == Some(&Value::Bool(true))
                && value.get("resolved") == Some(&Value::String(path.clone()))
                && location != &link)
        });
        packages.insert(path.clone(), current);
        ensure!(
            packages
                .get(&link)
                .is_none_or(|record| record.get("link") == Some(&Value::Bool(true))),
            "Workspace name collides with installed root package: {name}"
        );
        packages.insert(
            link,
            object(&json!({"resolved":path,"link":true}), "workspace link")?.clone(),
        );
    }
    for (location, record) in &packages {
        if record.get("link") == Some(&Value::Bool(true)) {
            resolve_lock_node(&packages, location)?;
        }
    }
    let mut output = json!({"lockfileVersion":3,"requires":true,"packages":packages});
    for field in ["name", "version"] {
        if let Some(value) = manifest.get(field) {
            output[field] = value.clone();
        }
    }
    write_json(&root.join("package-lock.json"), &output)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReconcileStrategy {
    Seeded,
    PruneDev,
    CleanFallback,
}

impl ReconcileStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seeded => "seeded",
            Self::PruneDev => "prune-dev",
            Self::CleanFallback => "clean-fallback",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconciledPackageChange {
    pub path: String,
    pub name: String,
    pub before_version: String,
    pub after_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockReconciliationReport {
    pub strategy: ReconcileStrategy,
    pub changed_packages: Vec<ReconciledPackageChange>,
    pub added_count: usize,
    pub removed_count: usize,
}

fn node_package_name(location: &str, identity: &Fields) -> String {
    identity
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            location
                .rsplit("node_modules/")
                .next()
                .unwrap_or(location)
                .to_owned()
        })
}

fn node_version(identity: &Fields) -> String {
    identity
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

pub fn compute_reconciliation_report(
    strategy: ReconcileStrategy,
    before: &NativeGraph,
    after: &NativeGraph,
) -> LockReconciliationReport {
    fn external(graph: &NativeGraph) -> BTreeMap<String, BTreeMap<String, (&String, &Node)>> {
        let mut packages: BTreeMap<String, BTreeMap<String, (&String, &Node)>> = BTreeMap::new();
        for (path, node) in &graph.nodes {
            if !graph.roots.contains_key(path) {
                packages
                    .entry(node_package_name(path, &node.identity))
                    .or_default()
                    .insert(node_version(&node.identity), (path, node));
            }
        }
        packages
    }
    let old = external(before);
    let next = external(after);
    let mut changed_packages = Vec::new();
    let mut added_count = 0;
    let mut removed_count = 0;
    for name in old.keys().chain(next.keys()).collect::<BTreeSet<_>>() {
        match (old.get(name), next.get(name)) {
            (Some(a), Some(b)) => {
                let old_versions = a.keys().cloned().collect::<Vec<_>>().join(", ");
                let next_versions = b.keys().cloned().collect::<Vec<_>>().join(", ");
                if old_versions != next_versions
                    || a.iter().any(|(version, (_, node))| {
                        b.get(version)
                            .is_none_or(|(_, next)| node.identity != next.identity)
                    })
                {
                    changed_packages.push(ReconciledPackageChange {
                        path: b.values().next().unwrap().0.clone(),
                        name: name.clone(),
                        before_version: old_versions,
                        after_version: next_versions,
                    });
                }
            }
            (None, Some(b)) => added_count += b.len(),
            (Some(a), None) => removed_count += a.len(),
            _ => {}
        }
    }
    LockReconciliationReport {
        strategy,
        changed_packages,
        added_count,
        removed_count,
    }
}

pub fn format_product_graph_drift(
    before: &ProductGraph,
    after: &NativeGraph,
    cause: &anyhow::Error,
) -> String {
    let report = compute_reconciliation_report(ReconcileStrategy::Seeded, &before.graph, after);
    let mut details: Vec<String> = report
        .changed_packages
        .iter()
        .take(10)
        .map(|item| {
            format!(
                "{}: {} -> {}",
                item.name, item.before_version, item.after_version
            )
        })
        .collect();
    if details.len() < 10 {
        for (location, old_node) in &before.graph.nodes {
            if details.len() >= 10 {
                break;
            }
            if !before.graph.roots.contains_key(location) && !after.nodes.contains_key(location) {
                let pkg = node_package_name(location, &old_node.identity);
                let old_ver = node_version(&old_node.identity);
                let relocated = after
                    .nodes
                    .iter()
                    .find(|(loc, n)| {
                        !after.roots.contains_key(*loc)
                            && node_package_name(loc, &n.identity) == pkg
                    })
                    .map(|(_, n)| node_version(&n.identity))
                    .unwrap_or_else(|| "removed".to_owned());
                if relocated != old_ver {
                    let entry = format!("{pkg}: {old_ver} -> {relocated}");
                    if !details.contains(&entry) {
                        details.push(entry);
                    }
                }
            }
        }
    }
    let summary = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    format!(
        "Product lockfile nodes drifted during resolution{summary}: {cause}; inspect with --reconcile-lock --preview and apply with --reconcile-lock"
    )
}

pub fn assert_direct_product_roots_preserved(
    before: &ProductGraph,
    after: &NativeGraph,
) -> Result<()> {
    for (root, edges) in &before.roots {
        let final_edges = after
            .roots
            .get(root)
            .with_context(|| format!("Product native root disappeared: {root}"))?;
        let root_label = if root.is_empty() {
            "root"
        } else {
            root.as_str()
        };
        for old in edges {
            let next = final_edges
                .iter()
                .find(|item| item.field == old.field && item.name == old.name)
                .with_context(|| {
                    format!("Product dependency conflict: {root_label}:{}", old.name)
                })?;
            ensure!(
                old.field == next.field
                    && old.name == next.name
                    && old.spec == next.spec
                    && old.to.is_none() == next.to.is_none(),
                "Product dependency conflict: {root_label}:{}",
                old.name
            );
            let local = old
                .to
                .as_ref()
                .and_then(|target| before.graph.nodes.get(target))
                .and_then(|node| node.identity.get("local"));
            if let Some(local) = local {
                let next_local = next
                    .to
                    .as_ref()
                    .and_then(|target| after.nodes.get(target))
                    .and_then(|node| node.identity.get("local"));
                ensure!(
                    Some(local) == next_local,
                    "Local workspace dependency target changed: {root_label}:{}",
                    old.name
                );
            } else if old.spec.starts_with("file:") {
                ensure!(
                    old.to == next.to,
                    "Local workspace dependency target changed: {root_label}:{}",
                    old.name
                );
            }
        }
    }
    Ok(())
}

pub fn capture_retained_product_graph(
    root: &Path,
    state: &ProjectState,
    old_lock: &Value,
) -> Result<(NativeGraph, ProductGraph)> {
    if old_lock.get("lockfileVersion") == Some(&json!("9.0")) {
        let graph = crate::pnpm::Lock::from_value(old_lock)?.retained_graph(root)?;
        let roots = graph
            .roots
            .iter()
            .filter(|(path, _)| path.is_empty() || state.components.contains_key(*path))
            .map(|(path, edges)| {
                (
                    path.clone(),
                    edges
                        .iter()
                        .filter(|edge| {
                            edge.field != "devDependencies"
                                || !state
                                    .components
                                    .get(path)
                                    .is_some_and(|c| c.tool_dependencies.contains_key(&edge.name))
                        })
                        .cloned()
                        .collect(),
                )
            })
            .collect();
        return Ok((graph.clone(), ProductGraph { graph, roots }));
    }
    let raw_packages = object(
        old_lock
            .get("packages")
            .context("Missing native lock packages")?,
        "native lock packages",
    )?;
    let mut old_packages: Packages = BTreeMap::new();
    for (location, value) in raw_packages {
        old_packages.insert(location.clone(), object(value, location)?.clone());
    }
    let mut nodes = BTreeMap::new();
    for (location, raw) in &old_packages {
        if raw.get("link") == Some(&Value::Bool(true)) {
            continue;
        }
        let is_workspace = !location.contains("node_modules/")
            && exists(&input_path(root, Path::new(location).join("package.json"))?)?;
        let current_manifest = if is_workspace {
            Some(native_manifest(root, location)?)
        } else {
            None
        };
        let mut edges = Vec::new();
        for field in DEPENDENCY_FIELDS {
            let locked_deps = dependency_table(raw, field)?;
            let current_deps = match &current_manifest {
                Some(m) => Some(dependency_table(m, field)?),
                None => None,
            };
            for (name, value) in locked_deps {
                let Ok(spec) = text(&value, &format!("{location}:{name}")) else {
                    continue;
                };
                if let Some(cur) = &current_deps
                    && cur.get(&name).and_then(Value::as_str) != Some(spec)
                {
                    continue;
                }
                if parse_npm_dependency_spec(spec).is_err() {
                    continue;
                }
                if let Ok(to) =
                    admitted_dependency_target(&old_packages, location, raw, field, &name, spec)
                {
                    edges.push(Edge {
                        field: field.to_owned(),
                        name,
                        spec: spec.to_owned(),
                        to,
                    });
                }
            }
        }
        let identity_source = current_manifest.as_ref().unwrap_or(raw);
        nodes.insert(
            location.clone(),
            Node {
                identity: artifact_identity(location, identity_source)?,
                edges,
            },
        );
    }
    let graph_roots: BTreeMap<String, Vec<Edge>> = nodes
        .iter()
        .filter(|(location, _)| !location.contains("node_modules/"))
        .map(|(location, node)| (location.clone(), node.edges.clone()))
        .collect();
    let mut product_roots = BTreeMap::new();
    for (path, edges) in &graph_roots {
        if !path.is_empty() && !state.components.contains_key(path) {
            continue;
        }
        let owned = state.components.get(path).map(|c| &c.tool_dependencies);
        product_roots.insert(
            path.clone(),
            edges
                .iter()
                .filter(|edge| {
                    edge.field != "devDependencies"
                        || !owned.is_some_and(|tools| tools.contains_key(&edge.name))
                })
                .cloned()
                .collect(),
        );
    }
    let graph = NativeGraph {
        nodes,
        roots: graph_roots,
    };
    let product = ProductGraph {
        graph: graph.clone(),
        roots: product_roots,
    };
    Ok((graph, product))
}

pub fn assert_retained_product_graph_preserved(
    before: &ProductGraph,
    after: &NativeGraph,
) -> Result<()> {
    struct RetainedComparison<'a> {
        before: &'a ProductGraph,
        after: &'a NativeGraph,
        mapping: BTreeMap<String, String>,
        seen: BTreeSet<(String, String)>,
    }
    impl RetainedComparison<'_> {
        fn edge(&mut self, old: &Edge, next: Option<&Edge>, label: &str) -> Result<()> {
            let next = next.with_context(|| format!("Product dependency conflict: {label}"))?;
            ensure!(
                old.field == next.field
                    && old.name == next.name
                    && old.spec == next.spec
                    && old.to.is_none() == next.to.is_none(),
                "Product dependency conflict: {label}"
            );
            let (Some(old_target), Some(next_target)) = (&old.to, &next.to) else {
                return Ok(());
            };
            if let Some(mapped) = self.mapping.get(old_target) {
                ensure!(
                    mapped == crate::pnpm::strip_peer_suffixes(next_target),
                    "Product shared dependency split: {label}"
                );
            }
            self.mapping.insert(
                old_target.clone(),
                crate::pnpm::strip_peer_suffixes(next_target).to_owned(),
            );
            if !self.seen.insert((old_target.clone(), next_target.clone())) {
                return Ok(());
            }
            let a = self
                .before
                .graph
                .nodes
                .get(old_target)
                .with_context(|| format!("Product artifact disappeared: {label}"))?;
            let b = self
                .after
                .nodes
                .get(next_target)
                .with_context(|| format!("Product artifact disappeared: {label}"))?;
            ensure!(
                a.identity == b.identity,
                "Product artifact changed: {label}"
            );
            if !old_target.contains("node_modules/") && self.before.roots.contains_key(old_target) {
                ensure!(
                    old_target == next_target,
                    "Local workspace dependency target changed: {label}"
                );
                return Ok(());
            }
            let aa: Vec<_> = a
                .edges
                .iter()
                .filter(|edge| edge.field != "devDependencies")
                .collect();
            let bb: Vec<_> = b
                .edges
                .iter()
                .filter(|edge| edge.field != "devDependencies")
                .collect();
            ensure!(
                aa.len() == bb.len(),
                "Product dependency topology changed: {label}"
            );
            for edge in aa {
                self.edge(
                    edge,
                    bb.iter()
                        .copied()
                        .find(|item| item.field == edge.field && item.name == edge.name),
                    &format!("{label} -> {}", edge.name),
                )?;
            }
            Ok(())
        }
    }
    for (root, edges) in &before.roots {
        let mut comparison = RetainedComparison {
            before,
            after,
            mapping: BTreeMap::new(),
            seen: BTreeSet::new(),
        };
        let final_edges = after
            .roots
            .get(root)
            .with_context(|| format!("Product native root disappeared: {root}"))?;
        for edge in edges {
            comparison.edge(
                edge,
                final_edges
                    .iter()
                    .find(|next| next.field == edge.field && next.name == edge.name),
                &format!(
                    "{}:{}",
                    if root.is_empty() { "root" } else { root },
                    edge.name
                ),
            )?;
        }
    }
    Ok(())
}

pub fn prune_dev_lock_packages(lock_path: &Path) -> Result<()> {
    let mut lock: Value = crate::files::read_json(lock_path)?;
    let packages = lock
        .get_mut("packages")
        .and_then(Value::as_object_mut)
        .context("Missing packages in seeded package-lock.json")?;
    packages.retain(|location, record| {
        if !location.contains("node_modules/") {
            return true;
        }
        let is_dev = record.get("dev") == Some(&Value::Bool(true))
            || record.get("devOptional") == Some(&Value::Bool(true));
        !is_dev
    });
    write_json(lock_path, &lock)
}
