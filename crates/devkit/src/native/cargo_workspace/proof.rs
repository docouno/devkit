//! Bind Cargo's accepted selections to the local inputs of its resolver.
use super::*;
use crate::model::{CargoResolution, CargoResolutionInputs};

impl CargoResolutionInputs {
    pub(crate) fn proof(&self, lock: &[u8]) -> CargoResolution {
        CargoResolution {
            version: 1,
            lock: digest(lock),
            inputs: self.clone(),
            extra: Default::default(),
        }
    }
}

impl CargoResolution {
    pub(crate) fn lock_matches(&self, bytes: &[u8]) -> bool {
        self.version == 1 && self.lock == digest(bytes)
    }

    pub(crate) fn matches(&self, inputs: &CargoResolutionInputs, bytes: &[u8]) -> bool {
        self.inputs == *inputs && self.lock_matches(bytes)
    }

    pub(crate) fn constructive_inputs_match(
        &self,
        inputs: &CargoResolutionInputs,
        added: &BTreeSet<String>,
    ) -> bool {
        self.version == 1
            && self.inputs.policy == inputs.policy
            && self.inputs.locals == inputs.locals
            && self
                .inputs
                .members
                .iter()
                .all(|(path, before)| inputs.members.get(path) == Some(before))
            && inputs.members.keys().all(|path| {
                self.inputs.members.contains_key(path)
                    || added.iter().any(|dir| Path::new(path).starts_with(dir))
            })
    }
}

fn hash_value(mut value: Value) -> Result<String> {
    // serde_json preserves insertion order elsewhere in Devkit. Cargo table
    // order and comments have no bearing on the selected packages.
    value.sort_all_objects();
    Ok(digest(serde_json::to_vec(&value)?))
}

fn strip_descriptive_fields(manifest: &mut Fields) {
    for field in ["badges", "lints", "profile"] {
        manifest.remove(field);
    }
    for field in ["package", "workspace"] {
        let Some(table) = manifest.get_mut(field).and_then(Value::as_object_mut) else {
            continue;
        };
        let package = if field == "workspace" {
            table.remove("metadata");
            table.remove("lints");
            table.get_mut("package").and_then(Value::as_object_mut)
        } else {
            Some(table)
        };
        if let Some(package) = package {
            for key in [
                "authors",
                "categories",
                "description",
                "documentation",
                "homepage",
                "include",
                "exclude",
                "keywords",
                "license",
                "license-file",
                "metadata",
                "publish",
                "readme",
                "repository",
            ] {
                package.remove(key);
            }
        }
    }
}

fn recipe_fingerprint(mut manifest: Fields) -> Result<String> {
    if let Some(package) = manifest.get_mut("package").and_then(Value::as_object_mut) {
        package.remove("name");
        package.remove("version");
    }
    hash_value(Value::Object(manifest))
}

pub(crate) fn cargo_template_recipe(bytes: &[u8]) -> Result<String> {
    let manifest: toml::Value = toml::from_str(std::str::from_utf8(bytes)?)?;
    let mut manifest = object(&serde_json::to_value(manifest)?, "Cargo template manifest")?.clone();
    strip_descriptive_fields(&mut manifest);
    if manifest
        .get("dependencies")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        manifest.remove("dependencies");
    }
    recipe_fingerprint(manifest)
}

/// The only wire which may be omitted from a member's policy fingerprint is
/// an unconditional, required dependency on a featureless member. It cannot
/// change the features selected on another package. A duplicate declaration
/// of the same target, or a feature expression in the caller, keeps the wire
/// in the fingerprint.
fn plain_wires(member: &CargoMember, manifest: &Fields) -> BTreeMap<String, String> {
    let mut wires = BTreeMap::new();
    if manifest.contains_key("features") {
        return wires;
    }
    let Some(deps) = manifest.get("dependencies").and_then(Value::as_object) else {
        return wires;
    };
    let mut declarations = BTreeMap::new();
    for dep in &member.dependencies {
        for name in std::iter::once(&dep.alias).chain((dep.name != dep.alias).then_some(&dep.name))
        {
            declarations
                .entry(name.as_str())
                .and_modify(|slot| *slot = None)
                .or_insert(dep.path.as_ref());
        }
    }
    for (name, value) in deps {
        let Some(fields) = value.as_object() else {
            continue;
        };
        if fields.len() != 1 || !fields.get("path").is_some_and(Value::is_string) {
            continue;
        }
        if let Some(Some(path)) = declarations.get(name.as_str()) {
            wires.insert(name.clone(), (*path).clone());
        }
    }
    wires
}

