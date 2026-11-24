//! Accepted engine and tool records. The legacy keys describe the x86-64
//! artifact so older engines can read the same immutable release.
use crate::{
    Result,
    delivery::EnginePin,
    distribution::{Target, missing_target, valid_target_triple},
    files::{SHA256_RE, SemVer, input_path, project_root, read_regular_file, version},
    model::{FileIdentity, ProjectState, ToolingProfile},
};
use anyhow::{Context, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(crate) fn managed_bytes(
    root: &Path,
    name: &str,
    managed: &BTreeMap<String, FileIdentity>,
) -> Result<Vec<u8>> {
    let conflict = || {
        format!(
            "Managed file conflict: {name}; restore with 'git checkout -- {name}' before running 'make sync' (or 'devkit sync')"
        )
    };
    let expected = managed.get(name).with_context(conflict)?;
    let (bytes, id) = read_regular_file(&input_path(root, name)?, 65536, "toolchain pin file")?
        .with_context(conflict)?;
    ensure!(id.matches_git_tracked(expected), "{}", conflict());
    Ok(bytes)
}

pub(crate) struct Records<'a>(pub(crate) BTreeMap<&'a str, &'a str>);

impl<'a> Records<'a> {
    pub(crate) fn parse(bytes: &'a [u8], prefix: &str) -> Result<Self> {
        ensure!(bytes.len() <= 65536, "Toolchain pins exceed byte budget");
        let mut values = BTreeMap::new();
        for line in std::str::from_utf8(bytes)?.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .context("Invalid toolchain pin record")?;
            ensure!(
                key.starts_with(prefix)
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    && !value.is_empty()
                    && !value
                        .chars()
                        .any(|c| c.is_whitespace() || ['\'', '"', '\0'].contains(&c))
                    && values.insert(key, value).is_none(),
                "Invalid or duplicate toolchain pin: {key}"
            );
        }
        Ok(Self(values))
    }

    pub(crate) fn require(&self, key: &str) -> Result<&'a str> {
        self.0
            .get(key)
            .copied()
            .with_context(|| format!("Missing toolchain pin: {key}"))
    }

    /// Interpret the optional extension only when its complete set of columns
    /// agrees with the declared target list. Unknown triples can be carried
    /// forward, but only a known Target can be selected for execution.
    fn targets(&self, list: &str, columns: &[&str]) -> Result<Vec<(&'a str, String)>> {
        let mut targets = Vec::new();
        let mut expected = BTreeSet::new();
        if let Some(triples) = self.0.get(list) {
            for triple in triples.split(',') {
                ensure!(
                    valid_target_triple(triple),
                    "Invalid target triple in {list}"
                );
                let suffix = triple.to_ascii_uppercase().replace('-', "_");
                for column in columns {
                    let key = format!("{column}_{suffix}");
                    ensure!(
                        expected.insert(key.clone()),
                        "Duplicate target in {list}: {triple}"
                    );
                    self.require(&key)?;
                }
                targets.push((triple, suffix));
            }
            ensure!(
                targets.iter().any(|(t, _)| *t == Target::LinuxX64.triple()),
                "Missing x86-64 compatibility target in {list}"
            );
        }
        for key in self.0.keys() {
            if columns.iter().any(|column| {
                key.strip_prefix(column)
                    .is_some_and(|tail| tail.starts_with('_'))
            }) {
                ensure!(expected.contains(*key), "Undeclared target pin: {key}");
            }
        }
        Ok(targets)
    }
}

#[derive(Clone, Debug)]
pub struct EnginePins {
    pub version: String,
    hashes: BTreeMap<String, String>,
}

