//! Test fixture authoring only; not included in release payloads.
use devkit::{Result, files::*, model::*, native::*, projections::*, state::has_electron};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};
fn main() -> Result<()> {
    let root = PathBuf::from(std::env::args_os().nth(1).expect("fixture root"));
    if std::env::args_os().nth(2).as_deref() == Some(std::ffi::OsStr::new("--port-lease")) {
        let _lock = devkit::state::CheckoutLock::acquire(&root)?;
        let cache = devkit::cache::cache_root(None)?;
        let mut ports = devkit::stand::ports::Ports::open(&root, &cache)?;
        let declarations = devkit::stand::ports::declarations(
            &json!({"services":{"api":{"ports":[{"target":8080,"published":"0","host_ip":"127.0.0.1"}]}}}),
            &std::collections::BTreeSet::from(["api".into()]),
        )?;
        ports.allocate(
            &declarations,
            None,
            "fixture",
            &Default::default(),
            &Default::default(),
        )?;
        println!("{}", serde_json::to_string(&ports)?);
        return Ok(());
    }
    if std::env::args_os().nth(2).as_deref() == Some(std::ffi::OsStr::new("--compose-inventory")) {
        let inventory = devkit::compose::admit_compose_inventory(
            &root,
            &devkit::runtime::DockerComposeNormalizer {
                runner: devkit::processes::ProcessRunner::new()?,
            },
        )?;
        println!("{}", serde_json::to_string(&inventory)?);
        return Ok(());
    }
    let mut state: ProjectState = read_json(&root.join(".devkit/manifest.json"))?;
    let tools = include_bytes!("../../../runtime/compose.tools.yaml");
    let compose = if has_electron(&state) {
        tools.to_vec()
    } else {
        render_headless_tool_compose(tools)?
    };
    write_atomic(
        &root.join(".devkit/runtime/compose.tools.yaml"),
        &compose,
        0o644,
    )?;
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
