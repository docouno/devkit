use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Extras = BTreeMap<String, Value>;

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename=$wire)] $variant),+ }
        impl $name { pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}
wire_enum!(PackageMode { Local => "local", Registry => "registry" });
wire_enum!(Role { App => "app", Library => "library" });
wire_enum!(Toolchain { Node => "node", Rust => "rust" });
wire_enum!(ToolingProfile { Node => "node", Web => "web", Electron => "electron", ReactLibrary => "react-library", TsLibrary => "ts-library", ReactUi => "react-ui", Rust => "rust" });
wire_enum!(DevelopmentOwner { Managed => "managed", Project => "project" });
wire_enum!(DevelopmentKind { Compose => "compose", Desktop => "desktop" });
wire_enum!(MigrationSeverity { Info => "info", Recommended => "recommended", ActionRequired => "action-required" });

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationEntry {
    pub id: String,
    pub version: String,
    pub severity: MigrationSeverity,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    pub doc_path: String,
    pub anchor: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub sha256: String,
    pub mode: u32,
}
impl FileIdentity {
    /// Git tracks content and executable class (`100644` vs `100755`), while worktree
    /// permissions reflect host `umask` (`0002` -> `0664`/`0775`, `0077` -> `0600`/`0700`).
    pub fn matches_git_tracked(&self, expected: &Self) -> bool {
        self.sha256 == expected.sha256 && (self.mode & 0o111 != 0) == (expected.mode & 0o111 != 0)
    }
    pub fn matches_git_tracked_opt(actual: Option<&Self>, expected: Option<&Self>) -> bool {
        match (actual, expected) {
            (Some(a), Some(e)) => a.matches_git_tracked(e),
            (None, None) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub schema_version: u32,
    pub version: String,
    pub files: BTreeMap<String, FileIdentity>,
    pub environment: Environment,
    pub rust_version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<MigrationEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Environment {
    pub name: String,
    pub version: String,
    pub archive: String,
    pub integrity: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub template: String,
    pub version: String,
    #[serde(flatten)]
    pub extra: Extras,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Development {
    pub owner: DevelopmentOwner,
    pub kind: DevelopmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(flatten)]
    pub extra: Extras,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentState {
    pub role: Role,
    pub toolchain: Toolchain,
    pub tooling_profile: ToolingProfile,
    pub origin: Origin,
    pub tool_dependencies: BTreeMap<String, String>,
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub development: Option<Development>,
    #[serde(flatten)]
    pub extra: Extras,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectState {
    pub schema_version: u32,
    pub version: String,
    pub release_identity: String,
    pub generation: String,
    pub package_mode: PackageMode,
    pub managed: BTreeMap<String, FileIdentity>,
    pub components: BTreeMap<String, ComponentState>,
    #[serde(flatten)]
    pub extra: Extras,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputIdentitySet {
    pub schema_version: u32,
    pub files: BTreeMap<String, Option<FileIdentity>>,
    pub directories: Vec<String>,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeInventory {
    pub schema_version: u32,
    pub protocol: String,
    pub entrypoints: Vec<String>,
    pub inputs: InputIdentitySet,
    pub services: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFile {
    pub path: String,
    pub before: Option<FileIdentity>,
    pub after: Option<FileIdentity>,
    pub backup: Option<String>,
    pub staged: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateJournal {
    pub schema_version: u32,
    pub stage: String,
    pub files: Vec<PlannedFile>,
    #[serde(default)]
    pub created_directories: Vec<String>,
    #[serde(default)]
    pub created_parents: Vec<String>,
}
