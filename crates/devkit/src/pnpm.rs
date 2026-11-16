//! Bounded admission of pnpm workspace settings and its resolved v9 graph.
//! Ecosystem resolution stays in the pinned pnpm executable.
use crate::{
    Result,
    files::*,
    native::{self, Fields, NpmDepSpec},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

pub const PHYSICAL_FIELDS: [&str; 3] = ["dependencies", "optionalDependencies", "devDependencies"];
const MAX_BYTES: usize = 32 * 1024 * 1024;

pub fn parse_yaml<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(bytes.len() <= MAX_BYTES, "pnpm YAML exceeds byte budget");
    let mut options = serde_saphyr::Options::default();
    let mut budget = serde_saphyr::Budget::default();
    budget.max_reader_input_bytes = Some(MAX_BYTES);
    budget.max_total_scalar_bytes = MAX_BYTES;
    budget.max_recorded_anchor_bytes = 0;
    budget.max_recorded_anchor_events = 0;
    budget.max_events = 2_000_000;
    budget.max_nodes = 1_000_000;
    budget.max_depth = 64;
    budget.max_documents = 1;
    budget.max_aliases = 0;
    budget.max_anchors = 0;
    budget.max_merge_keys = 0;
    options.budget = Some(budget);
    Ok(serde_saphyr::from_str_with_options(
        std::str::from_utf8(bytes)?,
        options,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn duplicated_legacy_snapshots_keep_required_reachability() {
        let temporary = TempDir::new().unwrap();
        write_json(
            &temporary.path().join("package.json"),
            &json!({"name":"root","private":true}),
        )
        .unwrap();
        let mut packages = native::Packages::new();
        packages.insert(String::new(), Fields::new());
        packages.insert(
            "node_modules/foo".into(),
            object(&json!({"version":"1.0.0"}), "package")
                .unwrap()
                .clone(),
        );
        packages.insert(
            "node_modules/bar/node_modules/foo".into(),
            object(&json!({"version":"1.0.0","optional":true}), "package")
                .unwrap()
                .clone(),
        );
        let lock = import_legacy_npm_lock_seed(temporary.path(), &packages).unwrap();
        assert_eq!(lock.snapshots.len(), 1);
        assert!(!lock.snapshots["foo@1.0.0"].contains_key("optional"));
    }

    fn legacy_variants(root: &Path) -> (native::Packages, crate::model::ProjectState) {
        let root_manifest = json!({"name":"root","version":"1.0.0","private":true});
        let library = json!({"name":"lib","version":"1.0.0","private":true,"dependencies":{"foo":"1.0.0"},"devDependencies":{"tool":"1.0.0"}});
        write_json(&root.join("package.json"), &root_manifest).unwrap();
        write_json(&root.join("packages/lib/package.json"), &library).unwrap();
        let artifact = |name: &str, version: &str| json!({"version":version,"resolved":format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"),"integrity":format!("sha512-{}", "A".repeat(86)+"==")});
        let mut tool = artifact("tool", "1.0.0");
        tool["dependencies"] = json!({"foo":"1.0.0"});
        let mut foo = artifact("foo", "1.0.0");
        foo["dependencies"] = json!({"leaf":"*"});
        let packages = [
            ("", root_manifest),
            ("packages/lib", library),
            ("node_modules/tool", tool),
            ("node_modules/tool/node_modules/foo", foo.clone()),
            (
                "node_modules/tool/node_modules/foo/node_modules/leaf",
                artifact("leaf", "1.0.0"),
            ),
            ("packages/lib/node_modules/foo", foo),
            (
                "packages/lib/node_modules/foo/node_modules/leaf",
                artifact("leaf", "2.0.0"),
            ),
        ]
        .into_iter()
        .map(|(path, value)| (path.into(), value.as_object().unwrap().clone()))
        .collect::<native::Packages>();
        let state = serde_json::from_value(json!({"schemaVersion":2,"version":"0.5.0","releaseIdentity":"a".repeat(64),"generation":"00000000-0000-4000-8000-000000000000","packageMode":"registry","managed":{},"components":{"packages/lib":{"role":"library","toolchain":"node","toolingProfile":"react-library","origin":{"template":"react-client","version":"0.4.1"},"toolDependencies":{"tool":"1.0.0"},"dependencies":[]}}})).unwrap();
        (packages, state)
    }

    #[test]
    fn legacy_tooling_variants_prefer_product_resolutions_and_keep_an_independent_graph_guard() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        let (packages, state) = legacy_variants(root);
        write_json(
            &root.join("package-lock.json"),
            &json!({"lockfileVersion":3,"packages":packages}),
        )
        .unwrap();
        let baseline = crate::graph::read_legacy_npm_product_graph(root, &state).unwrap();
        let protected = legacy_product_locations(root, &state, &packages).unwrap();
        assert!(protected.contains("packages/lib/node_modules/foo"));
        assert!(!protected.contains("node_modules/tool/node_modules/foo"));
        let mut lock =
            import_legacy_npm_lock_seed_preserving(root, &packages, &protected, false).unwrap();
        assert_eq!(lock.snapshots["foo@1.0.0"]["dependencies"]["leaf"], "2.0.0");
        crate::graph::assert_product_graphs_preserved(&baseline, &lock.native_graph(root).unwrap())
            .unwrap();
        lock.snapshots
            .get_mut("foo@1.0.0")
            .unwrap()
            .insert("dependencies".into(), json!({"leaf":"1.0.0"}));
        assert!(
            crate::graph::assert_product_graphs_preserved(
                &baseline,
                &lock.native_graph(root).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn product_dev_variants_require_reconciliation_with_consistent_artifacts() {
        let temporary = TempDir::new().unwrap();
        let (mut packages, mut state) = legacy_variants(temporary.path());
        state
            .components
            .get_mut("packages/lib")
            .unwrap()
            .tool_dependencies
            .clear();
        let protected = legacy_product_locations(temporary.path(), &state, &packages).unwrap();
        assert!(protected.contains("node_modules/tool/node_modules/foo"));
        let error =
            import_legacy_npm_lock_seed_preserving(temporary.path(), &packages, &protected, false)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Conflicting product resolutions")
        );
        import_legacy_npm_lock_seed_preserving(temporary.path(), &packages, &protected, true)
            .unwrap();
        packages
            .get_mut("packages/lib/node_modules/foo")
            .unwrap()
            .insert("integrity".into(), json!("different artifact"));
        assert!(
            import_legacy_npm_lock_seed_preserving(temporary.path(), &packages, &protected, true)
                .unwrap_err()
                .to_string()
                .contains("Conflicting legacy identity")
        );
        let integrity = packages["node_modules/tool/node_modules/foo"]["integrity"].clone();
        let changed = packages.get_mut("packages/lib/node_modules/foo").unwrap();
        changed.insert("integrity".into(), integrity);
        changed.insert("dependencies".into(), json!({"leaf":"^9.0.0"}));
        assert!(
            import_legacy_npm_lock_seed_preserving(temporary.path(), &packages, &protected, true)
                .unwrap_err()
                .to_string()
                .contains("Conflicting legacy dependency declarations")
        );
    }

    #[test]
    fn merging_v9_seeds_preserves_product_paths_and_protects_peers_even_when_development_tools() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        let (packages, mut state) = legacy_variants(root);
        let full = import_legacy_npm_lock_seed_preserving(root, &packages, &BTreeSet::new(), true)
            .unwrap();
        let mut tooling = full.clone();
        tooling
            .importers
            .get_mut("packages/lib")
            .unwrap()
            .dependencies
            .clear();
        write_json(&root.join("packages/lib/package.json"), &json!({"name":"lib","version":"1.0.0","private":true,"devDependencies":{"tool":"1.0.0"}})).unwrap();
        tooling.prune_unused().unwrap();
        write_json(
            &root.join("apps/main/package.json"),
            &json!({"name":"main","version":"1.0.0","private":true,"dependencies":{"foo":"1.0.0"}}),
        )
        .unwrap();
        let mut component = state.components["packages/lib"].clone();
        component.role = crate::model::Role::App;
        component.tooling_profile = crate::model::ToolingProfile::Node;
        component.tool_dependencies.clear();
        state.components.insert("apps/main".into(), component);
        let mut product = Lock::default();
        let mut importer = Importer::default();
        importer.dependencies.insert(
            "foo".into(),
            Dependency {
                specifier: "1.0.0".into(),
                version: "1.0.0".into(),
            },
        );
        product.importers.insert(".".into(), importer);
        product
            .packages
            .insert("foo@1.0.0".into(), full.packages["foo@1.0.0"].clone());
        product
            .packages
            .insert("leaf@2.0.0".into(), full.packages["leaf@2.0.0"].clone());
        product.snapshots.insert(
            "foo@1.0.0".into(),
            object(&json!({"dependencies":{"leaf":"2.0.0"}}), "snapshot")
                .unwrap()
                .clone(),
        );
        product.snapshots.insert("leaf@2.0.0".into(), Fields::new());
        let seeds = |tooling: &Lock| {
            vec![
                crate::graph::LockSeed {
                    prefix: String::new(),
                    lock: serde_json::to_value(tooling).unwrap(),
                },
                crate::graph::LockSeed {
                    prefix: "apps/main".into(),
                    lock: serde_json::to_value(&product).unwrap(),
                },
            ]
        };
        crate::graph::seed_workspace_lock(root, &state, &seeds(&tooling)).unwrap();
        assert_eq!(
            read_yaml::<Lock>(root, "pnpm-lock.yaml").unwrap().snapshots["foo@1.0.0"]["dependencies"]
                ["leaf"],
            "2.0.0"
        );

        // A declared peer remains a product root when its physical installation
        // also appears in the managed development-tool subset.
        write_json(&root.join("packages/lib/package.json"), &json!({"name":"lib","version":"1.0.0","private":true,"peerDependencies":{"foo":"*"},"devDependencies":{"foo":"1.0.0"}})).unwrap();
        state
            .components
            .get_mut("packages/lib")
            .unwrap()
            .tool_dependencies = BTreeMap::from([("foo".into(), "1.0.0".into())]);
        let importer = tooling.importers.get_mut("packages/lib").unwrap();
        importer.dev_dependencies.clear();
        importer.dev_dependencies.insert(
            "foo".into(),
            Dependency {
                specifier: "1.0.0".into(),
                version: "1.0.0".into(),
            },
        );
        assert!(
            crate::graph::seed_workspace_lock(root, &state, &seeds(&tooling))
                .unwrap_err()
                .to_string()
                .contains("Conflicting product resolutions")
        );
        crate::graph::seed_workspace_lock_with_reconciliation(root, &state, &seeds(&tooling), true)
            .unwrap();
    }

    fn fixture(root: &Path) -> Lock {
        write_yaml(
            root,
            "pnpm-workspace.yaml",
            &Workspace::new(vec!["apps/web".into(), "packages/ui".into()]),
        )
        .unwrap();
        write_json(
            &root.join("package.json"),
            &json!({"name":"fixture","private":true}),
        )
        .unwrap();
        write_json(&root.join("apps/web/package.json"),&json!({"name":"web","private":true,
            "dependencies":{"react":"19.3.1","ui":"workspace:*"},"peerDependencies":{"react":"^19.0.0"}})).unwrap();
        write_json(
            &root.join("packages/ui/package.json"),
            &json!({"name":"ui","private":false,"version":"1.0.0"}),
        )
        .unwrap();
        let integrity = format!("sha512-{}", "A".repeat(86) + "==");
        let value = json!({"lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{},"apps/web":{"dependencies":{"react":{"specifier":"19.3.1","version":"19.3.1"},"ui":{"specifier":"workspace:*","version":"link:../../packages/ui"}}},"packages/ui":{}},
            "packages":{"react@19.3.1":{"resolution":{"integrity":integrity}}},"snapshots":{"react@19.3.1":{}}});
        Lock::from_value(&value).unwrap()
    }

    #[test]
    fn peers_resolve_from_physical_importer_fields_and_workspace_links_stay_local() {
        let temporary = TempDir::new().unwrap();
        let lock = fixture(temporary.path());
        lock.validate(temporary.path()).unwrap();
        let graph = lock.native_graph(temporary.path()).unwrap();
        let roots = &graph.roots["apps/web"];
        assert!(roots.iter().any(|edge| edge.field == "peerDependencies"
            && edge.spec == "^19.0.0"
            && edge.to.as_deref() == Some("react@19.3.1")));
        assert!(
            roots
                .iter()
                .any(|edge| edge.name == "ui" && edge.to.as_deref() == Some("packages/ui"))
        );
        assert_eq!(
            relative_link("apps/web", "packages/ui"),
            "../../packages/ui"
        );
        assert_eq!(
            package_parts("@scope/pkg@1.0.0(react@19.3.1)").unwrap(),
            ("@scope/pkg", "1.0.0")
        );
        assert!(package_parts("pkg@1.0.0(react@19.3.1").is_err());
    }

    #[test]
    fn parser_rejects_aliases_anchors_extra_workspace_settings_and_unsafe_protocols() {
        assert!(parse_yaml::<Value>(b"a: &anchor [1]\nb: *anchor\n").is_err());
        let workspace = b"packages: []\nnodeLinker: isolated\npackageImportMethod: hardlink\nverifyDepsBeforeRun: false\nstrictSsl: false\n";
        assert!(parse_yaml::<Workspace>(workspace).is_err());
        for spec in [
            "workspace:../../../outside",
            "workspace:../pkg",
            "npm:pkg@workspace:*",
            "workspace:latest",
        ] {
            assert!(native::parse_npm_dependency_spec(spec).is_err(), "{spec}");
        }
        for spec in [
            "workspace:*",
            "workspace:^",
            "workspace:~",
            "workspace:^1.2.3",
        ] {
            assert!(native::parse_npm_dependency_spec(spec).is_ok(), "{spec}");
        }
    }

    #[test]
    fn stale_specifiers_integrities_and_missing_snapshots_fail_closed() {
        let temporary = TempDir::new().unwrap();
        let lock = fixture(temporary.path());
        let mut stale = lock.clone();
        stale
            .importers
            .get_mut("apps/web")
            .unwrap()
            .dependencies
            .get_mut("react")
            .unwrap()
            .specifier = "19.0.0".into();
        assert!(
            stale
                .native_graph(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("Native lock is stale")
        );
        let mut corrupt = lock.clone();
        corrupt
            .packages
            .get_mut("react@19.3.1")
            .unwrap()
            .get_mut("resolution")
            .unwrap()["integrity"] = json!("sha512-bad");
        assert!(corrupt.validate(temporary.path()).is_err());
        let mut absent = lock;
        absent.snapshots.clear();
        assert!(absent.validate(temporary.path()).is_err());
    }
}

pub fn read_yaml<T: serde::de::DeserializeOwned>(root: &Path, name: &str) -> Result<T> {
    let path = input_path(root, name)?;
    ensure!(
        fs::metadata(&path)?.len() <= MAX_BYTES as u64,
        "pnpm YAML exceeds byte budget"
    );
    parse_yaml(&fs::read(path)?)
}

pub fn write_yaml(root: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    write_atomic(
        &safe_path(root, name)?,
        serde_saphyr::to_string(value)?.as_bytes(),
        0o644,
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub packages: Vec<String>,
    pub node_linker: String,
    pub package_import_method: String,
    pub verify_deps_before_run: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub patched_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only_built_dependencies: Vec<String>,
}
impl Workspace {
    pub fn new(packages: Vec<String>) -> Self {
        Self {
            packages,
            node_linker: "isolated".into(),
            package_import_method: "hardlink".into(),
            verify_deps_before_run: false,
            overrides: BTreeMap::new(),
            patched_dependencies: BTreeMap::new(),
            only_built_dependencies: vec![],
        }
    }
    pub fn validate(&self, root: &Path) -> Result<()> {
        ensure!(
            self.node_linker == "isolated"
                && self.package_import_method == "hardlink"
                && !self.verify_deps_before_run,
            "Unsupported pnpm workspace linking settings"
        );
        let mut seen = BTreeSet::new();
        for path in &self.packages {
            relative_path(path)?;
            ensure!(seen.insert(path), "Duplicate pnpm workspace member: {path}");
            input_path(root, path)?;
        }
        for (name, value) in &self.overrides {
            ensure!(!name.is_empty(), "Empty pnpm override selector");
            if let NpmDepSpec::LocalFile(path) = native::parse_npm_dependency_spec(value)? {
                input_path(root, path)?;
            }
        }
        for path in self.patched_dependencies.values() {
            let path = input_path(root, path)?;
            ensure!(
                fs::symlink_metadata(path)?.is_file(),
                "pnpm patch must be a regular checkout file"
            );
        }
        ensure!(
            self.only_built_dependencies
                .iter()
                .all(|n| native::valid_npm_package_name(n)),
            "Invalid onlyBuiltDependencies package name"
        );
        Ok(())
    }
    pub fn read(root: &Path) -> Result<Self> {
        let workspace: Self = read_yaml(root, "pnpm-workspace.yaml")?;
        workspace.validate(root)?;
        Ok(workspace)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Dependency {
    pub specifier: String,
    pub version: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Importer {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, Dependency>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, Dependency>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev_dependencies: BTreeMap<String, Dependency>,
}
impl Importer {
    pub fn field(&self, field: &str) -> &BTreeMap<String, Dependency> {
        match field {
            "dependencies" => &self.dependencies,
            "optionalDependencies" => &self.optional_dependencies,
            "devDependencies" => &self.dev_dependencies,
            _ => unreachable!("physical importer field"),
        }
    }
    pub fn field_mut(&mut self, field: &str) -> &mut BTreeMap<String, Dependency> {
        match field {
            "dependencies" => &mut self.dependencies,
            "optionalDependencies" => &mut self.optional_dependencies,
            "devDependencies" => &mut self.dev_dependencies,
            _ => unreachable!("physical importer field"),
        }
    }
    pub fn dependency(&self, name: &str) -> Option<&Dependency> {
        self.optional_dependencies
            .get(name)
            .or(self.dependencies.get(name))
            .or(self.dev_dependencies.get(name))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lock {
    pub lockfile_version: String,
    #[serde(default)]
    pub settings: Fields,
    pub importers: BTreeMap<String, Importer>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, Fields>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshots: BTreeMap<String, Fields>,
    #[serde(flatten)]
    pub extra: Fields,
}
impl Default for Lock {
    fn default() -> Self {
        Self {
            lockfile_version: "9.0".into(),
            settings: object(
                &json!({"autoInstallPeers":true,"excludeLinksFromLockfile":false}),
                "settings",
            )
            .unwrap()
            .clone(),
            importers: BTreeMap::new(),
            packages: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            extra: Fields::new(),
        }
    }
}

pub fn strip_peer_suffixes(key: &str) -> &str {
    key.split_once('(').map_or(key, |(base, _)| base)
}
pub fn package_parts(key: &str) -> Result<(&str, &str)> {
    let base = strip_peer_suffixes(key);
    let separator = base
        .get(1..)
        .and_then(|v| v.find('@'))
        .map(|i| i + 1)
        .context("Invalid pnpm package key")?;
    let (name, version) = (&base[..separator], &base[separator + 1..]);
    ensure!(
        native::valid_npm_package_name(name) && !version.is_empty(),
        "Invalid pnpm package key: {key}"
    );
    let mut depth = 0usize;
    for byte in key.as_bytes().iter().skip(base.len()) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                ensure!(depth > 0, "Invalid pnpm peer suffix");
                depth -= 1;
            }
            _ => {}
        }
    }
    ensure!(depth == 0, "Unbalanced pnpm peer suffix");
    Ok((name, version))
}

pub fn relative_link(from: &str, to: &str) -> String {
    let from: Vec<_> = from
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let to: Vec<_> = to
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path = vec![".."; from.len() - common];
    path.extend_from_slice(&to[common..]);
    if path.is_empty() {
        ".".into()
    } else {
        path.join("/")
    }
}

fn legacy_key(
    packages: &native::Packages,
    location: &str,
    visiting: &mut BTreeSet<String>,
    memo: &mut BTreeMap<String, String>,
) -> Result<String> {
    if let Some(key) = memo.get(location) {
        return Ok(key.clone());
    }
    ensure!(
        visiting.len() < 64,
        "Legacy peer graph exceeds depth budget"
    );
    let record = packages.get(location).context("Missing legacy package")?;
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| location.rsplit("node_modules/").next().unwrap_or(location));
    ensure!(
        native::valid_npm_package_name(name),
        "Invalid legacy package name: {name}"
    );
    let version = text(
        record
            .get("version")
            .context("Missing legacy package version")?,
        "legacy package version",
    )?;
    let source = record
        .get("resolved")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with("file:"))
        .unwrap_or(version);
    let mut key = format!("{name}@{source}");
    if visiting.insert(location.into()) {
        for peer in native::dependency_table(record, "peerDependencies")?.keys() {
            if let Some(target) = native::locked_dependency(packages, location, peer)?
                && target.contains("node_modules/")
            {
                let peer_key = legacy_key(packages, &target, visiting, memo)?;
                key.push('(');
                key.push_str(&peer_key);
                key.push(')');
            }
        }
        visiting.remove(location);
        memo.insert(location.into(), key.clone());
    }
    Ok(key)
}

fn reference_for(name: &str, key: &str) -> String {
    key.strip_prefix(&format!("{name}@"))
        .unwrap_or(key)
        .to_owned()
}

fn remap_environment(
    root: &Path,
    path: &str,
    specifier: &str,
    version: &str,
    lock: &mut Lock,
) -> Result<String> {
    let current = strip_peer_suffixes(version);
    let next = if let Some(file) = specifier.strip_prefix("file:") {
        format!("file:{}", native::local_path(root, path, file)?)
    } else {
        specifier.to_owned()
    };
    if current == next {
        return Ok(version.to_owned());
    }
    if !current.starts_with("file:") && !next.starts_with("file:") {
        return Ok(version.to_owned());
    }
    let old_key = format!("@docouno/env@{current}");
    let new_key = format!("@docouno/env@{next}");
    if let Some(mut package) = lock.packages.get(&old_key).cloned() {
        let mut resolution = package
            .get("resolution")
            .and_then(Value::as_object)
            .cloned()
            .context("Environment resolution")?;
        if let Some(file) = next.strip_prefix("file:") {
            use base64::Engine;
            use sha2::Digest;
            let bytes = fs::read(input_path(root, file)?)?;
            resolution.insert(
                "integrity".into(),
                json!(format!(
                    "sha512-{}",
                    base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&bytes))
                )),
            );
            resolution.insert("tarball".into(), json!(next));
            if !current.starts_with("file:") {
                package.insert("version".into(), json!(current));
            }
        } else {
            resolution.remove("tarball");
            package.remove("version");
        }
        package.insert("resolution".into(), json!(resolution));
        lock.packages.insert(new_key.clone(), package);
        let snapshots: Vec<_> = lock
            .snapshots
            .iter()
            .filter(|(key, _)| strip_peer_suffixes(key) == old_key)
            .map(|(key, value)| (format!("{new_key}{}", &key[old_key.len()..]), value.clone()))
            .collect();
        for (key, value) in snapshots {
            lock.snapshots.insert(key, value);
        }
    }
    Ok(format!("{next}{}", &version[current.len()..]))
}

pub fn refresh_importers(
    root: &Path,
    state: &crate::model::ProjectState,
    lock: &mut Lock,
) -> Result<()> {
    let mut names = BTreeMap::new();
    let mut manifests = BTreeMap::new();
    manifests.insert(String::new(), native::native_manifest(root, "")?);
    for (path, _) in crate::catalog::node_components(state) {
        let manifest = native::native_manifest(root, path)?;
        let name = text(
            manifest.get("name").context("Workspace name")?,
            "workspace name",
        )?
        .to_owned();
        ensure!(
            names.insert(name, path.clone()).is_none(),
            "Duplicate npm workspace name"
        );
        manifests.insert(path.clone(), manifest);
    }
    for (path, manifest) in manifests {
        let id = importer_id(&path).to_owned();
        let old = lock.importers.remove(&id).unwrap_or_default();
        let mut importer = Importer::default();
        for field in PHYSICAL_FIELDS {
            for (name, value) in native::dependency_table(&manifest, field)? {
                if field == "dependencies"
                    && native::dependency_table(&manifest, "optionalDependencies")?
                        .contains_key(&name)
                {
                    continue;
                }
                let specifier = text(&value, "dependency specifier")?.to_owned();
                let parsed = native::parse_npm_dependency_spec(&specifier)?;
                let previous = old
                    .field(field)
                    .get(&name)
                    .or_else(|| old.dependency(&name))
                    .filter(|dep| dep.specifier == specifier || name == "@docouno/env");
                let local = match &parsed {
                    NpmDepSpec::Workspace(_) => Some(
                        names
                            .get(&name)
                            .with_context(|| {
                                format!("Workspace dependency has no registered target: {name}")
                            })?
                            .clone(),
                    ),
                    NpmDepSpec::LocalFile(value) => {
                        let target = native::local_path(root, &path, value)?;
                        fs::symlink_metadata(input_path(root, &target)?)
                            .ok()
                            .filter(|m| m.is_dir() && state.components.contains_key(&target))
                            .map(|_| target)
                    }
                    _ => None,
                };
                let version = if matches!(parsed, NpmDepSpec::LocalFile(_))
                    && let Some(dep) = previous
                {
                    dep.version.clone()
                } else if let Some(target) = local {
                    format!("link:{}", relative_link(&path, &target))
                } else if let Some(dep) = previous {
                    dep.version.clone()
                } else {
                    let requested = match &parsed {
                        NpmDepSpec::Alias { target_name, .. } => target_name.as_str(),
                        _ => &name,
                    };
                    let Some(key) = lock.snapshots.keys().find(|key| {
                        package_parts(key).is_ok_and(|(package, version)| {
                            package == requested && version == specifier
                        })
                    }) else {
                        // A seed is intentionally incomplete for newly authored
                        // declarations. The ecosystem resolver selects them later.
                        continue;
                    };
                    reference_for(&name, key)
                };
                let version = if name == "@docouno/env" {
                    remap_environment(root, &path, &specifier, &version, lock)?
                } else {
                    version
                };
                importer
                    .field_mut(field)
                    .insert(name, Dependency { specifier, version });
            }
        }
        lock.importers.insert(id, importer);
    }
    Ok(())
}

/// Convert already selected npm v3 resolutions in memory. This never asks a
/// registry to resolve a range and keeps peer recursion bounded by the graph.
pub fn import_legacy_npm_lock_seed(root: &Path, packages: &native::Packages) -> Result<Lock> {
    import_legacy_npm_lock_seed_preserving(
        root,
        packages,
        &packages.keys().cloned().collect(),
        false,
    )
}

/// Collapse managed tooling variants while preserving every product-owned path.
/// Product graphs are independently compared again after ecosystem resolution.
pub(crate) fn import_legacy_npm_lock_seed_preserving(
    root: &Path,
    packages: &native::Packages,
    protected: &BTreeSet<String>,
    reconcile: bool,
) -> Result<Lock> {
    let mut lock = Lock::default();
    let mut keys = BTreeMap::new();
    let mut protected_snapshots = BTreeSet::new();
    let mut declarations = BTreeMap::new();
    for (location, record) in packages {
        if location.contains("node_modules/") && record.get("link") != Some(&Value::Bool(true)) {
            legacy_key(packages, location, &mut BTreeSet::new(), &mut keys)?;
        }
    }
    for (location, record) in packages {
        if record.get("link") == Some(&Value::Bool(true)) {
            continue;
        }
        if let Some(key) = keys.get(location) {
            let declared = ["dependencies", "optionalDependencies"]
                .iter()
                .map(|field| native::dependency_table(record, field))
                .collect::<Result<Vec<_>>>()?;
            if let Some(previous) =
                declarations.insert(strip_peer_suffixes(key).to_owned(), declared.clone())
            {
                ensure!(
                    previous == declared,
                    "Conflicting legacy dependency declarations: {key}"
                );
            }
            let mut resolution = Fields::new();
            for (source, target) in [("integrity", "integrity"), ("resolved", "tarball")] {
                if let Some(value) = record.get(source) {
                    resolution.insert(target.into(), value.clone());
                }
            }
            let mut package = Fields::new();
            package.insert("resolution".into(), json!(resolution));
            if strip_peer_suffixes(key).contains("@file:") {
                package.insert("version".into(), record["version"].clone());
            }
            for field in [
                "engines",
                "os",
                "cpu",
                "libc",
                "peerDependencies",
                "peerDependenciesMeta",
                "bin",
            ] {
                if let Some(value) = record.get(field) {
                    package.insert(field.into(), value.clone());
                }
            }
            if let Some(previous) = lock
                .packages
                .insert(strip_peer_suffixes(key).into(), package.clone())
            {
                ensure!(previous == package, "Conflicting legacy identity: {key}");
            }
            let mut snapshot = Fields::new();
            if record.get("optional") == Some(&Value::Bool(true)) {
                snapshot.insert("optional".into(), json!(true));
            }
            for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
                for dependency in native::dependency_table(record, field)?.keys() {
                    let target = native::locked_dependency(packages, location, dependency)?;
                    let optional = field == "optionalDependencies"
                        || (field == "peerDependencies"
                            && record
                                .get("peerDependenciesMeta")
                                .and_then(|m| m.get(dependency))
                                .and_then(|m| m.get("optional"))
                                == Some(&Value::Bool(true)));
                    let Some(target) = target else {
                        ensure!(
                            optional,
                            "Missing legacy dependency {location}:{dependency}"
                        );
                        continue;
                    };
                    let reference = if let Some(key) = keys.get(&target) {
                        reference_for(dependency, key)
                    } else {
                        format!("link:{target}")
                    };
                    let section = if optional {
                        "optionalDependencies"
                    } else {
                        "dependencies"
                    };
                    snapshot
                        .entry(section)
                        .or_insert(json!({}))
                        .as_object_mut()
                        .context("Snapshot field")?
                        .insert(dependency.clone(), json!(reference));
                }
            }
            let product = protected.contains(location);
            if let Some(previous) = lock.snapshots.get_mut(key) {
                merge_snapshot(
                    previous,
                    snapshot,
                    protected_snapshots.contains(key),
                    product,
                    reconcile,
                    key,
                )?;
            } else {
                lock.snapshots.insert(key.clone(), snapshot);
            }
            if product {
                protected_snapshots.insert(key.clone());
            }
        } else {
            let manifest = native::native_manifest(root, location)?;
            let mut importer = Importer::default();
            for field in PHYSICAL_FIELDS {
                for (name, value) in native::dependency_table(&manifest, field)? {
                    if field == "dependencies"
                        && native::dependency_table(&manifest, "optionalDependencies")?
                            .contains_key(&name)
                    {
                        continue;
                    }
                    let specifier = text(&value, "legacy dependency specifier")?.to_owned();
                    let mut target = native::locked_dependency(packages, location, &name)?;
                    if target.is_none() {
                        match native::parse_npm_dependency_spec(&specifier)? {
                            NpmDepSpec::Workspace(_) => {
                                for path in Workspace::read(root)?.packages {
                                    if native::native_manifest(root, &path)?
                                        .get("name")
                                        .and_then(Value::as_str)
                                        == Some(&name)
                                    {
                                        target = Some(path);
                                        break;
                                    }
                                }
                            }
                            NpmDepSpec::LocalFile(file) => {
                                let path = native::local_path(root, location, &file)?;
                                if fs::metadata(input_path(root, &path)?).is_ok_and(|m| m.is_dir())
                                {
                                    target = Some(path);
                                }
                            }
                            _ => {}
                        }
                    }
                    let Some(target) = target else {
                        ensure!(
                            field == "optionalDependencies",
                            "Missing legacy dependency {location}:{name}"
                        );
                        continue;
                    };
                    let version = if let Some(key) = keys.get(&target) {
                        reference_for(&name, key)
                    } else {
                        format!("link:{}", relative_link(location, &target))
                    };
                    importer
                        .field_mut(field)
                        .insert(name, Dependency { specifier, version });
                }
            }
            lock.importers
                .insert(importer_id(location).into(), importer);
        }
    }
    Ok(lock)
}
/// npm can install one artifact in several physical locations with different
/// compatible transitive versions; pnpm needs one snapshot per peer context.
pub(crate) fn merge_snapshot(
    previous: &mut Fields,
    next: Fields,
    previous_product: bool,
    next_product: bool,
    reconcile: bool,
    key: &str,
) -> Result<()> {
    let same = ["dependencies", "optionalDependencies"]
        .iter()
        .map(|field| {
            Ok(native::dependency_table(previous, field)?
                == native::dependency_table(&next, field)?)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .all(|same| same);
    ensure!(
        same || !previous_product || !next_product || reconcile,
        "Conflicting product resolutions for {key}; inspect and retry with --reconcile-lock"
    );
    let optional = previous.get("optional") == Some(&json!(true))
        && next.get("optional") == Some(&json!(true));
    if same {
        previous.extend(next);
    } else if next_product && !previous_product {
        *previous = next;
    }
    if optional {
        previous.insert("optional".into(), json!(true));
    } else {
        previous.remove("optional");
    }
    Ok(())
}

pub(crate) fn legacy_product_locations(
    root: &Path,
    state: &crate::model::ProjectState,
    packages: &native::Packages,
) -> Result<BTreeSet<String>> {
    let mut pending = Vec::new();
    for location in packages.keys().filter(|p| !p.contains("node_modules/")) {
        if !location.is_empty() && !state.components.contains_key(location) {
            continue;
        }
        let manifest = native::native_manifest(root, location)?;
        for field in native::DEPENDENCY_FIELDS {
            for name in native::dependency_table(&manifest, field)?.keys() {
                if field == "devDependencies"
                    && state
                        .components
                        .get(location)
                        .is_some_and(|c| c.tool_dependencies.contains_key(name))
                {
                    continue;
                }
                if let Some(target) = native::locked_dependency(packages, location, name)? {
                    pending.push(target);
                }
            }
        }
    }
    let mut protected = BTreeSet::new();
    while let Some(location) = pending.pop() {
        if !protected.insert(location.clone()) {
            continue;
        }
        let manifest;
        let record = if location.contains("node_modules/") {
            packages
                .get(&location)
                .context("Missing legacy product package")?
        } else {
            manifest = native::native_manifest(root, &location)?;
            &manifest
        };
        for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
            for name in native::dependency_table(record, field)?.keys() {
                if let Some(target) = native::locked_dependency(packages, &location, name)? {
                    pending.push(target);
                }
            }
        }
    }
    Ok(protected)
}

pub fn snapshot_key(name: &str, reference: &str) -> Result<String> {
    ensure!(
        native::valid_dependency_name(name),
        "Invalid pnpm dependency name"
    );
    let base = strip_peer_suffixes(reference);
    let key =
        if !base.starts_with("file:") && base[usize::from(base.starts_with('@'))..].contains('@') {
            reference.to_owned()
        } else {
            format!("{name}@{reference}")
        };
    package_parts(&key)?;
    Ok(key)
}
pub fn importer_id(path: &str) -> &str {
    if path.is_empty() { "." } else { path }
}
pub fn root_path(id: &str) -> &str {
    if id == "." { "" } else { id }
}

impl Lock {
    pub fn native_graph(&self, root: &Path) -> Result<crate::graph::NativeGraph> {
        self.graph(root, true)
    }
    pub fn retained_graph(&self, root: &Path) -> Result<crate::graph::NativeGraph> {
        self.graph(root, false)
    }
    fn graph(&self, root: &Path, strict: bool) -> Result<crate::graph::NativeGraph> {
        use crate::graph::{Edge, NativeGraph, Node};
        let mut nodes = BTreeMap::new();
        let mut roots = BTreeMap::new();
        for id in self.importers.keys() {
            let path = root_path(id);
            let manifest = native::native_manifest(root, path)?;
            if strict {
                self.validate_importer(root, path, &manifest)?;
            }
            let mut edges = Vec::new();
            for field in native::DEPENDENCY_FIELDS {
                for (name, value) in native::dependency_table(&manifest, field)? {
                    let spec = text(&value, "dependency specifier")?;
                    if !strict
                        && field != "peerDependencies"
                        && self.importers[id]
                            .field(field)
                            .get(&name)
                            .is_none_or(|d| d.specifier != spec)
                    {
                        continue;
                    }
                    let to = self.target(root, path, &manifest, field, (&name, spec), true)?;
                    edges.push(Edge {
                        field: field.into(),
                        name,
                        spec: spec.into(),
                        to,
                    });
                }
            }
            let mut identity = Fields::new();
            identity.insert("local".into(), json!(path));
            for key in ["name", "version"] {
                if let Some(value) = manifest.get(key) {
                    identity.insert(key.into(), value.clone());
                }
            }
            roots.insert(path.into(), edges.clone());
            nodes.insert(path.into(), Node { identity, edges });
        }
        for (key, snapshot) in &self.snapshots {
            let (name, version) = package_parts(key)?;
            let package = &self.packages[strip_peer_suffixes(key)];
            let resolution = object(&package["resolution"], "pnpm resolution")?;
            let mut identity = Fields::new();
            identity.insert("name".into(), json!(name));
            identity.insert(
                "version".into(),
                package.get("version").cloned().unwrap_or(json!(version)),
            );
            if let Some(integrity) = resolution.get("integrity") {
                identity.insert("integrity".into(), integrity.clone());
            }
            if let Some(tarball) = resolution
                .get("tarball")
                .and_then(Value::as_str)
                .filter(|v| v.starts_with("file:"))
            {
                identity.insert("resolved".into(), json!(tarball));
            }
            identity.insert(
                "optional".into(),
                json!(snapshot.get("optional") == Some(&Value::Bool(true))),
            );
            if let Some(directory) = resolution.get("directory").and_then(Value::as_str) {
                let path = native::local_path(root, "", directory)?;
                let manifest = native::native_manifest(root, &path)?;
                identity.clear();
                identity.insert("local".into(), json!(path));
                for field in ["name", "version"] {
                    if let Some(value) = manifest.get(field) {
                        identity.insert(field.into(), value.clone());
                    }
                }
            }
            let mut edges = Vec::new();
            for field in ["dependencies", "optionalDependencies"] {
                for (name, value) in native::dependency_table(snapshot, field)? {
                    let to = self.resolve(root, ".", &name, text(&value, "snapshot reference")?)?;
                    edges.push(Edge {
                        field: field.into(),
                        name,
                        spec: String::new(),
                        to: Some(to),
                    });
                }
            }
            nodes.insert(key.clone(), Node { identity, edges });
        }
        Ok(NativeGraph { nodes, roots })
    }
    pub fn prune_dev(&mut self) -> Result<()> {
        self.prune(false)
    }
    pub fn prune_unused(&mut self) -> Result<()> {
        self.prune(true)
    }
    fn prune(&mut self, include_dev: bool) -> Result<()> {
        let mut pending = Vec::new();
        for importer in self.importers.values_mut() {
            for field in PHYSICAL_FIELDS {
                if field == "devDependencies" && !include_dev {
                    continue;
                }
                for (name, dep) in importer.field(field) {
                    if !dep.version.starts_with("link:") {
                        pending.push(snapshot_key(name, &dep.version)?);
                    }
                }
            }
            if !include_dev {
                importer.dev_dependencies.clear();
            }
        }
        let mut retained = BTreeSet::new();
        while let Some(key) = pending.pop() {
            if !retained.insert(key.clone()) {
                continue;
            }
            let snapshot = self
                .snapshots
                .get(&key)
                .context("Missing retained pnpm snapshot")?;
            for field in ["dependencies", "optionalDependencies"] {
                for (name, value) in native::dependency_table(snapshot, field)? {
                    let reference = text(&value, "snapshot reference")?;
                    if !reference.starts_with("link:") {
                        pending.push(snapshot_key(&name, reference)?);
                    }
                }
            }
        }
        self.snapshots.retain(|key, _| retained.contains(key));
        let packages: BTreeSet<_> = retained
            .iter()
            .map(|s| strip_peer_suffixes(s).to_owned())
            .collect();
        self.packages.retain(|key, _| packages.contains(key));
        Ok(())
    }
    pub fn read(root: &Path) -> Result<Self> {
        let lock: Self = read_yaml(root, "pnpm-lock.yaml")?;
        lock.validate(root)?;
        Ok(lock)
    }
    pub fn from_value(value: &Value) -> Result<Self> {
        Ok(serde_json::from_value(value.clone())?)
    }
    pub fn write(&self, root: &Path) -> Result<()> {
        write_yaml(root, "pnpm-lock.yaml", self)
    }
    pub fn validate(&self, root: &Path) -> Result<()> {
        ensure!(
            self.lockfile_version == "9.0",
            "Expected pnpm lockfileVersion 9.0"
        );
        ensure!(
            self.settings.get("autoInstallPeers") != Some(&Value::Bool(false))
                && self.settings.get("excludeLinksFromLockfile") != Some(&Value::Bool(true)),
            "Unsupported pnpm lock settings"
        );
        ensure!(
            self.importers.contains_key("."),
            "Native lock omits root importer"
        );
        for (id, importer) in &self.importers {
            if id != "." {
                relative_path(id)?;
                input_path(root, id)?;
            }
            for field in PHYSICAL_FIELDS {
                for (name, dep) in importer.field(field) {
                    native::parse_npm_dependency_spec(&dep.specifier)?;
                    self.resolve(root, id, name, &dep.version)?;
                }
            }
        }
        for (key, package) in &self.packages {
            package_parts(key)?;
            ensure!(
                strip_peer_suffixes(key) == key,
                "Peer suffix in pnpm package identity"
            );
            let resolution = object(
                package
                    .get("resolution")
                    .context("Missing pnpm package resolution")?,
                "pnpm resolution",
            )?;
            if resolution.get("type").and_then(Value::as_str) == Some("directory") {
                let path = text(
                    resolution
                        .get("directory")
                        .context("Missing pnpm directory")?,
                    "pnpm directory",
                )?;
                ensure!(
                    fs::metadata(input_path(root, path)?)?.is_dir(),
                    "Missing local pnpm package directory"
                );
            } else {
                ensure!(
                    resolution
                        .get("integrity")
                        .and_then(Value::as_str)
                        .is_some_and(native::valid_integrity),
                    "Unprovable native artifact identity: {key}"
                );
                if let Some(tarball) = resolution.get("tarball") {
                    let tarball = text(tarball, "pnpm tarball")?;
                    ensure!(
                        native::valid_resolved_source(root, tarball, false),
                        "Unprovable pnpm tarball source: {key}"
                    );
                }
            }
        }
        for (key, snapshot) in &self.snapshots {
            package_parts(key)?;
            ensure!(
                self.packages.contains_key(strip_peer_suffixes(key)),
                "Native lock omits package identity: {key}"
            );
            for field in ["dependencies", "optionalDependencies"] {
                for (name, reference) in native::dependency_table(snapshot, field)? {
                    self.resolve(
                        root,
                        ".",
                        &name,
                        text(&reference, "pnpm snapshot reference")?,
                    )?;
                }
            }
        }
        Ok(())
    }
    pub fn resolve(&self, root: &Path, id: &str, name: &str, reference: &str) -> Result<String> {
        if let Some(path) = reference.strip_prefix("link:") {
            let path = native::local_path(root, root_path(id), path)?;
            native::native_manifest(root, &path)?;
            ensure!(
                self.importers.contains_key(importer_id(&path)),
                "Local pnpm link has no importer; use a file directory snapshot: {path}"
            );
            return Ok(path);
        }
        let key = snapshot_key(name, reference)?;
        ensure!(
            self.snapshots.contains_key(&key)
                && self.packages.contains_key(strip_peer_suffixes(&key)),
            "Native lock target is missing: {key}"
        );
        Ok(key)
    }
    pub fn target(
        &self,
        root: &Path,
        path: &str,
        manifest: &Fields,
        field: &str,
        dependency: (&str, &str),
        verify_spec: bool,
    ) -> Result<Option<String>> {
        let (name, spec) = dependency;
        let parsed = native::parse_npm_dependency_spec(spec)?;
        let id = importer_id(path);
        let importer = self
            .importers
            .get(id)
            .with_context(|| format!("Native lock omits workspace: {path}"))?;
        let entry = if field == "peerDependencies" {
            importer
                .dependency(name)
                .or_else(|| self.importers.get(".").and_then(|i| i.dependency(name)))
        } else if field == "dependencies" && importer.optional_dependencies.contains_key(name) {
            importer.optional_dependencies.get(name)
        } else {
            importer.field(field).get(name)
        };
        let optional = field == "optionalDependencies"
            || (field == "peerDependencies"
                && manifest
                    .get("peerDependenciesMeta")
                    .and_then(|m| m.get(name))
                    .and_then(|m| m.get("optional"))
                    == Some(&Value::Bool(true)));
        let Some(entry) = entry else {
            ensure!(
                optional,
                "Native lock omits dependency {path}:{name}; run 'devkit sync && make setup'"
            );
            return Ok(None);
        };
        if verify_spec
            && field != "peerDependencies"
            && !(field == "dependencies" && importer.optional_dependencies.contains_key(name))
        {
            ensure!(
                entry.specifier == spec,
                "Native lock is stale: {path}:{name}; run 'devkit sync && make setup'"
            );
        }
        let reference_id = if field == "peerDependencies" && importer.dependency(name).is_none() {
            "."
        } else {
            id
        };
        let target = self.resolve(root, reference_id, name, &entry.version)?;
        if let NpmDepSpec::LocalFile(relative) = &parsed {
            let expected = native::local_path(root, path, relative)?;
            let actual = if entry.version.starts_with("link:") {
                target.clone()
            } else {
                let resolution = object(
                    &self.packages[strip_peer_suffixes(&target)]["resolution"],
                    "Local package resolution",
                )?;
                if let Some(directory) = resolution.get("directory").and_then(Value::as_str) {
                    native::local_path(root, "", directory)?
                } else {
                    let tarball = resolution
                        .get("tarball")
                        .and_then(Value::as_str)
                        .and_then(|url| url.strip_prefix("file:"))
                        .context("Local package must resolve to its declared file")?;
                    native::local_path(root, "", tarball)?
                }
            };
            ensure!(
                actual == expected,
                "Local dependency resolves to a different path: {path}:{name}"
            );
        }
        if matches!(parsed, NpmDepSpec::Workspace(_)) {
            ensure!(
                entry.version.starts_with("link:"),
                "Workspace dependency must resolve to a local importer: {path}:{name}"
            );
            ensure!(
                Workspace::read(root)?.packages.contains(&target),
                "Workspace dependency has no registered target: {path}:{name}"
            );
        }
        if entry.version.starts_with("link:") {
            ensure!(
                !matches!(parsed, NpmDepSpec::Alias { .. }),
                "Alias cannot resolve to workspace package"
            );
            let target_manifest = native::native_manifest(root, &target)?;
            ensure!(
                target_manifest.get("name").and_then(Value::as_str) == Some(name),
                "Workspace dependency name differs from target: {path}:{name}"
            );
        } else if let NpmDepSpec::Alias { target_name, .. } = parsed {
            ensure!(
                package_parts(&target)?.0 == target_name,
                "Alias dependency resolves to mismatched package"
            );
        } else if matches!(parsed, NpmDepSpec::RegistryRange(_)) {
            ensure!(
                package_parts(&target)?.0 == name,
                "Registry dependency resolves to mismatched package"
            );
        }
        Ok(Some(target))
    }
    pub fn validate_importer(&self, root: &Path, path: &str, manifest: &Fields) -> Result<()> {
        let importer = self
            .importers
            .get(importer_id(path))
            .context("Native lock omits workspace importer")?;
        for field in PHYSICAL_FIELDS {
            let mut expected = native::dependency_table(manifest, field)?;
            if field == "dependencies" {
                for key in native::dependency_table(manifest, "optionalDependencies")?.keys() {
                    expected.remove(key);
                }
            }
            let expected_len = expected.len();
            for (name, value) in expected {
                self.target(
                    root,
                    path,
                    manifest,
                    field,
                    (&name, text(&value, "dependency specifier")?),
                    true,
                )?;
            }
            ensure!(
                expected_len == importer.field(field).len(),
                "Native lock is stale: {path}:{field}; run 'devkit sync && make setup'"
            );
        }
        for (name, value) in native::dependency_table(manifest, "peerDependencies")? {
            self.target(
                root,
                path,
                manifest,
                "peerDependencies",
                (&name, text(&value, "peer specifier")?),
                true,
            )?;
        }
        Ok(())
    }
}
