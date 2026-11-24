//! Toolchain records are read at three boundaries: engine dispatch, the accepted
//! checkout, and the complete runtime shipped in a release.
use crate::{
    Result,
    delivery::EnginePin,
    files::{SHA256_RE, TOOLCHAIN_PINS_MAX_BYTES, read_regular_file, safe_path, version},
    model::ProjectState,
    state::{has_electron, uses_profile_scoped_runtime},
};
use anyhow::{Context, ensure};
use std::{collections::BTreeMap, path::Path};

const PINS_PATH: &str = ".devkit/runtime/toolchains.env";
const DESKTOP_HASH: &str = "DEVKIT_DESKTOP_RECIPE_HASH";

struct Records<'a>(BTreeMap<&'a str, &'a str>);

impl<'a> Records<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= TOOLCHAIN_PINS_MAX_BYTES,
            "Toolchain pins exceed byte budget"
        );
        let mut values = BTreeMap::new();
        for line in std::str::from_utf8(bytes)?.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .context("Invalid toolchains.env record")?;
            ensure!(
                key.starts_with("DEVKIT_")
                    && key
                        .bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
                    && !value.is_empty()
                    && !value
                        .chars()
                        .any(|c| c.is_whitespace() || ['\'', '"', '\0'].contains(&c))
                    && values.insert(key, value).is_none(),
                "Invalid/duplicate toolchain key: {key}"
            );
        }
        if let Some(pin) = values.get(DESKTOP_HASH) {
            ensure!(SHA256_RE.is_match(pin), "Invalid {DESKTOP_HASH}");
        }
        Ok(Self(values))
    }

    fn require(&self, key: &str) -> Result<&'a str> {
        self.0
            .get(key)
            .copied()
            .with_context(|| format!("Missing toolchain pin: {key}"))
    }

    fn engine(&self) -> Result<EnginePin> {
        let pin = EnginePin {
            version: self.require("DEVKIT_VERSION")?.into(),
            target: self.require("DEVKIT_ENGINE_TARGET")?.into(),
            sha256: self.require("DEVKIT_ENGINE_SHA256")?.into(),
        };
        pin.validate()?;
        Ok(pin)
    }
}

impl EnginePin {
    /// Dispatch only uses the engine identity. Other runtime keys can evolve
    /// independently in the engine selected by this file.
    pub fn from_toolchains(bytes: &[u8]) -> Result<Self> {
        Records::parse(bytes)?.engine()
    }
}

#[derive(Clone, Debug)]
pub struct CheckoutToolPins {
    pub engine: EnginePin,
    pub node_image: String,
    pub node_version: String,
    pub rust_version: String,
    pub rust_recipe: String,
    pub desktop_recipe: Option<String>,
}

impl CheckoutToolPins {
    fn from_records(records: &Records<'_>) -> Result<Self> {
        let engine = records.engine()?;
        let node_image = records.require("DEVKIT_NODE_IMAGE")?;
        let (name, digest) = node_image
            .rsplit_once("@sha256:")
            .context("Node image must be digest pinned")?;
        let rust_recipe = records.require("DEVKIT_RUST_RECIPE_HASH")?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"./:-".contains(&c))
                && SHA256_RE.is_match(digest)
                && SHA256_RE.is_match(rust_recipe),
            "Invalid immutable tool pins"
        );
        Ok(Self {
            engine,
            node_image: node_image.into(),
            node_version: version(records.require("DEVKIT_NODE_VERSION")?)?.into(),
            rust_version: version(records.require("DEVKIT_RUST_VERSION")?)?.into(),
            rust_recipe: rust_recipe.into(),
            desktop_recipe: records.0.get(DESKTOP_HASH).map(|value| (*value).to_owned()),
        })
    }

    pub fn admit(root: &Path, state: &ProjectState) -> Result<Self> {
        let (bytes, id) = read_regular_file(
            &safe_path(root, PINS_PATH)?,
            TOOLCHAIN_PINS_MAX_BYTES as u64,
            "toolchain pins",
        )?
        .with_context(|| format!("Missing {PINS_PATH}"))?;
        ensure!(
            state
                .managed
                .get(PINS_PATH)
                .is_some_and(|expected| id.matches_git_tracked(expected)),
            "Managed file conflict: {PINS_PATH}; restore with 'git checkout -- {PINS_PATH}' before running 'make sync' (or 'devkit sync')"
        );
        let pins = Self::from_records(&Records::parse(&bytes)?)?;
        ensure!(
            pins.engine.version == state.version,
            "Manifest version differs from accepted toolchains.env"
        );
        if has_electron(state) {
            let recipe = state
                .managed
                .get(".devkit/runtime/desktop.Dockerfile")
                .context("Missing managed desktop.Dockerfile")?;
            ensure!(
                pins.desktop_recipe.as_ref() == Some(&recipe.sha256),
                "{DESKTOP_HASH} differs from the managed desktop.Dockerfile"
            );
        } else if uses_profile_scoped_runtime(&state.version)? {
            ensure!(
                pins.desktop_recipe.is_none(),
                "Unexpected {DESKTOP_HASH} in a checkout without Electron"
            );
        } else {
            ensure!(
                pins.desktop_recipe.is_some(),
                "Missing toolchain pin: {DESKTOP_HASH}"
            );
        }
        Ok(pins)
    }
}

#[derive(Debug)]
pub struct ReleaseToolPins {
    pub engine: EnginePin,
    pub node_image: String,
    pub desktop_recipe: String,
}

impl ReleaseToolPins {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let records = Records::parse(bytes)?;
        let pins = CheckoutToolPins::from_records(&records)?;
        let desktop_recipe = pins
            .desktop_recipe
            .with_context(|| format!("Missing toolchain pin: {DESKTOP_HASH}"))?;
        ensure!(records.0.len() == 8, "Unexpected release toolchain pins");
        Ok(Self {
            engine: pins.engine,
            node_image: pins.node_image,
            desktop_recipe,
        })
    }
}
