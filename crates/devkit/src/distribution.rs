//! Wire identities shared by the native consumer and the author assembler.
use crate::{Result, files::*};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const TARGET: &str = "x86_64-unknown-linux-musl";
pub const ENGINE_ASSET: &str = "devkit-x86_64-unknown-linux-musl";
pub const RELEASE_BASE: &str = "https://github.com/docouno/devkit/releases/download";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub schema_version: u32,
    pub version: String,
    pub target: String,
    pub source_commit: Option<String>,
    pub source_tree: Option<String>,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        schema_version: 1,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: if cfg!(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "musl"
        )) {
            TARGET
        } else {
            "unsupported-build-target"
        }
        .to_owned(),
        source_commit: option_env!("DEVKIT_SOURCE_COMMIT").map(str::to_owned),
        source_tree: option_env!("DEVKIT_SOURCE_TREE").map(str::to_owned),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AssetIdentity {
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpmArtifact {
    pub name: String,
    pub version: String,
    pub archive: String,
    pub integrity: String,
    pub shasum: String,
}

/// The outer build-set index is written after all archives. It never hashes
/// itself and is not part of the release payload that those archives contain.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseIndex {
    pub schema_version: u32,
    pub version: String,
    pub target: String,
    pub source_commit: String,
    pub source_tree: String,
    pub build_run: String,
    pub publication_eligible: bool,
    pub release_identity: String,
    pub engine: String,
    pub archive: String,
    pub packages: BTreeMap<String, NpmArtifact>,
    pub files: BTreeMap<String, AssetIdentity>,
}

impl ReleaseIndex {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "Unsupported release-index schemaVersion"
        );
        version(&self.version)?;
        ensure!(
            self.target == TARGET,
            "Unsupported release target: {}",
            self.target
        );
        ensure!(
            SHA256_RE.is_match(&self.release_identity),
            "Invalid release identity"
        );
        ensure!(
            self.engine == ENGINE_ASSET,
            "Unexpected native engine asset"
        );
        ensure!(
            self.archive == format!("devkit-{}.tar.gz", self.version),
            "Unexpected release archive"
        );
        ensure!(
            !self.files.is_empty() && self.files.len() <= 32,
            "Invalid release asset count"
        );
        for (name, value) in &self.files {
            relative_path(name)?;
            ensure!(
                !name.contains('/') && name != "release-index.json",
                "Unsafe release asset: {name}"
            );
            ensure!(
                SHA256_RE.is_match(&value.sha256) && value.size <= 128 * 1024 * 1024,
                "Invalid release asset identity: {name}"
            );
        }
        for name in [
            self.engine.as_str(),
            self.archive.as_str(),
            "release.json",
            "install.sh",
            "bootstrap.sh",
            "checksums.txt",
        ] {
            ensure!(self.files.contains_key(name), "Release index omits {name}");
        }
        ensure!(
            self.files["release.json"].sha256 == self.release_identity,
            "Release index manifest identity differs"
        );
        ensure!(
            self.packages.len() == 2,
            "Expected CLI and environment npm artifacts"
        );
        for (kind, name) in [("cli", "@docouno/devkit"), ("environment", "@docouno/env")] {
            let package = self
                .packages
                .get(kind)
                .ok_or_else(|| anyhow::anyhow!("Missing npm artifact: {kind}"))?;
            ensure!(
                package.name == name && package.version == self.version,
                "Npm artifact identity differs: {kind}"
            );
            relative_path(&package.archive)?;
            ensure!(
                !package.archive.contains('/') && self.files.contains_key(&package.archive),
                "Npm artifact is absent: {kind}"
            );
            ensure!(
                package.integrity.starts_with("sha512-")
                    && package.shasum.len() == 40
                    && package
                        .shasum
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "Invalid npm artifact integrity: {kind}"
            );
        }
        Ok(())
    }
}
