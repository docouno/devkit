//! Test fixture authoring only; not included in release payloads.
use devkit::{Result, files::*, model::*, native::*, projections::*};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};
fn main() -> Result<()> {
    let root = PathBuf::from(std::env::args_os().nth(1).expect("fixture root"));
    let mut state: ProjectState = read_json(&root.join(".devkit/manifest.json"))?;
    let dependencies = derive_component_dependencies(&root, &state)?;
    for (path, deps) in dependencies {
        state.components.get_mut(&path).unwrap().dependencies = deps;
    }
    let inputs = collect_native_input_closure(&root, &state)?;
    write_atomic(
        &root.join(".devkit/project.mk"),
        render_project_make(&state).as_bytes(),
        0o644,
    )?;
    write_atomic(
        &root.join(".devkit/compose.apps.yaml"),
        &render_app_compose(&state)?,
        0o644,
    )?;
    write_atomic(
        &root.join(".devkit/compose.workspace.json"),
        &render_workspace_compose(&state, &inputs)?,
        0o644,
    )?;
    let profiles = state
        .components
        .values()
        .map(|c| (c.tooling_profile.as_str(), json!(c.tool_dependencies)))
        .collect::<BTreeMap<_, _>>();
    write_json(
        &root.join(".devkit/tooling-profiles.json"),
        &json!({"schemaVersion":1,"version":state.version,"packageMode":state.package_mode,"profiles":profiles}),
    )?;
    write_json(&root.join(".devkit/manifest.json"), &state)?;
    Ok(())
}
