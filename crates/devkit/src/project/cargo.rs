//! Project-owned Cargo manifests and release-owned seed material in the staged checkout.
use super::*;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

pub(super) const AUTHORITIES: [&str; 4] = [
    "Cargo.toml",
    "Cargo.lock",
    "rustfmt.toml",
    "rust-toolchain.toml",
];

pub(super) fn has_rust(state: &ProjectState) -> bool {
    state
        .components
        .values()
        .any(|c| c.toolchain == Toolchain::Rust)
}

fn document(bytes: &[u8], name: &str) -> Result<DocumentMut> {
    std::str::from_utf8(bytes)?
        .parse::<DocumentMut>()
        .with_context(|| format!("Invalid TOML in {name}"))
}

pub(super) struct TemplateNames {
    source: String,
    generated: String,
}

impl TemplateNames {
    pub(super) fn read(
        release: &AdmittedRelease,
        role: Role,
        spec: &ComponentSpec,
    ) -> Result<Self> {
        let manifest = format!("templates/{}/Cargo.toml", template(&spec.template)?.id);
        let source = document(&release.bytes(&manifest)?, &manifest)?;
        let original = source
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(Item::as_str)
            .context("Template Cargo package name")?;
        let prefix = if role == Role::App { "app" } else { "lib" };
        Ok(Self {
            source: original.to_owned(),
            generated: format!("{prefix}-{}", spec.name),
        })
    }

    pub(super) fn render(&self, file: &str, bytes: Vec<u8>) -> Result<Vec<u8>> {
        if file == "Cargo.toml" {
            let mut manifest = document(&bytes, file)?;
            manifest["package"]["name"] = value(self.generated.as_str());
            return Ok(manifest.to_string().into_bytes());
        }
        if file == "Cargo.lock" {
            let mut seed = document(&bytes, file)?;
            let entry = seed
                .get_mut("package")
                .and_then(Item::as_array_of_tables_mut)
                .and_then(|packages| {
                    packages.iter_mut().find(|p| {
                        !p.contains_key("source")
                            && p.get("name").and_then(Item::as_str) == Some(self.source.as_str())
                    })
                })
                .context("Missing template crate in release Cargo.lock seed")?;
            entry["name"] = value(self.generated.as_str());
            return Ok(seed.to_string().into_bytes());
        }
        if file.ends_with(".rs") {
            return Ok(String::from_utf8(bytes)?
                .replace(
                    &self.source.replace('-', "_"),
                    &self.generated.replace('-', "_"),
                )
                .replace(&self.source, &self.generated)
                .into_bytes());
        }
        Ok(bytes)
    }
}

pub(super) fn add_use_dependencies(
    root: &Path,
    path: &str,
    uses: &[(String, ToolingProfile, String)],
) -> Result<()> {
    if uses.is_empty() {
        return Ok(());
    }
    let manifest = input_path(root, format!("{path}/Cargo.toml"))?;
    let mut doc = document(&read_input_bytes(&manifest)?, &format!("{path}/Cargo.toml"))?;
    let deps = doc
        .entry("dependencies")
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .context("Invalid Cargo dependencies table")?;
    for (dep, _, name) in uses {
        ensure!(
            !deps.contains_key(name),
            "Cargo dependency {name} already exists in {path}/Cargo.toml"
        );
        let mut spec = InlineTable::new();
        spec.insert("path", local_relative(path, dep).into());
        deps.insert(name, value(spec));
    }
    write_atomic(&manifest, doc.to_string().as_bytes(), 0o644)
}

pub(super) fn add_members(
    root: &Path,
    paths: &[String],
    allow_create: bool,
) -> Result<BTreeSet<PathBuf>> {
    let manifest = input_path(root, "Cargo.toml")?;
    let loaded = read_regular_file(&manifest, NATIVE_METADATA_MAX_BYTES, "Cargo manifest")?;
    let (old, original) = if let Some((bytes, identity)) = loaded {
        (bytes, Some(identity))
    } else {
        ensure!(allow_create, "Missing root Cargo.toml");
        (
            b"[workspace]\nresolver = \"3\"\nmembers = []\n".to_vec(),
            None,
        )
    };
    let mut doc = document(&old, "Cargo.toml")?;
    ensure!(
        !doc.contains_key("package"),
        "Root Cargo.toml must be a virtual workspace"
    );
    let ws = doc
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .context("Root Cargo.toml requires [workspace]")?;
    let members = ws
        .entry("members")
        .or_insert(value(Array::new()))
        .as_array_mut()
        .context("workspace.members must be an array in root Cargo.toml")?;
    let mut listed = BTreeSet::new();
    for member in members.iter() {
        let path = member
            .as_str()
            .context("workspace.members entries must be strings")?;
        listed.insert(input_path(root, path)?);
    }
    for path in paths {
        if listed.insert(input_path(root, path)?) {
            members.push(path.as_str());
        }
    }
    let updated = doc.to_string();
    if original.is_none() || updated.as_bytes() != old {
        write_atomic(
            &manifest,
            updated.as_bytes(),
            original.map_or(0o644, |id| id.mode),
        )?;
    }
    Ok(listed)
}

