use crate::{
    Result,
    catalog::*,
    compose::{MAX_YAML_BYTES, parse_compose},
    files::*,
    model::*,
    state::{has_electron, uses_profile_scoped_runtime},
};
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
    let has_packageable = state.components.values().any(|c| {
        matches!(
            c.tooling_profile,
            ToolingProfile::TsLibrary | ToolingProfile::ReactUi
        )
    });
    if has_packageable {
        result += &format!(
            "DEVKIT_PACKAGEABLE_PACKAGES := {}\n",
            names(&|c| {
                c.role == Role::Library
                    && matches!(
                        c.tooling_profile,
                        ToolingProfile::TsLibrary | ToolingProfile::ReactUi
                    )
            })
        );
    }
    for (path, c) in &state.components {
        if c.toolchain == Toolchain::Node {
            result += &format!("{}: setup-node-workspace\n", target("setup", path, c.role));
        }
        for command in ["check", "test", "build", "dev"] {
            if c.role == Role::Library && command == "dev" {
                continue;
            }
            let mut dependencies: BTreeSet<_> = c.dependencies.iter().collect();
            if command == "dev"
                && SemVer::parse(&state.version)
                    .is_ok_and(|v| v >= SemVer::parse("0.5.0").expect("valid version"))
                && let Some(api) = c
                    .development
                    .as_ref()
                    .and_then(|d| d.api.as_ref())
                    .and_then(|p| state.components.get(p))
            {
                dependencies.extend(api.dependencies.iter());
            }
            let deps = dependencies
                .into_iter()
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
        if matches!(
            c.tooling_profile,
            ToolingProfile::Electron | ToolingProfile::TsLibrary | ToolingProfile::ReactUi
        ) {
            result += &format!(
                "{}: {}\n",
                target("package", path, c.role),
                target("build", path, c.role)
            );
        }
    }
    if SemVer::parse(&state.version)
        .is_ok_and(|version| version >= SemVer::parse("0.5.0").expect("valid version"))
    {
        let token = digest(format!("{}\n{result}", state.release_identity).as_bytes());
        result += &format!("DEVKIT_EXPECTED_PLAN := {token}\n");
    }
    result
}

