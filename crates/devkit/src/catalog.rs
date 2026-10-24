use crate::{Result, files::slug, model::*};
use anyhow::{bail, ensure};

#[derive(Clone, Copy, Debug)]
pub struct Template {
    pub id: &'static str,
    pub role: Role,
    pub toolchain: Toolchain,
    pub profile: ToolingProfile,
}
pub fn template(value: &str) -> Result<Template> {
    let (id, role, toolchain, profile) = match value {
        "node" | "node-app" | "node-api" => {
            ("node-api", Role::App, Toolchain::Node, ToolingProfile::Node)
        }
        "react" | "react-app" | "react-web" => {
            ("react-web", Role::App, Toolchain::Node, ToolingProfile::Web)
        }
        "rust" | "rust-app" | "rust-api" => {
            ("rust-api", Role::App, Toolchain::Rust, ToolingProfile::Rust)
        }
        "electron-react" => (
            "electron-react",
            Role::App,
            Toolchain::Node,
            ToolingProfile::Electron,
        ),
        "react-client" => (
            "react-client",
            Role::Library,
            Toolchain::Node,
            ToolingProfile::ReactLibrary,
        ),
        _ => bail!("Unknown component template: {value}"),
    };
    Ok(Template {
        id,
        role,
        toolchain,
        profile,
    })
}
pub fn profile_template(profile: ToolingProfile) -> &'static str {
    match profile {
        ToolingProfile::Node => "node-api",
        ToolingProfile::Web => "react-web",
        ToolingProfile::Electron => "electron-react",
        ToolingProfile::ReactLibrary => "react-client",
        ToolingProfile::Rust => "rust-api",
    }
}
pub fn profile_allows(profile: ToolingProfile, role: Role) -> bool {
    match profile {
        ToolingProfile::ReactLibrary => role == Role::Library,
        ToolingProfile::Rust => true,
        _ => role == Role::App,
    }
}
pub fn profile_toolchain(profile: ToolingProfile) -> Toolchain {
    if profile == ToolingProfile::Rust {
        Toolchain::Rust
    } else {
        Toolchain::Node
    }
}
pub fn component_path(role: Role, name: &str) -> Result<String> {
    slug(name)?;
    Ok(format!(
        "{}/{name}",
        if role == Role::App {
            "apps"
        } else {
            "packages"
        }
    ))
}
pub fn valid_component_path(value: &str) -> Result<&str> {
    let parts: Vec<_> = value.split('/').collect();
    ensure!(
        parts.len() == 2 && ["apps", "packages"].contains(&parts[0]),
        "Invalid component path: {value}"
    );
    slug(parts[1])?;
    Ok(value)
}
pub fn node_components(state: &ProjectState) -> Vec<(&String, &ComponentState)> {
    state
        .components
        .iter()
        .filter(|(_, c)| c.toolchain == Toolchain::Node)
        .collect()
}
pub fn profile_outputs(profile: ToolingProfile) -> &'static [&'static str] {
    match profile {
        ToolingProfile::Rust => &["target"],
        ToolingProfile::Electron => &["dist", "release"],
        _ => &["dist"],
    }
}
