//! Wire identities shared by the native consumer and the author assembler.
use crate::{Result, files::*};
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const TARGET: &str = "x86_64-unknown-linux-musl";
pub const ENGINE_ASSET: &str = "devkit-x86_64-unknown-linux-musl";
pub const RELEASE_BASE: &str = "https://github.com/docouno/devkit/releases/download";

/// Keep the original x86-64 asset and payload names readable by 0.5.x.
/// Darwin joins the publication matrix after native Docker acceptance.
pub const REQUIRED_PUBLISHABLE_TARGETS: &[Target] = &[Target::LinuxX64, Target::LinuxArm64];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Target {
    LinuxX64,
    LinuxArm64,
    DarwinArm64,
}

impl Target {
    pub const ALL: [Self; 3] = [Self::LinuxX64, Self::LinuxArm64, Self::DarwinArm64];

    pub fn host() -> Result<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Ok(Self::LinuxX64),
            ("linux", "aarch64") => Ok(Self::LinuxArm64),
            ("macos", "aarch64") => Ok(Self::DarwinArm64),
            (os, arch) => bail!("Unsupported Devkit host: {os}/{arch}"),
        }
    }

    pub fn parse(triple: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.triple() == triple)
            .with_context(|| format!("Unsupported native target: {triple}"))
    }

    pub fn triple(self) -> &'static str {
        match self {
            Self::LinuxX64 => TARGET,
            Self::LinuxArm64 => "aarch64-unknown-linux-musl",
            Self::DarwinArm64 => "aarch64-apple-darwin",
        }
    }

    pub fn asset(self) -> &'static str {
        match self {
            Self::LinuxX64 => ENGINE_ASSET,
            Self::LinuxArm64 => "devkit-aarch64-unknown-linux-musl",
            Self::DarwinArm64 => "devkit-aarch64-apple-darwin",
        }
    }

    pub fn payload_name(self) -> &'static str {
        match self {
            Self::LinuxX64 => "devkit",
            _ => self.asset(),
        }
    }

    pub fn key_suffix(self) -> &'static str {
        match self {
            Self::LinuxX64 => "X86_64_UNKNOWN_LINUX_MUSL",
            Self::LinuxArm64 => "AARCH64_UNKNOWN_LINUX_MUSL",
            Self::DarwinArm64 => "AARCH64_APPLE_DARWIN",
        }
    }
}

/// Wire readers can admit future triples without claiming to execute them.
pub fn valid_target_triple(triple: &str) -> bool {
    triple.len() <= 96
        && triple.contains('-')
        && triple.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

pub fn missing_target(version: &str, target: Target) -> anyhow::Error {
    let current = env!("CARGO_PKG_VERSION");
    crate::cli::error(
        "UNSUPPORTED_HOST_TARGET",
        format!(
            "Devkit {version} has no engine for {}. Upgrade the checkout with: npx @docouno/devkit@{current} update . --to {current}",
            target.triple()
        ),
    )
}

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
        target: if cfg!(any(target_os = "macos", target_env = "musl")) {
            Target::host()
                .map(Target::triple)
                .unwrap_or("unsupported-build-target")
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

/// The checksum list is itself pinned by the outer index.
pub fn checksums(files: &BTreeMap<String, AssetIdentity>) -> String {
    files
        .iter()
        .filter(|(name, _)| name.as_str() != "checksums.txt")
        .map(|(name, identity)| format!("{}  {name}\n", identity.sha256))
        .collect()
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
    #[serde(
        default,
        deserialize_with = "engine_map",
        skip_serializing_if = "Option::is_none"
    )]
    pub engines: Option<BTreeMap<String, String>>,
    pub archive: String,
    pub packages: BTreeMap<String, NpmArtifact>,
    pub files: BTreeMap<String, AssetIdentity>,
}

// Absence selects the historical single-target wire format. A present field
// must be an unambiguous object, including when different JSON readers are used.
fn engine_map<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Option<BTreeMap<String, String>>, D::Error> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            fmt.write_str("an object of unique engine targets")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut engines = BTreeMap::new();
            while let Some((target, asset)) = map.next_entry()? {
                if engines.insert(target, asset).is_some() {
                    return Err(serde::de::Error::custom("Duplicate release engine target"));
                }
            }
            Ok(engines)
        }
    }
    de.deserialize_map(Entries).map(Some)
}

impl ReleaseIndex {
    pub fn engine_asset(&self, target: Target) -> Result<&str> {
        let asset = match &self.engines {
            Some(engines) => engines.get(target.triple()),
            None if self.target == target.triple() => Some(&self.engine),
            None => None,
        };
        asset
            .map(String::as_str)
            .ok_or_else(|| missing_target(&self.version, target))
    }

