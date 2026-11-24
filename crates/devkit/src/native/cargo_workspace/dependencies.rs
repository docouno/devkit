//! Compare manifest declarations with the package IDs Cargo selected.
use super::*;
use semver::VersionReq;

#[derive(Debug, Clone)]
pub struct CargoDeclaration {
    pub alias: String,
    pub name: String,
    pub requirement: Option<VersionReq>,
    /// Checkout-relative directory after resolving workspace inheritance.
    pub path: Option<String>,
    /// The Cargo lock source, excluding only the precise Git commit.
    pub source: String,
    pub optional: bool,
    pub dev: bool,
}

#[derive(Debug, Clone)]
pub struct CargoPatch {
    pub origin: String,
    pub target: CargoDeclaration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoReplacementSpec {
    name: String,
    version: Version,
    url: url::Url,
    kind: Option<String>,
}

// PackageIdSpec uses the exact URL and optional source kind. Unlike a patch
// origin, "index.git" in a replacement spec does not match "index".
fn package_source(raw: &str) -> Result<(url::Url, Option<String>)> {
    let (base, kind) = if let Some(git) = raw.strip_prefix("git+") {
        let (url, reference) = git.split_once('?').unwrap_or((git, ""));
        (url, Some(format!("git?{reference}")))
    } else if let Some(registry) = raw.strip_prefix("registry+") {
        (registry, Some("registry".to_owned()))
    } else if let Some(path) = raw.strip_prefix("path+") {
        (path, Some("path".to_owned()))
    } else {
        (raw, raw.starts_with("sparse+").then(|| "sparse".to_owned()))
    };
    Ok((url::Url::parse(base)?, kind))
}

impl CargoReplacementSpec {
    pub(crate) fn parse(key: &str) -> Result<Self> {
        let (source, spec) = key.rsplit_once('#').unwrap_or((CRATES_IO, key));
        let (url, kind) = package_source(source)?;
        let (name, version) =
            if let Some(pair) = spec.split_once('@').or_else(|| spec.split_once(':')) {
                pair
            } else {
                let name = url
                    .path_segments()
                    .and_then(|mut s| s.next_back())
                    .context("Missing replacement package name")?;
                (name, spec)
            };
        let version = Version::parse(version)
            .with_context(|| format!("Cargo replacement must specify an exact version: {key}"))?;
        Ok(Self {
            name: name.to_owned(),
            version,
            url,
            kind,
        })
    }

    pub(crate) fn applies_to(&self, id: &CargoPackageId) -> Result<bool> {
        if self.name != id.name || self.version != Version::parse(&id.version)? {
            return Ok(false);
        }
        let Some(source) = id.source.as_deref() else {
            return Ok(false);
        };
        let (url, kind) = package_source(crate::cargo_lock::reference_source(source))?;
        Ok(self.url == url && (self.kind.is_none() || self.kind == kind))
    }