/// Return explicit root `workspace.members` paths already under `component`, if any.
pub(super) fn listed_members(root: &Path, component: &str) -> Result<Vec<String>> {
    let manifest = input_path(root, "Cargo.toml")?;
    let Some((bytes, _)) =
        read_regular_file(&manifest, NATIVE_METADATA_MAX_BYTES, "Cargo manifest")?
    else {
        return Ok(Vec::new());
    };
    let doc = document(&bytes, "Cargo.toml")?;
    let Some(members) = doc
        .get("workspace")
        .and_then(Item::as_table_like)
        .and_then(|ws| ws.get("members"))
        .and_then(Item::as_array)
    else {
        return Ok(Vec::new());
    };
    let root = crate::files::project_root(root)?;
    let mut matched = Vec::new();
    for entry in members.iter() {
        let raw = entry
            .as_str()
            .context("workspace.members entries must be strings")?;
        let normalized = relative_name(&root, &input_path(&root, raw)?)?;
        if Path::new(&normalized).starts_with(component) {
            let member_manifest = input_path(&root, format!("{normalized}/Cargo.toml"))?;
            ensure!(
                exists(&member_manifest)?,
                "Listed Cargo workspace member is missing {normalized}/Cargo.toml"
            );
            let member_doc = document(
                &read_input_bytes(&member_manifest)?,
                &format!("{normalized}/Cargo.toml"),
            )?;
            ensure!(
                !member_doc.contains_key("workspace"),
                "Move the members of {normalized}/Cargo.toml into root Cargo.toml and remove the nested [workspace] before registering"
            );
            ensure!(
                member_doc.contains_key("package"),
                "Expected [package] in {normalized}/Cargo.toml"
            );
            matched.push(normalized);
        }
    }
    matched.sort();
    matched.dedup();
    Ok(matched)
}

/// Never follow directory links or look for package manifests in generated outputs.
pub(super) fn discover_members(root: &Path, component: &str) -> Result<Vec<String>> {
    let root = crate::files::project_root(root)?;
    let mut pending = vec![input_path(&root, component)?];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        let rel = relative_name(&root, &dir)?;
        let path = input_path(&root, format!("{rel}/Cargo.toml"))?;
        if exists(&path)? {
            let doc = document(&read_input_bytes(&path)?, &format!("{rel}/Cargo.toml"))?;
            ensure!(
                !doc.contains_key("workspace"),
                "Move the members of {rel}/Cargo.toml into root Cargo.toml and remove the nested [workspace] before registering"
            );
            ensure!(
                doc.contains_key("package"),
                "Expected [package] in {rel}/Cargo.toml"
            );
            if rel == component {
                return Ok(vec![rel]);
            }
            found.push(rel);
            continue;
        }
        for entry in sorted_entries(&dir)?.into_iter().rev() {
            if entry.file_type()?.is_dir()
                && !matches!(
                    entry.file_name().to_str(),
                    Some(
                        ".git"
                            | ".devkit"
                            | ".artifacts"
                            | ".worktrees"
                            | "target"
                            | "node_modules"
                            | "tests"
                            | "benches"
                            | "examples"
                    )
                )
            {
                pending.push(entry.path());
            }
        }
    }
    ensure!(!found.is_empty(), "No Cargo package found in {component}");
    Ok(found)
}

