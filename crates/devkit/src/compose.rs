use crate::{Result, files::*, model::*};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::Path,
};

pub const ENTRYPOINTS: [&str; 2] = [".devkit/compose.apps.yaml", "compose.yaml"];
const MAX_YAML_BYTES: usize = 4 * 1024 * 1024;
pub fn parse_compose(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() <= MAX_YAML_BYTES,
        "Compose YAML exceeds byte budget"
    );
    let mut options = serde_saphyr::Options::default();
    let mut budget = serde_saphyr::Budget::default();
    budget.max_reader_input_bytes = Some(MAX_YAML_BYTES);
    budget.max_events = 200_000;
    budget.max_depth = 64;
    budget.max_aliases = 100;
    budget.max_anchors = 100;
    budget.max_documents = 1;
    budget.max_nodes = 100_000;
    budget.max_total_scalar_bytes = MAX_YAML_BYTES;
    budget.max_recorded_anchor_events = 100_000;
    budget.max_recorded_anchor_bytes = MAX_YAML_BYTES;
    budget.max_merge_keys = 1000;
    options.budget = Some(budget);
    options.alias_limits.max_total_replayed_events = 100_000;
    options.alias_limits.max_replay_stack_depth = 32;
    options.alias_limits.max_alias_expansions_per_anchor = 100;
    let value: Value = serde_saphyr::from_str_with_options(std::str::from_utf8(bytes)?, options)?;
    object(&value, "Compose document")?;
    Ok(value)
}
fn capture(root: &Path, snapshot: Option<&Path>) -> Result<ComposeInventory> {
    let root = project_root(root)?;
    let mut files = BTreeMap::new();
    let mut include_env = std::collections::BTreeSet::new();
    let mut queue: VecDeque<_> = ENTRYPOINTS.iter().map(|p| (p.to_string(), true)).collect();
    while let Some((name, optional)) = queue.pop_front() {
        ensure!(
            files.len() < 1024,
            "Compose reference closure exceeds file budget"
        );
        let path = input_path(&root, &name)?;
        let key = relative_name(&root, &path)?;
        if files.contains_key(&key) {
            continue;
        }
        let Some((bytes, id)) = read_regular_file(&path, MAX_YAML_BYTES as u64, "Compose input")?
        else {
            files.insert(key.clone(), None);
            ensure!(optional, "Missing Compose input: {key}");
            continue;
        };
        files.insert(key.clone(), Some(id.clone()));
        let data = parse_compose(&bytes).with_context(|| format!("Invalid Compose YAML: {key}"))?;
        if let Some(snapshot) = snapshot {
            write_atomic(&input_path(snapshot, &key)?, &bytes, id.mode)?;
        }
        let mut add = |value: &Value, include: bool| -> Result<()> {
            let reference = text(value, "Compose local reference")?;
            ensure!(
                !reference.contains('$')
                    && !reference.contains("://")
                    && !Path::new(reference).is_absolute(),
                "Unsupported Compose reference in {key}"
            );
            let parent = Path::new(&key).parent().context("Compose parent")?;
            let target = input_path(&root, parent.join(reference))?;
            if include {
                let env = input_path(
                    &root,
                    target
                        .parent()
                        .context("Compose include parent")?
                        .join(".env"),
                )?;
                include_env.insert(relative_name(&root, &env)?);
            }
            queue.push_back((relative_name(&root, &target)?, false));
            Ok(())
        };
        if let Some(includes) = data.get("include") {
            for value in includes
                .as_array()
                .context("Compose include must be a local string array")?
            {
                add(value, true)?;
            }
        }
        let empty = json!({});
        for service in object(data.get("services").unwrap_or(&empty), "Compose services")?.values()
        {
            let s = object(service, "Compose service")?;
            if let Some(extension) = s.get("extends")
                && let Some(file) = object(extension, "Compose extends")?.get("file")
            {
                add(file, false)?;
            }
        }
    }
    for name in include_env {
        ensure!(
            !files.contains_key(&name),
            "Compose model overlaps include environment: {name}"
        );
        let path = input_path(&root, &name)?;
        let value = read_regular_file(&path, MAX_YAML_BYTES as u64, "Compose include environment")?;
        if let Some(snapshot) = snapshot
            && let Some((bytes, id)) = &value
        {
            write_atomic(&input_path(snapshot, &name)?, bytes, id.mode)?;
        }
        files.insert(name, value.map(|(_, id)| id));
    }
    let digest = digest(serde_json::to_vec(&json!([
        "devkit-compose-inputs-v1",
        files
    ]))?);
    Ok(ComposeInventory {
        schema_version: 1,
        protocol: "compose-local-v1".to_string(),
        entrypoints: ENTRYPOINTS
            .iter()
            .filter(|n| files.get(**n).is_some_and(Option::is_some))
            .map(|s| s.to_string())
            .collect(),
        inputs: InputIdentitySet {
            schema_version: 1,
            files,
            directories: vec![],
            digest,
        },
        services: vec![],
    })
}
pub fn capture_compose_inputs(root: &Path) -> Result<ComposeInventory> {
    capture(root, None)
}
pub fn assert_compose_inventory(root: &Path, inventory: &ComposeInventory) -> Result<()> {
    ensure!(
        inventory.schema_version == 1 && inventory.protocol == "compose-local-v1",
        "Invalid Compose inventory"
    );
    for name in &inventory.services {
        ensure!(
            !name.is_empty() && !name.chars().any(char::is_control),
            "Invalid Compose service name"
        );
    }
    let now = capture_compose_inputs(root)?;
    ensure!(
        now.inputs == inventory.inputs && now.entrypoints == inventory.entrypoints,
        "Compose admitted inputs changed"
    );
    Ok(())
}
/// Native Compose, rather than a second YAML interpreter, defines the effective
/// service inventory. Implementations must return its normalized config JSON.
pub trait ComposeNormalizer {
    fn normalize(&self, snapshot: &Path, entrypoints: &[String]) -> Result<Value>;
}
pub fn admit_compose_inventory(
    root: &Path,
    normalizer: &dyn ComposeNormalizer,
) -> Result<ComposeInventory> {
    let state = safe_path(root, ".devkit/state")?;
    fs::create_dir_all(&state)?;
    let stage = tempfile::Builder::new()
        .prefix("compose-")
        .tempdir_in(state)?;
    let mut inventory = capture(root, Some(stage.path()))?;
    if !inventory.entrypoints.is_empty() {
        let model = normalizer.normalize(stage.path(), &inventory.entrypoints)?;
        let empty = json!({});
        inventory.services = object(
            model.get("services").unwrap_or(&empty),
            "Normalized Compose services",
        )?
        .keys()
        .cloned()
        .collect();
    }
    assert_compose_inventory(root, &inventory)?;
    Ok(inventory)
}