    /// Publication is stricter than consumption. New readers still accept
    /// historical x86-only releases, while new publications must be complete.
    pub fn validate_publishable_matrix(&self, targets: &[Target]) -> Result<()> {
        self.validate()?;
        let engines = self
            .engines
            .as_ref()
            .context("Missing publishable engine matrix")?;
        ensure!(
            engines.len() == targets.len()
                && targets.iter().all(|t| engines.contains_key(t.triple())),
            "Release does not contain exactly the required publishable targets"
        );
        for asset in self.files.keys() {
            if asset.starts_with("devkit-") && asset != &self.archive {
                ensure!(
                    targets.iter().any(|t| t.asset() == asset),
                    "Unlisted native asset: {asset}"
                );
            }
        }
        Ok(())
    }

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
        if let Some(engines) = &self.engines {
            ensure!(
                engines.get(&self.target) == Some(&self.engine),
                "Inconsistent legacy engine identity"
            );
            for (target, asset) in engines {
                ensure!(
                    valid_target_triple(target)
                        && *asset == format!("devkit-{target}")
                        && self.files.contains_key(asset),
                    "Invalid or missing engine asset for target {target}"
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn legacy_index() -> ReleaseIndex {
        let sha = "a".repeat(64);
        let assets = [
            ENGINE_ASSET,
            "devkit-0.5.1.tar.gz",
            "env.tgz",
            "cli.tgz",
            "release.json",
            "install.sh",
            "bootstrap.sh",
            "checksums.txt",
        ];
        let files: BTreeMap<_, _> = assets
            .into_iter()
            .map(|a| (a, json!({"sha256": sha, "size": 48})))
            .collect();
        let npm = |name, archive| json!({"name": name, "version": "0.5.1", "archive": archive, "integrity":"sha512-test", "shasum": "b".repeat(40)});
        serde_json::from_value(json!({
            "schemaVersion": 1, "version": "0.5.1", "target": TARGET,
            "buildRun": "tests", "sourceCommit": "a".repeat(40), "sourceTree": "b".repeat(40),
            "publicationEligible": true, "releaseIdentity": sha, "engine": ENGINE_ASSET,
            "archive": "devkit-0.5.1.tar.gz", "files": files,
            "packages": {"cli": npm("@docouno/devkit", "cli.tgz"), "environment": npm("@docouno/env", "env.tgz")}
        })).unwrap()
    }

    #[test]
    fn wire_reader_accepts_historical_and_future_indices_but_publication_needs_exact_targets() {
        let mut index = legacy_index();
        index.validate().unwrap();
        assert_eq!(index.engine_asset(Target::LinuxX64).unwrap(), ENGINE_ASSET);
        assert!(index.engine_asset(Target::LinuxArm64).is_err());
        assert!(
            index
                .validate_publishable_matrix(REQUIRED_PUBLISHABLE_TARGETS)
                .is_err()
        );
        let arm = Target::LinuxArm64;
        index
            .files
            .insert(arm.asset().into(), index.files[ENGINE_ASSET].clone());
        index.engines = Some(BTreeMap::from([
            (TARGET.into(), ENGINE_ASSET.into()),
            (arm.triple().into(), arm.asset().into()),
        ]));
        index
            .validate_publishable_matrix(REQUIRED_PUBLISHABLE_TARGETS)
            .unwrap();
        assert_eq!(index.engine_asset(arm).unwrap(), arm.asset());
        for (target, asset) in [
            (Target::DarwinArm64.triple(), Target::DarwinArm64.asset()),
            (
                "riscv64gc-unknown-linux-musl",
                "devkit-riscv64gc-unknown-linux-musl",
            ),
        ] {
            index
                .engines
                .as_mut()
                .unwrap()
                .insert(target.into(), asset.into());
            assert!(index.validate().is_err(), "unlisted file");
            index
                .files
                .insert(asset.into(), index.files[ENGINE_ASSET].clone());
            index.validate().unwrap();
            assert!(
                index
                    .validate_publishable_matrix(REQUIRED_PUBLISHABLE_TARGETS)
                    .is_err()
            );
        }
        index
            .engines
            .as_mut()
            .unwrap()
            .insert(TARGET.into(), arm.asset().into());
        assert!(index.validate().is_err(), "legacy identity must agree");
    }

    #[test]
    fn only_an_absent_engine_map_selects_the_legacy_format() {
        let index = serde_json::to_string(&legacy_index()).unwrap();
        for map in [
            "null",
            r#"{"future-target":"first","future-target":"second"}"#,
        ] {
            let raw = format!(r#"{{"engines":{map},{}"#, &index[1..]);
            assert!(serde_json::from_str::<ReleaseIndex>(&raw).is_err());
        }
    }
}