pub(super) fn reconcile_members(root: &Path, state: &ProjectState) -> Result<()> {
    let root = crate::files::project_root(root)?;
    let listed = add_members(&root, &[], false)?;
    let proof = state.cargo_resolution.as_ref().filter(|p| p.version == 1);
    let mut restored = Vec::new();
    for (component, spec) in &state.components {
        let component_dir = root.join(component);
        let listed_here = listed
            .range(component_dir.clone()..)
            .next()
            .is_some_and(|path| path.starts_with(&component_dir));
        if spec.toolchain != Toolchain::Rust || listed_here {
            continue;
        }
        let guidance =
            "list the intended paths in workspace.members in root Cargo.toml and run make sync";

        // A lock identifies packages by name, version and source, without
        // recording whether the registered crate lived at the component root.
        let mut previous = proof
            .into_iter()
            .flat_map(|record| record.inputs.members.keys())
            .filter(|path| Path::new(path).starts_with(component))
            .peekable();
        ensure!(
            previous.peek().is_some(),
            "No recorded Cargo member paths for {component}; {guidance}"
        );
        for member in previous {
            let name = format!("{member}/Cargo.toml");
            let manifest = input_path(&root, &name).with_context(|| {
                format!("Cannot restore accepted Cargo member {member}; {guidance}")
            })?;
            ensure!(
                manifest.starts_with(&component_dir)
                    && relative_name(&root, &manifest)? == name
                    && manifest.is_file(),
                "Cannot restore previously accepted Cargo member at {name}; {guidance}"
            );
            restored.push(member.clone());
        }
    }
    if !restored.is_empty() {
        add_members(&root, &restored, false)?;
    }
    Ok(())
}

pub(super) fn template_seed(
    release: &AdmittedRelease,
    role: Role,
    spec: &ComponentSpec,
) -> Result<(String, Vec<u8>)> {
    let template = template(&spec.template)?.id;
    let bytes = release.bytes(&format!("templates/{template}/Cargo.lock"))?;
    Ok((
        format!("{}/Cargo.lock", component_path(role, &spec.name)?),
        TemplateNames::read(release, role, spec)?.render("Cargo.lock", bytes)?,
    ))
}

fn fmt_settings(bytes: &[u8], path: &str) -> Result<toml::Table> {
    toml::from_str(std::str::from_utf8(bytes)?)
        .with_context(|| format!("Invalid formatter settings in {path}"))
}

/// Adopt only compatible inputs. The caller tracks all removed files in the journal.
pub(super) fn workspace_defaults(
    root: &Path,
    release: &AdmittedRelease,
    first: bool,
    adopting: Option<&str>,
) -> Result<Option<(String, Vec<u8>)>> {
    let root_fmt = input_path(root, "rustfmt.toml")?;
    let mut formatter = exists(&root_fmt)?
        .then(|| read_input_bytes(&root_fmt))
        .transpose()?;
    ensure!(first || formatter.is_some(), "Missing root rustfmt.toml");
    let root_pin = input_path(root, "rust-toolchain.toml")?;
    let pin = release
        .manifest
        .files
        .get("templates/rust-api/rust-toolchain.toml")
        .context("Missing release Rust toolchain pin")?;
    if first && let Some(actual) = identity(&root_pin)? {
        ensure!(
            actual.matches_git_tracked(pin),
            "Existing root rust-toolchain.toml conflicts with the release toolchain pin"
        );
    }
    let mut seed = None;
    if let Some(component) = adopting {
        for name in ["rust-toolchain.toml", "rustfmt.toml", "Cargo.lock"] {
            let rel = format!("{component}/{name}");
            let path = input_path(root, &rel)?;
            let Some(actual) = identity(&path)? else {
                continue;
            };
            match name {
                "rust-toolchain.toml" => ensure!(
                    actual.matches_git_tracked(pin),
                    "{rel} conflicts with the accepted rust-toolchain.toml"
                ),
                "rustfmt.toml" => {
                    let bytes = read_input_bytes(&path)?;
                    let settings = fmt_settings(&bytes, &rel)?;
                    if let Some(current) = &formatter {
                        ensure!(
                            settings == fmt_settings(current, "rustfmt.toml")?,
                            "{rel} conflicts with root rustfmt.toml"
                        );
                    } else {
                        formatter = Some(bytes);
                    }
                }
                _ => seed = Some((rel, read_input_bytes(&path)?)),
            }
            remove_file(&path)?;
        }
    }
    if !exists(&root_fmt)? {
        let bytes = match formatter {
            Some(bytes) => bytes,
            None => release.bytes("templates/rust-api/rustfmt.toml")?,
        };
        write_atomic(&root_fmt, &bytes, 0o644)?;
    }
    Ok(seed)
}
