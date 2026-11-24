use crate::{Result, files::slug, model::*};
use anyhow::{Context, bail, ensure};

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
        "rust-library" => (
            "rust-library",
            Role::Library,
            Toolchain::Rust,
            ToolingProfile::Rust,
        ),
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
        "ts-library" => (
            "ts-library",
            Role::Library,
            Toolchain::Node,
            ToolingProfile::TsLibrary,
        ),
        "react-ui" => (
            "react-ui",
            Role::Library,
            Toolchain::Node,
            ToolingProfile::ReactUi,
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
        ToolingProfile::TsLibrary => "ts-library",
        ToolingProfile::ReactUi => "react-ui",
        ToolingProfile::Rust => "rust-api",
    }
}
pub fn profile_allows(profile: ToolingProfile, role: Role) -> bool {
    match profile {
        ToolingProfile::ReactLibrary | ToolingProfile::TsLibrary | ToolingProfile::ReactUi => {
            role == Role::Library
        }
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
        ToolingProfile::Electron | ToolingProfile::TsLibrary | ToolingProfile::ReactUi => {
            &["dist", "release"]
        }
        _ => &["dist"],
    }
}

/// Select the pinned Cargo resolver image from the same recipe used for Rust gates.
pub fn rust_base_image(recipe: &str, rust_version: &str) -> Result<String> {
    let mut stages = recipe.lines().map(str::trim).filter(|line| {
        line.split_ascii_whitespace()
            .next()
            .is_some_and(|instruction| instruction.eq_ignore_ascii_case("FROM"))
    });
    let from = stages
        .next()
        .context("Missing FROM in runtime/rust.Dockerfile")?;
    let mut words = from.split_ascii_whitespace().skip(1).peekable();
    while words.peek().is_some_and(|word| word.starts_with("--")) {
        let flag = words.next().expect("checked flag");
        ensure!(
            flag.starts_with("--platform=") && flag.len() > "--platform=".len(),
            "Unsupported FROM flag in runtime/rust.Dockerfile: {flag}"
        );
    }
    let image = words.next().context("Missing Rust base image")?;
    if let Some(alias_kw) = words.next() {
        ensure!(
            alias_kw.eq_ignore_ascii_case("AS") && words.next().is_some() && words.next().is_none(),
            "Unexpected trailing tokens in runtime/rust.Dockerfile FROM instruction"
        );
    }
    let prefix = format!("rust:{rust_version}-slim-bookworm@sha256:");
    ensure!(
        stages.next().is_none()
            && image
                .strip_prefix(&prefix)
                .is_some_and(|digest| crate::files::SHA256_RE.is_match(digest)),
        "Expected a single digest-pinned {prefix}<digest> base image in runtime/rust.Dockerfile"
    );
    Ok(image.to_owned())
}