pub(crate) fn verify_make_plan(state: &ProjectState, token: &str) -> Result<()> {
    let projection = render_project_make(state);
    ensure!(
        SHA256_RE.is_match(token)
            && projection
                .lines()
                .last()
                .and_then(|line| { line.strip_prefix("DEVKIT_EXPECTED_PLAN := ") })
                == Some(token),
        "Accepted Make plan changed during this invocation; rerun the command"
    );
    Ok(())
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
    let modern = SemVer::parse(&state.version)? >= SemVer::parse("0.5.0")?;
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
                server.role == Role::App
                    && sd.kind == DevelopmentKind::Compose
                    && (!modern
                        || (api != path
                            && matches!(
                                server.tooling_profile,
                                ToolingProfile::Node | ToolingProfile::Rust
                            )
                            && matches!(
                                c.tooling_profile,
                                ToolingProfile::Web | ToolingProfile::Electron
                            ))),
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
                if modern {
                    "${DEVKIT_CARGO_CACHE_DIR:?}"
                } else {
                    "${DEVKIT_PROJECT_ROOT:?}/.devkit/cache/cargo"
                }
                .to_string(),
                "/cache/cargo".to_string(),
                false,
            ));
        } else {
            volumes.extend(node_volumes.clone());
            if modern {
                volumes.push(volume(
                    "${DEVKIT_PNPM_PATH:?}".into(),
                    "/devkit/bin/pnpm".into(),
                    true,
                ));
                env.extend([
                    ("PATH".into(),json!("/devkit/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")),
                    ("XDG_CACHE_HOME".into(),json!("/tmp/devkit-xdg-cache")),
                    ("NPM_CONFIG_CACHE".into(),json!("/tmp/devkit-npm-cache")),
                    ("PNPM_CONFIG_PM_ON_FAIL".into(),json!("ignore")),
                    ("PNPM_CONFIG_RUNTIME".into(),json!("false")),
                    ("PNPM_CONFIG_RUNTIME_ON_FAIL".into(),json!("ignore")),
                    ("PNPM_CONFIG_IGNORE_PNPMFILE".into(),json!("true")),
                ]);
            }
        }
        let command = if rust {
            vec!["cargo", "run", "--locked", "--offline"]
        } else if desktop {
            vec!["/devkit/devkit", "internal", "desktop"]
        } else {
            vec![
                if modern { "/devkit/bin/pnpm" } else { "npm" },
                "run",
                "dev",
            ]
        };
        let mut service = json!({"image":if rust{"${DEVKIT_RUST_IMAGE:-devkit-unprepared-rust}"}else if desktop{"${DEVKIT_DESKTOP_IMAGE:-devkit-unprepared-desktop}"}else{"${DEVKIT_NODE_IMAGE:?}"},"init":true,"user":"${DEVKIT_UID:?}:${DEVKIT_GID:?}","working_dir":format!("/workspace/{path}"),"command":command,"environment":env,"volumes":volumes});
        if modern
            && matches!(
                c.tooling_profile,
                ToolingProfile::Node | ToolingProfile::Rust
            )
        {
            // Project Compose follows this projection. Its env files and explicit
            // environment take precedence over the starter defaults.
            service["env_file"] = json!([".devkit/runtime/api.env"]);
        }
        if modern && let Some(api) = &d.api {
            let backend = &state.components[api];
            let name = backend
                .development
                .as_ref()
                .and_then(|d| d.service.as_deref())
                .unwrap_or_else(|| basename(api));
            service["depends_on"] = json!({name:{"condition":"service_started","required":true}});
        }
        if modern && !rust && !desktop {
            service["tmpfs"] = json!(["/tmp:mode=1777"]);
        }
        if desktop {
            service["volumes"].as_array_mut().unwrap().push(volume(
                "${DEVKIT_ENGINE_PATH:?}".into(),
                "/devkit/devkit".into(),
                true,
            ));
        }
        if desktop {
            let map = service.as_object_mut().unwrap();
            map.extend(serde_json::from_value::<serde_json::Map<String,Value>>(json!({"read_only":true,"tmpfs":["/tmp:mode=1777"],"shm_size":"256m","cap_drop":["ALL"],"security_opt":["no-new-privileges:true","seccomp=${DEVKIT_PROJECT_ROOT:?}/.devkit/runtime/desktop-seccomp.json"]}))?);
        } else {
            service["ports"] = json!([format!(
                "127.0.0.1:{}:{}",
                if modern {
                    format!(
                        "${{DEVKIT_PORT_{}:-0}}",
                        name.to_uppercase().replace('-', "_")
                    )
                } else {
                    String::new()
                },
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
    let mut services = json!({"node":{"volumes":volumes}});
    if !uses_profile_scoped_runtime(&state.version)? || has_electron(state) {
        services["desktop"] = services["node"].clone();
    }
    Ok(json!({"services":services}))
}
pub fn render_workspace_compose(
    state: &ProjectState,
    inputs: &InputIdentitySet,
) -> Result<Vec<u8>> {
    json_bytes(&render_workspace_compose_value(state, inputs)?)
}

fn value_mentions_electron_cache(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("ELECTRON_CACHE") || text.contains("/cache/electron"),
        Value::Array(items) => items.iter().any(value_mentions_electron_cache),
        Value::Object(map) => map.iter().any(|(key, item)| {
            key.contains("ELECTRON_CACHE")
                || key.contains("/cache/electron")
                || value_mentions_electron_cache(item)
        }),
        _ => false,
    }
}

fn node_leaks_electron_cache(node: &Map<String, Value>) -> bool {
    let inherits_desktop_volumes = node.get("volumes_from").is_some_and(|value| match value {
        Value::Array(items) => items.iter().any(|item| {
            item.as_str()
                .is_none_or(|entry| entry == "desktop" || entry.starts_with("desktop:"))
        }),
        _ => true,
    });
    inherits_desktop_volumes
        || node.iter().any(|(key, value)| {
            key.contains("ELECTRON_CACHE")
                || key.contains("/cache/electron")
                || value_mentions_electron_cache(value)
        })
}

fn checked_tool_compose_profile(bytes: &[u8], electron: bool) -> Result<Value> {
    let model = parse_compose(bytes)?;
    let services = object(&model["services"], "Tool Compose services")?;
    let node = services.get("node").context("Missing node tool service")?;
    let node_settings = object(node, "Node tool service")?;
    ensure!(
        !node_leaks_electron_cache(node_settings),
        "The node tool service contains an Electron cache"
    );
    if electron {
        ensure!(
            services.contains_key("desktop"),
            "Missing desktop tool service"
        );
    } else {
        let source = std::str::from_utf8(bytes)?;
        ensure!(
            !services.contains_key("desktop")
                && !source.contains("desktop-seccomp.json")
                && !source.contains("ELECTRON_CACHE"),
            "Headless tool Compose contains desktop runtime references"
        );
    }
    Ok(model)
}

fn checked_release_tool_compose(bytes: &[u8]) -> Result<Value> {
    let model = checked_tool_compose_profile(bytes, true)?;
    let desktop = &model["services"]["desktop"];
    let security = desktop["security_opt"]
        .as_array()
        .context("Missing desktop security options")?;
    let volumes = desktop["volumes"]
        .as_array()
        .context("Missing desktop volumes")?;
    ensure!(
        desktop["shm_size"] == "256m"
            && desktop["cap_drop"] == json!(["ALL"])
            && security.iter().any(|opt| opt == "no-new-privileges:true")
            && security.iter().any(|opt| {
                opt == "seccomp=${DEVKIT_PROJECT_ROOT:?}/.devkit/runtime/desktop-seccomp.json"
            }),
        "Invalid desktop tool service security settings"
    );
    ensure!(
        desktop["environment"]["HOME"].is_string()
            && desktop["environment"]["ELECTRON_CACHE"] == "/cache/electron"
            && ["/workspace", "/devkit/bin/pnpm", "/cache/electron"]
                .iter()
                .all(|target| volumes.iter().any(|volume| volume["target"] == *target)),
        "Invalid desktop tool environment or mounts"
    );
    Ok(model)
}

pub fn render_headless_tool_compose(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut expected = checked_release_tool_compose(bytes)?;
    expected["services"]
        .as_object_mut()
        .unwrap()
        .remove("desktop");

    // Keep the release YAML's comments and anchors. Check the parsed models so
    // a change in the release layout cannot silently alter another service.
    let source = std::str::from_utf8(bytes)?;
    let (prefix, desktop_body) = source
        .split_once("\n  desktop:\n")
        .context("Missing desktop service block in release tool Compose")?;
    let body_len: usize = desktop_body
        .split_inclusive('\n')
        .take_while(|line| line.starts_with("   ") || line.trim().is_empty())
        .map(str::len)
        .sum();
    let headless = format!("{prefix}\n{}", &desktop_body[body_len..]).into_bytes();
    ensure!(
        checked_tool_compose_profile(&headless, false)? == expected,
        "Removing desktop changed other tool Compose settings"
    );
    Ok(headless)
}

pub(crate) fn verify_release_tool_compose(bytes: &[u8]) -> Result<()> {
    render_headless_tool_compose(bytes)?;
    Ok(())
}

fn assert_tool_compose(root: &Path, electron: bool) -> Result<()> {
    let path = safe_path(root, ".devkit/runtime/compose.tools.yaml")?;
    let (bytes, _) = read_regular_file(&path, MAX_YAML_BYTES as u64, "Tool Compose projection")?
        .context("Missing tool Compose projection")?;
    checked_tool_compose_profile(&bytes, electron)?;
    Ok(())
}

pub fn assert_projections(
    root: &Path,
    state: &ProjectState,
    inputs: &InputIdentitySet,
) -> Result<()> {
    ensure!(
        fs::read(safe_path(root, ".devkit/project.mk")?)? == render_project_make(state).as_bytes(),
        "Component Make registration projection is stale; run devkit sync"
    );
    let apps: Value = read_json(&safe_path(root, ".devkit/compose.apps.yaml")?)?;
    ensure!(
        apps == render_app_compose_value(state)?,
        "Development integration projection is stale; run devkit sync"
    );
    let workspace: Value = read_json(&safe_path(root, ".devkit/compose.workspace.json")?)?;
    ensure!(
        workspace == render_workspace_compose_value(state, inputs)?,
        "Workspace mount projection is stale; run devkit sync"
    );
    if uses_profile_scoped_runtime(&state.version)? {
        assert_tool_compose(root, has_electron(state))
            .context("Tool Compose projection is stale; run devkit sync")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_projection_rejects_stale_desktop_mounts_and_security() {
        let full = include_bytes!("../../../runtime/compose.tools.yaml");
        let mut model = checked_release_tool_compose(full).unwrap();
        assert!(checked_tool_compose_profile(full, false).is_err());
        model["services"]["desktop"]["volumes"] = model["services"]["node"]["volumes"].clone();
        assert!(checked_release_tool_compose(&json_bytes(&model).unwrap()).is_err());

        let mut model = parse_compose(full).unwrap();
        model["services"]["desktop"]["cap_drop"] = json!([]);
        assert!(checked_release_tool_compose(&json_bytes(&model).unwrap()).is_err());
        let headless = render_headless_tool_compose(full).unwrap();
        for mutation in [
            |node: &mut Value| {
                node["volumes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("cache:/cache/electron"));
            },
            |node: &mut Value| {
                node["volumes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type": "bind", "source": "/host", "target": "/cache/electron/"}));
            },
            |node: &mut Value| {
                node["volumes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("${DEVKIT_ELECTRON_CACHE_DIR}:/tmp/ec"));
            },
            |node: &mut Value| {
                node["environment"]["CUSTOM"] = json!("${DEVKIT_ELECTRON_CACHE_DIR}");
            },
            |node: &mut Value| {
                node["volumes_from"] = json!(["desktop:ro"]);
            },
        ] {
            let mut model = checked_tool_compose_profile(&headless, false).unwrap();
            mutation(&mut model["services"]["node"]);
            assert!(checked_tool_compose_profile(&json_bytes(&model).unwrap(), false).is_err());

            let mut full_model = parse_compose(full).unwrap();
            mutation(&mut full_model["services"]["node"]);
            assert!(checked_release_tool_compose(&json_bytes(&full_model).unwrap()).is_err());
        }
    }
}
