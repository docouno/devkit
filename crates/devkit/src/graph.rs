//! Product-rooted graph preservation over the physical lock graph chosen by npm.
use crate::{
    Result,
    files::{exists, input_path, object, text, write_json},
    model::{ProjectState, Toolchain},
    native::{
        DEPENDENCY_FIELDS, Fields, Packages, admitted_dependency_target, dependency_table,
        native_manifest, package_lock, parse_npm_dependency_spec, resolve_lock_node,
        root_workspace_link,
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
fn joined_local(base: &str, value: &str) -> Result<String> {
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

pub fn read_native_graph(root: &Path) -> Result<NativeGraph> {
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
    let graph = read_native_graph(root)?;
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
    Ok(ProductGraph { graph, roots })
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
                    mapped == next_target,
                    "Product shared dependency split: {label}"
                );
            }
            self.mapping.insert(old_target.clone(), next_target.clone());
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
    let mut changed_packages = Vec::new();
    let mut added_count = 0usize;
    let mut removed_count = 0usize;
    let all_locations: BTreeSet<&String> = before
        .nodes
        .keys()
        .chain(after.nodes.keys())
        .filter(|loc| loc.contains("node_modules/"))
        .collect();
    for location in all_locations {
        match (before.nodes.get(location), after.nodes.get(location)) {
            (Some(old_node), Some(new_node)) => {
                if old_node.identity != new_node.identity {
                    changed_packages.push(ReconciledPackageChange {
                        path: location.clone(),
                        name: node_package_name(location, &new_node.identity),
                        before_version: node_version(&old_node.identity),
                        after_version: node_version(&new_node.identity),
                    });
                }
            }
            (None, Some(_)) => added_count += 1,
            (Some(_), None) => removed_count += 1,
            (None, None) => {}
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
            if location.contains("node_modules/") && !after.nodes.contains_key(location) {
                let pkg = node_package_name(location, &old_node.identity);
                let old_ver = node_version(&old_node.identity);
                let relocated = after
                    .nodes
                    .iter()
                    .find(|(loc, n)| {
                        loc.contains("node_modules/") && node_package_name(loc, &n.identity) == pkg
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
            if old.spec.starts_with("file:")
                || old
                    .to
                    .as_ref()
                    .is_some_and(|target| !target.contains("node_modules/"))
            {
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
                    mapped == next_target,
                    "Product shared dependency split: {label}"
                );
            }
            self.mapping.insert(old_target.clone(), next_target.clone());
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
