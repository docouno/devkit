use crate::{Result, catalog::*, files::*, model::*};
use anyhow::{Context, ensure};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fs, path::Path};

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
fn target(command: &str, path: &str, role: Role) -> String {
    if role == Role::App {
        format!("{command}-{}", basename(path))
    } else {
        format!("{command}/{path}")
    }
}
pub fn render_project_make(state: &ProjectState) -> String {
    let names = |predicate: &dyn Fn(&ComponentState) -> bool| {
        state
            .components
            .iter()
            .filter(|(_, c)| predicate(c))
            .map(|(p, _)| basename(p))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut result = format!(
        "# Managed component registration.\nDEVKIT_REGISTERED_APPS := {}\nDEVKIT_REGISTERED_PACKAGES := {}\nDEVKIT_DESKTOP_APPS := {}\n",
        names(&|c| c.role == Role::App),
        names(&|c| c.role == Role::Library),
        names(&|c| c.role == Role::App && c.tooling_profile == ToolingProfile::Electron)
    );
    for (path, c) in &state.components {
        if c.toolchain == Toolchain::Node {
            result += &format!("{}: setup-node-workspace\n", target("setup", path, c.role));
        }
        for command in ["check", "test", "build", "dev"] {
            if c.role == Role::Library && command == "dev" {
                continue;
            }
            let deps = c
                .dependencies
                .iter()
                .flat_map(|d| {
                    if command == "check" {
                        vec![format!("check/{d}"), format!("build/{d}")]
                    } else {
                        vec![format!("build/{d}")]
                    }
                })
                .collect::<Vec<_>>();
            if !deps.is_empty() {
                result += &format!("{}: {}\n", target(command, path, c.role), deps.join(" "));
            }
        }
        if c.tooling_profile == ToolingProfile::Electron {
            result += &format!(
                "{}: {}\n",
                target("package", path, c.role),
                target("build", path, c.role)
            );
        }
    }
    result
}
fn volume(source: String, target: String, read_only: bool) -> Value {
    json!({"type":"bind","source":source,"target":target,"read_only":read_only,"bind":{"create_host_path":false,"selinux":"z"}})
}
fn project_volume(path: &str) -> Value {
    volume(
        format!("${{DEVKIT_PROJECT_ROOT:?}}/{path}"),
        format!("/workspace/{path}"),
        false,
    )
}
pub fn render_app_compose_value(state: &ProjectState) -> Result<Value> {
    // Insertion order is part of the accepted0.2 JS runtime projection wire
    // format. New native output uses this same deterministic serializer.
    let mut services = Map::new();
    let mut node_volumes = vec![project_volume("node_modules")];
    for (path, c) in node_components(state) {
        for directory in std::iter::once(&"node_modules").chain(profile_outputs(c.tooling_profile))
        {
            node_volumes.push(project_volume(&format!("{path}/{directory}")));
        }
    }
    for (path, c) in &state.components {
        let Some(d) = &c.development else {
            continue;
        };
        if c.role != Role::App || d.owner != DevelopmentOwner::Managed {
            continue;
        }
        let name = d.service.as_deref().unwrap_or_else(|| basename(path));
        ensure!(
            !services.contains_key(name),
            "Duplicate managed Compose service: {name}"
        );
        let rust = c.toolchain == Toolchain::Rust;
        let desktop = c.tooling_profile == ToolingProfile::Electron;
        let mut env = Map::from_iter([
            ("HOME".to_string(), json!("/tmp/devkit-home")),
            ("NPM_CONFIG_UPDATE_NOTIFIER".to_string(), json!("false")),
        ]);
        if let Some(api) = &d.api {
            let server = state
                .components
                .get(api)
                .context("Invalid API connection")?;
            let sd = server
                .development
                .as_ref()
                .context("Missing API development integration")?;
            ensure!(
                server.role == Role::App && sd.kind == DevelopmentKind::Compose,
                "Invalid API connection: {api}"
            );
            env.insert(
                "DEVKIT_API_TARGET".to_string(),
                json!(format!(
                    "http://{}:8080",
                    sd.service.as_deref().unwrap_or_else(|| basename(api))
                )),
            );
            if c.tooling_profile == ToolingProfile::Web {
                env.insert("VITE_API_ENABLED".to_string(), json!("true"));
            }
        }
        if rust {
            env.extend([
                ("CARGO_HOME".to_string(), json!("/cache/cargo")),
                (
                    "CARGO_TARGET_DIR".to_string(),
                    json!(format!("/workspace/{path}/target")),
                ),
                ("RUSTUP_AUTO_INSTALL".to_string(), json!("0")),
            ]);
        }
        let mut volumes = vec![volume(
            "${DEVKIT_PROJECT_ROOT:?}".to_string(),
            "/workspace".to_string(),
            true,
        )];
        if rust {
            volumes.push(project_volume(&format!("{path}/target")));
            volumes.push(volume(
                "${DEVKIT_PROJECT_ROOT:?}/.devkit/cache/cargo".to_string(),
                "/cache/cargo".to_string(),
                false,
            ));
        } else {
            volumes.extend(node_volumes.clone());
        }
        let command = if rust {
            vec!["cargo", "run", "--locked", "--offline"]
        } else if desktop {
            vec!["bash", "/workspace/.devkit/runtime/desktop-dev.sh"]
        } else {
            vec!["npm", "run", "dev"]
        };
        let mut service = json!({"image":if rust{"${DEVKIT_RUST_IMAGE:-devkit-unprepared-rust}"}else if desktop{"${DEVKIT_DESKTOP_IMAGE:-devkit-unprepared-desktop}"}else{"${DEVKIT_NODE_IMAGE:?}"},"init":true,"user":"${DEVKIT_UID:?}:${DEVKIT_GID:?}","working_dir":format!("/workspace/{path}"),"command":command,"environment":env,"volumes":volumes});
        if desktop {
            let map = service.as_object_mut().unwrap();
            map.extend(serde_json::from_value::<serde_json::Map<String,Value>>(json!({"read_only":true,"tmpfs":["/tmp:mode=1777"],"shm_size":"256m","cap_drop":["ALL"],"security_opt":["no-new-privileges:true","seccomp=${DEVKIT_PROJECT_ROOT:?}/.devkit/runtime/desktop-seccomp.json"]}))?);
        } else {
            service["ports"] = json!([format!(
                "127.0.0.1::{}",
                if c.tooling_profile == ToolingProfile::Web {
                    "5173"
                } else {
                    "8080"
                }
            )]);
        }
        services.insert(name.to_string(), service);
    }
    Ok(json!({"services":services}))
}
pub fn render_app_compose(state: &ProjectState) -> Result<Vec<u8>> {
    json_bytes(&render_app_compose_value(state)?)
}
pub fn workspace_write_directories(
    state: &ProjectState,
    inputs: &InputIdentitySet,
) -> Result<Vec<String>> {
    if node_components(state).is_empty() {
        return Ok(vec![]);
    }
    let mut paths = BTreeSet::new();
    for d in &inputs.directories {
        paths.insert(if d.is_empty() {
            "node_modules".to_string()
        } else {
            format!("{d}/node_modules")
        });
    }
    for (p, c) in node_components(state) {
        for o in profile_outputs(c.tooling_profile) {
            paths.insert(format!("{p}/{o}"));
        }
    }
    for p in &paths {
        ensure!(
            !p.chars().any(|c| ['\r', '\n', '\t', '\0'].contains(&c))
                && !p.starts_with('/')
                && !p.split('/').any(|c| ["", ".", ".."].contains(&c)),
            "Invalid workspace output path: {p}"
        );
    }
    let mut paths = paths.into_iter().collect::<Vec<_>>();
    // JavaScript Array.sort uses UTF16 code units; keep old runtime byte
    // compatibility even for nonmember local dependency directories.
    paths.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    Ok(paths)
}
pub fn render_workspace_compose_value(
    state: &ProjectState,
    inputs: &InputIdentitySet,
) -> Result<Value> {
    let volumes = workspace_write_directories(state, inputs)?
        .iter()
        .map(|p| project_volume(p))
        .collect::<Vec<_>>();
    Ok(json!({"services":{"node":{"volumes":volumes},"desktop":{"volumes":volumes}}}))
}
pub fn render_workspace_compose(
    state: &ProjectState,
    inputs: &InputIdentitySet,
) -> Result<Vec<u8>> {
    json_bytes(&render_workspace_compose_value(state, inputs)?)
}
pub fn assert_projections(
    root: &Path,
    state: &ProjectState,
    inputs: &InputIdentitySet,
) -> Result<()> {
    ensure!(
        fs::read(safe_path(root, ".devkit/project.mk")?)? == render_project_make(state).as_bytes(),
        "Component Make registration projection is stale; run the accepted devkit update"
    );
    let apps: Value = read_json(&safe_path(root, ".devkit/compose.apps.yaml")?)?;
    ensure!(
        apps == render_app_compose_value(state)?,
        "Development integration projection is stale; run the accepted devkit update"
    );
    let workspace: Value = read_json(&safe_path(root, ".devkit/compose.workspace.json")?)?;
    ensure!(
        workspace == render_workspace_compose_value(state, inputs)?,
        "Workspace mount projection is stale; run the accepted devkit update"
    );
    Ok(())
}