    fn applies_in_workspace(
        &self,
        model: &CargoWorkspaceModel,
        id: &CargoPackageId,
    ) -> Result<bool> {
        if self.url.scheme() != "file" || id.source.is_some() {
            return self.applies_to(id);
        }
        if self.name != id.name || self.version != Version::parse(&id.version)? {
            return Ok(false);
        }
        let path = Path::new("/workspace").join(&model.local_package(id)?.rel_dir);
        Ok(url::Url::from_file_path(path).ok().as_ref() == Some(&self.url))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CargoReplacement {
    pub(crate) spec: CargoReplacementSpec,
    pub(crate) target: CargoDeclaration,
}
impl CargoReplacement {
    pub(super) fn new(
        key: &str,
        value: &Value,
        ctx: &ReadContext<'_>,
        registries: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let spec = CargoReplacementSpec::parse(key)?;
        let target = ctx.declaration("", &spec.name, value, registries)?;
        Ok(Self { spec, target })
    }
}

pub(super) const CRATES_IO: &str = "https://github.com/rust-lang/crates.io-index";

/// Cargo compares patch origins by canonical URL. Keep the sparse protocol
/// and registry URL query intact. Only Git lock sources encode their reference
/// and precise commit in the query and fragment.
fn canonical_url(raw: &str) -> Result<String> {
    let mut url =
        url::Url::parse(raw).with_context(|| format!("Invalid Cargo source URL: {raw}"))?;
    ensure!(
        !url.cannot_be_a_base(),
        "Unsupported Cargo source URL: {raw}"
    );
    if url.path().ends_with('/') {
        url.path_segments_mut()
            .expect("hierarchical URL")
            .pop_if_empty();
    }
    if !url.scheme().contains('+') {
        if url.host_str() == Some("github.com") {
            url = url::Url::parse(&format!("https{}", &url[url::Position::AfterScheme..]))?;
            url.set_path(&url.path().to_lowercase());
        }
        if let Some(path) = url.path().strip_suffix(".git").map(str::to_owned) {
            url.set_path(&path);
        }
    }
    Ok(url.to_string())
}

pub(crate) fn cargo_patch_origin(
    raw: &str,
    registries: &BTreeMap<String, String>,
) -> Result<String> {
    let url = registries.get(raw).map(String::as_str).unwrap_or(raw);
    canonical_url(url)
}

fn cargo_source_origin(source: &str) -> Result<String> {
    if let Some(git) = source.strip_prefix("git+") {
        return canonical_url(git.split(['?', '#']).next().unwrap_or(git));
    }
    canonical_url(source.strip_prefix("registry+").unwrap_or(source))
}

fn sources_match(declared: &str, selected: &str) -> Result<bool> {
    if declared.starts_with("git+") && selected.starts_with("git+") {
        let reference = |source: &str| {
            crate::cargo_lock::reference_source(source)
                .split_once('?')
                .map(|(_, q)| q.to_owned())
        };
        return Ok(reference(declared) == reference(selected)
            && cargo_source_origin(declared)? == cargo_source_origin(selected)?);
    }
    if declared == selected {
        return Ok(true);
    }
    let normalize_registry = |raw: &str| -> Option<String> {
        if let Some(rest) = raw.strip_prefix("sparse+") {
            url::Url::parse(rest).ok().map(|u| format!("sparse+{u}"))
        } else if let Some(rest) = raw.strip_prefix("registry+") {
            url::Url::parse(rest).ok().map(|u| format!("registry+{u}"))
        } else {
            None
        }
    };
    Ok(matches!(
        (normalize_registry(declared), normalize_registry(selected)),
        (Some(a), Some(b)) if a == b
    ))
}

impl CargoDeclaration {
    pub(super) fn accepts_version(&self, id: &CargoPackageId) -> Result<bool> {
        if self.name != id.name {
            return Ok(false);
        }
        let version = Version::parse(&id.version)?;
        Ok(self
            .requirement
            .as_ref()
            .is_none_or(|req| req.matches(&version)))
    }
}

// Find an augmenting path for each locked ID in the small bipartite graph
// between direct edges and declarations. Iteration keeps manifest size off
// the call stack.
fn distinct_declarations(candidates: &[Vec<usize>], declarations: usize) -> bool {
    let mut assigned: Vec<Option<usize>> = vec![None; declarations];
    let mut chosen = vec![None; candidates.len()];
    for start in 0..candidates.len() {
        let mut predecessors = vec![None; declarations];
        let mut queue = VecDeque::from([start]);
        let mut matched = false;
        'search: while let Some(id) = queue.pop_front() {
            for &decl in &candidates[id] {
                if predecessors[decl].is_some() {
                    continue;
                }
                predecessors[decl] = Some(id);
                if let Some(other) = assigned[decl] {
                    queue.push_back(other);
                } else {
                    let mut next_decl = decl;
                    loop {
                        let owner = predecessors[next_decl].expect("visited declaration");
                        let previous = chosen[owner].replace(next_decl);
                        assigned[next_decl] = Some(owner);
                        if let Some(prev) = previous {
                            next_decl = prev;
                        } else {
                            break;
                        }
                    }
                    matched = true;
                    break 'search;
                }
            }
        }
        if !matched {
            return false;
        }
    }
    true
}

impl CargoWorkspaceModel {
    fn direct_source(&self, declaration: &CargoDeclaration, id: &CargoPackageId) -> Result<bool> {
        if let Some(path) = &declaration.path {
            Ok(self
                .local_packages
                .get(path)
                .is_some_and(|package| package.id == *id))
        } else if let Some(source) = &id.source {
            sources_match(&declaration.source, source)
        } else {
            Ok(false)
        }
    }
    pub(crate) fn patch_matches_id(&self, patch: &CargoPatch, id: &CargoPackageId) -> Result<bool> {
        Ok(patch.target.accepts_version(id)? && self.direct_source(&patch.target, id)?)
    }
    pub fn matches_dependency(&self, dep: &CargoDeclaration, id: &CargoPackageId) -> Result<bool> {
        if !dep.accepts_version(id)? {
            return Ok(false);
        }
        if self.direct_source(dep, id)? {
            if let Some(path) = &dep.path {
                return Ok(*path == self.local_package(id)?.rel_dir);
            }
            if self.patches.is_empty() {
                return Ok(true);
            }
            let origin = cargo_source_origin(&dep.source)?;
            // Cargo replaces the registry candidate at exactly this version.
            // It cannot keep an edge to the original source even when both
            // IDs already appear elsewhere in the accepted workspace lock.
            let superseded = self.patches.iter().any(|patch| {
                patch.origin == origin
                    && patch.target.name == dep.name
                    && patch
                        .target
                        .path
                        .as_ref()
                        .and_then(|path| self.local_packages.get(path))
                        .is_some_and(|p| p.id.name == id.name && p.id.version == id.version)
            });
            return Ok(!superseded);
        }
        if dep.path.is_some() || self.patches.is_empty() {
            return Ok(false);
        }
        let origin = cargo_source_origin(&dep.source)?;
        for patch in &self.patches {
            if patch.origin == origin
                && self.patch_matches_id(patch, id)?
                && (patch.target.path.is_none()
                    || patch.target.path.as_ref() == Some(&self.local_package(id)?.rel_dir))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn check_overrides(&self, lock: &crate::cargo_lock::Lock) -> Result<()> {
        for (id, record) in &lock.packages {
            let mut declared = None;
            for rule in &self.replacements {
                if rule.spec.applies_in_workspace(self, id)? {
                    ensure!(
                        declared.replace(rule).is_none(),
                        "Multiple replacements for {} {}",
                        id.name,
                        id.version
                    );
                }
            }
            // Replacing a package with the same source is a no-op in Cargo.
            let required = match declared {
                Some(rule) if !self.direct_source(&rule.target, id)? => Some(rule),
                _ => None,
            };
            let valid = match (required, &record.replacement) {
                (None, None) => true,
                (Some(rule), Some(target)) => {
                    target.name == id.name
                        && target.version == id.version
                        && self.direct_source(&rule.target, target)?
                        && (rule.target.path.is_none()
                            || rule.target.path.as_ref()
                                == Some(&self.local_package(target)?.rel_dir))
                }
                _ => false,
            };
            ensure!(
                valid,
                "Cargo.lock replacement for {} {} differs from [replace]; run make sync",
                id.name,
                id.version
            );
            if id.source.is_some() {
                for child in record
                    .dependencies
                    .iter()
                    .filter(|dep| dep.source.is_none())
                {
                    let path = &self.local_package(child)?.rel_dir;
                    // Remote manifests are absent during offline admission.
                    // A direct edge to local code requires an effective path
                    // patch. [replace] uses a separate edge on its source ID.
                    ensure!(
                        self.patches
                            .iter()
                            .any(|patch| patch.target.name == child.name
                                && patch.target.path.as_ref() == Some(path)),
                        "Cargo.lock selects local {path} from external {} without an applicable Cargo patch; run make sync",
                        id.name
                    );
                }
            }
        }
        Ok(())
    }
    /// Every selected ID needs its own declaration, although several aliases or
    /// target tables may legitimately share one ID. Checking each ID in
    /// isolation would allow a single "*" declaration to justify two versions.
    pub(crate) fn matches_direct(
        &self,
        member: &CargoMember,
        workspace_member: bool,
        deps: &BTreeSet<CargoPackageId>,
    ) -> Result<bool> {
        let mut candidates = vec![Vec::new(); deps.len()];
        let mut bindings = BTreeMap::new();
        for dep in &member.dependencies {
            if dep.dev && !workspace_member {
                continue;
            }
            // Repeating an alias in another target table does not give Cargo
            // another version choice for the very same requirement and source.
            let key = (
                &dep.alias,
                &dep.name,
                dep.requirement.as_ref().map(ToString::to_string),
                &dep.path,
                &dep.source,
            );
            let next = bindings.len();
            let index = *bindings.entry(key).or_insert(next);
            let mut found = false;
            for (i, id) in deps.iter().enumerate() {
                if self.matches_dependency(dep, id)? {
                    candidates[i].push(index);
                    found = true;
                }
            }
            // Cargo locks all workspace-member features. For a dependency
            // outside the workspace it locks the features actually requested.
            if !found && (workspace_member || !dep.optional) {
                return Ok(false);
            }
        }
        Ok(distinct_declarations(&candidates, bindings.len()))
    }
}

/// Config includes first merge by their literal TOML keys. Then Cargo maps
/// each source table to its canonical URL. This order matters when one file
/// uses a registry name and another uses the index URL for the same registry.
#[derive(Default)]
pub(super) struct PatchTables(BTreeMap<String, BTreeMap<String, (String, Value)>>);

impl PatchTables {
    pub(super) fn merge(&mut self, base: &str, data: &Fields) -> Result<()> {
        for (source, rows) in dependency_table(data, "patch")? {
            let table = self.0.entry(source).or_default();
            for (key, value) in object(&rows, "Cargo patch source")? {
                if let Some((directory, previous)) = table.get_mut(key)
                    && let (Some(before), Some(after)) =
                        (previous.as_object_mut(), value.as_object())
                {
                    before.extend(after.clone());
                    if after.contains_key("path") {
                        *directory = base.to_owned();
                    }
                } else {
                    table.insert(key.clone(), (base.to_owned(), value.clone()));
                }
            }
        }
        Ok(())
    }

    pub(super) fn apply(
        self,
        ctx: &ReadContext<'_>,
        registries: &BTreeMap<String, String>,
        patches: &mut BTreeMap<(String, String), CargoPatch>,
    ) -> Result<()> {
        let mut origins = BTreeMap::new();
        for (source, rows) in self.0 {
            origins.insert(cargo_patch_origin(&source, registries)?, rows);
        }
        for (origin, rows) in origins {
            for (alias, (base, value)) in rows {
                let target = ctx.declaration(&base, &alias, &value, registries)?;
                patches.insert(
                    (origin.clone(), alias),
                    CargoPatch {
                        origin: origin.clone(),
                        target,
                    },
                );
            }
        }
        Ok(())
    }
}

impl ReadContext<'_> {
    pub(super) fn declaration(
        &self,
        base: &str,
        alias: &str,
        value: &Value,
        registries: &BTreeMap<String, String>,
    ) -> Result<CargoDeclaration> {
        let detail = value.as_object();
        let string = |key: &str| {
            detail
                .and_then(|d| d.get(key))
                .map(|v| text(v, "Cargo dependency field"))
                .transpose()
        };
        if let Some(table) = detail {
            reject_local_cargo_git(table)?;
        }
        let source = if let Some(git) = string("git")? {
            let mut url = url::Url::parse(git).context("Invalid Cargo Git URL")?;
            for key in ["branch", "tag", "rev"] {
                if let Some(reference) = string(key)? {
                    url.query_pairs_mut().append_pair(key, reference);
                }
            }
            format!("git+{url}")
        } else {
            let name = string("registry")?.unwrap_or("crates-io");
            let index = registries.get(name).with_context(|| {
                format!("Unknown Cargo registry {name}; define its index in the root Cargo config")
            })?;
            if index.starts_with("sparse+") {
                index.clone()
            } else {
                format!("registry+{index}")
            }
        };
        let version = match value {
            Value::String(req) => Some(req.as_str()),
            Value::Object(_) => string("version")?,
            _ => bail!("Invalid Cargo dependency {alias} in {base}/Cargo.toml"),
        };
        Ok(CargoDeclaration {
            alias: alias.into(),
            name: string("package")?.unwrap_or(alias).to_owned(),
            requirement: version
                .map(VersionReq::parse)
                .transpose()
                .with_context(|| {
                    format!("Invalid Cargo version requirement for {alias} in {base}/Cargo.toml")
                })?,
            path: string("path")?
                .map(|v| cargo_local_path(self.root, base, v))
                .transpose()?,
            source,
            optional: value
                .get("optional")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            dev: false,
        })
    }
}