impl EnginePins {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::from_records(&Records::parse(bytes, "DEVKIT_")?)
    }

    fn from_records(records: &Records<'_>) -> Result<Self> {
        let version = version(records.require("DEVKIT_VERSION")?)?.to_owned();
        let legacy = records.require("DEVKIT_ENGINE_SHA256")?;
        let x64 = Target::LinuxX64.triple();
        ensure!(
            records.require("DEVKIT_ENGINE_TARGET")? == x64 && SHA256_RE.is_match(legacy),
            "Invalid legacy engine pin"
        );
        let mut hashes = BTreeMap::from([(x64.to_owned(), legacy.to_owned())]);
        for (triple, suffix) in
            records.targets("DEVKIT_ENGINE_TARGETS", &["DEVKIT_ENGINE_SHA256"])?
        {
            let sha256 = records.require(&format!("DEVKIT_ENGINE_SHA256_{suffix}"))?;
            ensure!(
                SHA256_RE.is_match(sha256) && (triple != x64 || sha256 == legacy),
                "Invalid or contradictory engine pin for {triple}"
            );
            hashes.insert(triple.to_owned(), sha256.to_owned());
        }
        Ok(Self { version, hashes })
    }

    pub fn hashes(&self) -> &BTreeMap<String, String> {
        &self.hashes
    }

    pub fn for_target(&self, target: Target) -> Result<EnginePin> {
        let triple = target.triple();
        let sha = self
            .hashes
            .get(triple)
            .ok_or_else(|| missing_target(&self.version, target))?;
        Ok(EnginePin {
            version: self.version.clone(),
            target: triple.into(),
            sha256: sha.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolPins {
    pub version: String,
    pub node_image: String,
    pub node_version: String,
    pub rust_version: String,
    pub rust_recipe: String,
    pub desktop_recipe: Option<String>,
    pub engines: EnginePins,
}

impl ToolPins {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let records = Records::parse(bytes, "DEVKIT_")?;
        let engines = EnginePins::from_records(&records)?;
        let node_image = records.require("DEVKIT_NODE_IMAGE")?;
        let (name, hash) = node_image
            .rsplit_once("@sha256:")
            .context("Node image must be digest pinned")?;
        let rust_recipe = records.require("DEVKIT_RUST_RECIPE_HASH")?;
        let desktop_recipe = records.0.get("DEVKIT_DESKTOP_RECIPE_HASH").copied();
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"./:-".contains(&b))
                && SHA256_RE.is_match(hash)
                && SHA256_RE.is_match(rust_recipe)
                && desktop_recipe.is_none_or(|pin| SHA256_RE.is_match(pin))
                && (desktop_recipe.is_some()
                    || SemVer::parse(&engines.version)? >= SemVer::parse("0.6.0")?),
            "Invalid immutable tool pins"
        );
        Ok(Self {
            version: engines.version.clone(),
            engines,
            node_image: node_image.into(),
            node_version: version(records.require("DEVKIT_NODE_VERSION")?)?.into(),
            rust_version: version(records.require("DEVKIT_RUST_VERSION")?)?.into(),
            rust_recipe: rust_recipe.into(),
            desktop_recipe: desktop_recipe.map(str::to_owned),
        })
    }

    pub fn host_engine(&self) -> Result<EnginePin> {
        self.engines.for_target(Target::host()?)
    }

    pub fn admit(root: &Path, state: &ProjectState) -> Result<Self> {
        let pins = Self::parse(&managed_bytes(
            root,
            ".devkit/runtime/toolchains.env",
            &state.managed,
        )?)?;
        ensure!(
            pins.version == state.version,
            "Manifest version differs from accepted toolchains.env"
        );
        if state
            .components
            .values()
            .any(|c| c.tooling_profile == ToolingProfile::Electron)
        {
            let recipe = state
                .managed
                .get(".devkit/runtime/desktop.Dockerfile")
                .context("Missing managed desktop recipe")?;
            ensure!(
                pins.desktop_recipe.as_ref() == Some(&recipe.sha256),
                "Desktop recipe differs from accepted pin"
            );
        }
        Ok(pins)
    }

    pub fn parse_release(bytes: &[u8]) -> Result<Self> {
        let pins = Self::parse(bytes)?;
        ensure!(
            pins.desktop_recipe.is_some(),
            "Missing release desktop recipe pin"
        );
        Ok(pins)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PnpmPin {
    pub package: String,
    pub version: String,
    pub tarball_sha256: String,
    pub binary_sha256: String,
}

impl PnpmPin {
    const COLUMNS: [&'static str; 3] = [
        "DEVKIT_PNPM_PACKAGE",
        "DEVKIT_PNPM_TARBALL_SHA256",
        "DEVKIT_PNPM_SHA256",
    ];

    pub fn target(&self) -> Result<Target> {
        Target::from_pnpm_package(&self.package)
            .with_context(|| format!("Unsupported pnpm package: {}", self.package))
    }

    pub fn validate(&self) -> Result<()> {
        version(&self.version)?;
        self.target()?;
        ensure!(
            SHA256_RE.is_match(&self.binary_sha256) && SHA256_RE.is_match(&self.tarball_sha256),
            "Invalid immutable pnpm pins"
        );
        Ok(())
    }

    fn select(records: &Records<'_>, target: Target) -> Result<Self> {
        let version = version(records.require("DEVKIT_PNPM_VERSION")?)?;
        let read = |suffix: &str| -> Result<Self> {
            let [pkg, tarball, binary] = Self::COLUMNS;
            let pin = Self {
                package: records.require(&format!("{pkg}{suffix}"))?.into(),
                version: version.into(),
                tarball_sha256: records.require(&format!("{tarball}{suffix}"))?.into(),
                binary_sha256: records.require(&format!("{binary}{suffix}"))?.into(),
            };
            ensure!(
                pin.package
                    .strip_prefix("@pnpm/exe.")
                    .is_some_and(|name| !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
                    && SHA256_RE.is_match(&pin.tarball_sha256)
                    && SHA256_RE.is_match(&pin.binary_sha256),
                "Invalid immutable pnpm pins"
            );
            Ok(pin)
        };
        let legacy = read("")?;
        ensure!(
            legacy.target()? == Target::LinuxX64,
            "Invalid legacy pnpm package"
        );
        let mut selected = (target == Target::LinuxX64).then(|| legacy.clone());
        for (triple, suffix) in records.targets("DEVKIT_PNPM_TARGETS", &Self::COLUMNS)? {
            let pin = read(&format!("_{suffix}"))?;
            if let Ok(known) = Target::parse(triple) {
                ensure!(
                    pin.target()? == known,
                    "pnpm package differs from target {triple}"
                );
            }
            ensure!(
                triple != Target::LinuxX64.triple() || pin == legacy,
                "Contradictory legacy pnpm pins"
            );
            if triple == target.triple() {
                selected = Some(pin);
            }
        }
        selected.with_context(|| format!("Missing pnpm pin for {}", target.triple()))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::select(&Records::parse(bytes, "DEVKIT_PNPM_")?, Target::LinuxX64)
    }
    pub fn from_source_pins(root: &Path) -> Result<Self> {
        Self::from_pin_file_for_target(&input_path(root, "toolchains.env")?, Target::LinuxX64)
    }
    pub fn release_env_from_source(root: &Path) -> Result<String> {
        let bytes = Self::read_pin_file(&input_path(root, "toolchains.env")?)?;
        let records = Records::parse(&bytes, "DEVKIT_")?;
        let pins = Target::LINUX
            .into_iter()
            .map(|target| Ok((target, Self::select(&records, target)?)))
            .collect::<Result<Vec<_>>>()?;
        let mut output = pins[0].1.encode();
        let triples = Target::LINUX
            .into_iter()
            .map(Target::triple)
            .collect::<Vec<_>>()
            .join(",");
        output.push_str(&format!("DEVKIT_PNPM_TARGETS={triples}\n"));
        for (target, pin) in pins {
            let values = [pin.package, pin.tarball_sha256, pin.binary_sha256];
            for (column, value) in Self::COLUMNS.into_iter().zip(values) {
                output.push_str(&format!("{column}_{}={value}\n", target.key_suffix()));
            }
        }
        Ok(output)
    }
    pub fn from_release_for_target(
        release: &crate::release::AdmittedRelease,
        target: Target,
    ) -> Result<Self> {
        Self::select(
            &Records::parse(&release.bytes("runtime/pnpm.env")?, "DEVKIT_PNPM_")?,
            target,
        )
    }
    pub fn from_checkout_for_target(root: &Path, target: Target) -> Result<Self> {
        let header = crate::state::read_accepted_header(root)?;
        let bytes = managed_bytes(root, ".devkit/runtime/pnpm.env", &header.managed)?;
        Self::select(&Records::parse(&bytes, "DEVKIT_PNPM_")?, target)
    }
    pub fn from_pin_file_for_target(path: &Path, target: Target) -> Result<Self> {
        let bytes = Self::read_pin_file(path)?;
        let source = std::str::from_utf8(&bytes)?
            .lines()
            .any(|l| l.starts_with("DEVKIT_NODE_IMAGE="));
        let prefix = if source { "DEVKIT_" } else { "DEVKIT_PNPM_" };
        let mut records = Records::parse(&bytes, prefix)?;
        // The 0.5.x author file implied this package. Its release projection
        // already wrote the explicit name.
        if source && !records.0.contains_key("DEVKIT_PNPM_TARGETS") {
            records
                .0
                .entry("DEVKIT_PNPM_PACKAGE")
                .or_insert(Target::LinuxX64.pnpm_package().unwrap());
        }
        Self::select(&records, target)
    }
    fn read_pin_file(path: &Path) -> Result<Vec<u8>> {
        let path = project_root(path)?;
        let (bytes, _) =
            read_regular_file(&path, 65536, "pnpm pin file")?.context("Missing pnpm pin file")?;
        Ok(bytes)
    }
    pub fn encode(&self) -> String {
        format!(
            "DEVKIT_PNPM_PACKAGE={}\nDEVKIT_PNPM_SHA256={}\nDEVKIT_PNPM_TARBALL_SHA256={}\nDEVKIT_PNPM_VERSION={}\n",
            self.package, self.binary_sha256, self.tarball_sha256, self.version
        )
    }
}