fn valid_crate_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_rust_version(raw: &str) -> bool {
    let parts: Vec<_> = raw.split('.').collect();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Only allow offline constructive lock synthesis when the member manifest uses
/// schema keys and values we have explicitly verified against Cargo rules.
fn valid_featureless_member_manifest(member: &CargoMember, manifest: &Fields) -> bool {
    if !valid_crate_name(&member.id.name) || Version::parse(&member.id.version).is_err() {
        return false;
    }
    if manifest.keys().any(|key| {
        !matches!(
            key.as_str(),
            "package"
                | "dependencies"
                | "dev-dependencies"
                | "build-dependencies"
                | "dev_dependencies"
                | "build_dependencies"
                | "target"
                | "lib"
                | "bin"
                | "test"
                | "bench"
                | "example"
                | "lints"
                | "badges"
                | "profile"
        )
    }) {
        return false;
    }
    let Some(package) = manifest.get("package").and_then(Value::as_object) else {
        return false;
    };
    if let Some(edition) = package.get("edition") {
        let ok = edition
            .as_str()
            .is_some_and(|e| matches!(e, "2015" | "2018" | "2021" | "2024"))
            || edition.get("workspace") == Some(&Value::Bool(true));
        if !ok {
            return false;
        }
    }
    if let Some(rv) = package.get("rust-version") {
        let ok = rv.as_str().is_some_and(valid_rust_version)
            || rv.get("workspace") == Some(&Value::Bool(true));
        if !ok {
            return false;
        }
    }
    if let Some(lib) = manifest.get("lib")
        && !lib
            .as_object()
            .is_some_and(|table| table.get("proc-macro").is_none_or(Value::is_boolean))
    {
        return false;
    }
    true
}

/// Identify dependency-free leaves, then their ordinary path-only parents.
/// A cycle or a dependency outside the supplied package set stays unresolved.
fn featureless_members(
    members: &BTreeMap<&str, &CargoMember>,
    manifests: &BTreeMap<String, Fields>,
    wires: &BTreeMap<&str, BTreeMap<String, String>>,
) -> BTreeSet<String> {
    let mut parents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut outstanding = BTreeMap::new();
    let mut queue = VecDeque::new();
    for (&path, member) in members {
        let manifest = &manifests[path];
        if manifest.contains_key("features")
            || manifest.contains_key("cargo-features")
            || manifest
                .get("package")
                .is_some_and(|p| p.get("links").is_some())
            || wires[path].len() != member.dependencies.len()
            || !valid_featureless_member_manifest(member, manifest)
        {
            continue;
        }
        if wires[path].is_empty() {
            queue.push_back(path);
        } else {
            outstanding.insert(path, wires[path].len());
            for target in wires[path].values() {
                parents.entry(target).or_default().push(path);
            }
        }
    }
    let mut known = BTreeSet::new();
    while let Some(leaf) = queue.pop_front() {
        known.insert(leaf.to_owned());
        for &parent in parents.get(leaf).into_iter().flatten() {
            let remaining = outstanding.get_mut(parent).expect("path-only parent");
            *remaining -= 1;
            if *remaining == 0 {
                queue.push_back(parent);
            }
        }
    }
    known
}

// A local lock graph proves neither Cargo's root manifest schema nor its
// workspace settings. Only compose locks for workspace options whose
// values we check here or during membership/override admission.
fn local_workspace_policy(root: &Fields) -> bool {
    if root
        .keys()
        .any(|key| !matches!(key.as_str(), "workspace" | "patch"))
    {
        return false;
    }
    if let Some(patches) = root.get("patch")
        && !patches.as_object().is_some_and(|origins| {
            origins.values().all(|table| {
                table
                    .as_object()
                    .is_some_and(|deps| deps.values().all(plain_path_dependency))
            })
        })
    {
        return false;
    }
    root.get("workspace")
        .and_then(Value::as_object)
        .is_some_and(|workspace| {
            workspace.iter().all(|(key, value)| match key.as_str() {
                "members" | "exclude" | "default-members" => true,
                "resolver" => matches!(value.as_str(), Some("1" | "2" | "3")),
                "metadata" => value.is_object(),
                "package" => value.as_object().is_some_and(|package| {
                    package.iter().all(|(key, value)| match key.as_str() {
                        "version" => value.as_str().is_some_and(|v| Version::parse(v).is_ok()),
                        "edition" => {
                            matches!(value.as_str(), Some("2015" | "2018" | "2021" | "2024"))
                        }
                        "rust-version" => value.as_str().is_some_and(valid_rust_version),
                        "license" | "license-file" | "description" | "documentation"
                        | "homepage" | "repository" => value.is_string(),
                        "readme" | "publish" => value.is_string() || value.is_boolean(),
                        "authors" | "categories" | "keywords" => value
                            .as_array()
                            .is_some_and(|items| items.iter().all(Value::is_string)),
                        _ => false,
                    })
                }),
                "dependencies" => value
                    .as_object()
                    .is_some_and(|deps| deps.values().all(plain_path_dependency)),
                "lints" => value.as_object().is_some_and(|tools| {
                    tools.iter().all(|(name, levels)| {
                        matches!(name.as_str(), "rust" | "clippy" | "rustdoc")
                            && levels.as_object().is_some_and(|levels| {
                                levels.values().all(|level| {
                                    matches!(
                                        level.as_str(),
                                        Some("allow" | "warn" | "deny" | "forbid")
                                    ) || level.as_object().is_some_and(|cfg| {
                                        cfg.get("level").and_then(Value::as_str).is_some_and(
                                            |lvl| {
                                                matches!(lvl, "allow" | "warn" | "deny" | "forbid")
                                            },
                                        ) && cfg.iter().all(|(k, v)| match k.as_str() {
                                            "level" => true,
                                            "priority" => v.as_i64().is_some(),
                                            _ => false,
                                        })
                                    })
                                })
                            })
                    })
                }),
                _ => false,
            })
        })
}

fn plain_path_dependency(value: &Value) -> bool {
    value.as_object().is_some_and(|dep| {
        dep.get("path").is_some_and(Value::is_string)
            && dep.iter().all(|(key, value)| match key.as_str() {
                "path" | "package" => value.is_string(),
                "version" => value
                    .as_str()
                    .is_some_and(|v| semver::VersionReq::parse(v).is_ok()),
                _ => false,
            })
    })
}

impl ReadContext<'_> {
    pub(super) fn fingerprint_resolution(
        &self,
        state: &ProjectState,
        model: &CargoWorkspaceModel,
    ) -> Result<(CargoResolutionInputs, bool, bool)> {
        let mut root = self.cargo[""].clone();
        if let Some(workspace) = root.get_mut("workspace").and_then(Value::as_object_mut) {
            workspace.remove("members");
        }
        // Check local_workspace_policy on the unstripped root manifest before
        // stripping descriptive fields for the canonical persisted digest.
        let mut configs = Vec::new();
        for path in &self.cargo_config_order {
            let mut settings = self.cargo_configs[path].clone();
            settings.remove("include");
            settings.remove("term");
            if !settings.is_empty() {
                configs.push((path, settings));
            }
        }
        let local_policy = local_workspace_policy(&root) && configs.is_empty();
        let recipe_workspace = Value::Object(root.clone())
            == json!({"workspace": {"resolver": "3"}})
            && configs.is_empty();
        // Strip root descriptive metadata ONLY when local_workspace_policy
        // already strictly type-checked every root key (so invalid [profile.*],
        // [workspace.lints], or [workspace.package] schemas always perturb
        // inputs.policy and trigger Cargo validation).
        if local_policy
            && let Some(workspace) = root.get_mut("workspace").and_then(Value::as_object_mut)
        {
            workspace.remove("metadata");
            workspace.remove("lints");
            if let Some(package) = workspace.get_mut("package").and_then(Value::as_object_mut) {
                for key in [
                    "authors",
                    "categories",
                    "description",
                    "documentation",
                    "homepage",
                    "include",
                    "exclude",
                    "keywords",
                    "license",
                    "license-file",
                    "publish",
                    "readme",
                    "repository",
                ] {
                    package.remove(key);
                }
            }
        }
        let mut policy = json!([
            root,
            configs,
            state
                .managed
                .get("rust-toolchain.toml")
                .map(|id| &id.sha256)
        ]);
        let parts = policy.as_array_mut().expect("policy tuple");
        if !model.default_members.is_empty() {
            parts.push(json!(model.default_members));
        }
        let mut inputs = CargoResolutionInputs {
            policy: hash_value(policy)?,
            ..Default::default()
        };
        let members: BTreeSet<_> = model
            .components
            .values()
            .flatten()
            .map(|m| m.rel_dir.as_str())
            .collect();
        let mut exact = BTreeMap::new();
        let mut locals = BTreeMap::new();
        for (path, source) in &self.cargo {
            if path.is_empty() {
                continue;
            }
            let mut manifest = source.clone();
            strip_descriptive_fields(&mut manifest);
            let fingerprint = hash_value(Value::Object(manifest))?;
            if members.contains(path.as_str()) {
                exact.insert(path, fingerprint);
            } else {
                locals.insert(path, fingerprint);
            }
        }
        if !model.conditional_paths.is_empty() {
            inputs.local_selections = model.selected_witnesses.clone();
        }
        inputs.locals = if let Some(selected) = &inputs.local_selections {
            hash_value(json!([locals, selected]))?
        } else {
            hash_value(json!(locals))?
        };
        inputs.digest = hash_value(json!([inputs.policy, exact, inputs.locals]))?;
        Ok((inputs, recipe_workspace, local_policy))
    }

    pub(super) fn bind_resolution_inputs(
        &self,
        state: &ProjectState,
        model: &mut CargoWorkspaceModel,
    ) -> Result<()> {
        let (mut inputs, recipe_workspace, local_policy) =
            self.fingerprint_resolution(state, model)?;
        model.local_workspace_policy = local_policy;
        model.recipe_workspace = recipe_workspace;
        let members: BTreeMap<_, _> = model
            .components
            .values()
            .flatten()
            .map(|m| (m.rel_dir.as_str(), m))
            .collect();
        let all: BTreeMap<_, _> = model
            .local_packages
            .iter()
            .map(|(path, m)| (path.as_str(), m))
            .collect();
        let wires: BTreeMap<_, _> = all
            .iter()
            .map(|(&path, &member)| (path, plain_wires(member, &self.cargo[path])))
            .collect();
        let featureless = featureless_members(&members, &self.cargo, &wires);
        model.simple_local_graph = if all.len() == members.len() {
            featureless.len() == members.len()
        } else {
            let simple = featureless_members(&all, &self.cargo, &wires);
            simple.len() == all.len()
        };
        for (&path, member) in &members {
            let source = &self.cargo[path];
            let mut manifest = source.clone();
            strip_descriptive_fields(&mut manifest);
            let mut stripped = 0;
            if let Some(deps) = manifest
                .get_mut("dependencies")
                .and_then(Value::as_object_mut)
            {
                for (name, target) in &wires[path] {
                    if featureless.contains(target) {
                        deps.remove(name);
                        stripped += 1;
                    }
                }
                if deps.is_empty() {
                    manifest.remove("dependencies");
                }
            }
            if !source
                .get("package")
                .is_some_and(|p| p.get("links").is_some())
                && stripped
                    == member
                        .dependencies
                        .iter()
                        .filter(|d| d.path.is_some())
                        .count()
            {
                model
                    .member_recipes
                    .insert(path.to_owned(), recipe_fingerprint(manifest.clone())?);
            }
            inputs
                .members
                .insert(path.to_owned(), hash_value(Value::Object(manifest))?);
        }
        model.inert_members = featureless;
        model.resolution_inputs = inputs;
        Ok(())
    }
}
