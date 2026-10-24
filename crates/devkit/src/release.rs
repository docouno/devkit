use crate::{Result, files::*, model::Release};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha512};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct AdmittedRelease {
    pub root: PathBuf,
    pub manifest: Release,
    pub identity: String,
}
pub fn release_identity(root: &Path) -> Result<String> {
    Ok(digest(fs::read(safe_path(root, "release.json")?)?))
}
fn parse_release(root: &Path) -> Result<Release> {
    let release: Release = read_json(&safe_path(root, "release.json")?)?;
    ensure!(
        release.schema_version == 1,
        "Unsupported release schemaVersion"
    );
    version(&release.version)?;
    version(&release.rust_version)?;
    ensure!(
        release.environment.name == "@docouno/env"
            && release.environment.version == release.version,
        "Environment package identity does not match this release"
    );
    relative_path(&release.environment.archive)?;
    ensure!(!release.files.is_empty(), "Release has no payload");
    for (name, id) in &release.files {
        relative_path(name)?;
        validate_identity(id)?;
        ensure!(
            name != "release.json",
            "Release manifest cannot hash itself"
        );
        ensure!(
            [0o644, 0o755].contains(&id.mode),
            "Invalid release file mode: {name}"
        );
    }
    ensure!(
        release.files.contains_key(&release.environment.archive),
        "Environment archive is not in the release"
    );
    Ok(release)
}
pub fn admit_release(root: &Path) -> Result<AdmittedRelease> {
    let root = project_root(root)?;
    let before =
        identity(&safe_path(&root, "release.json")?)?.context("Missing release manifest")?;
    let manifest = parse_release(&root)?;
    let mut actual = file_map(&root)?;
    actual.remove("release.json");
    ensure!(
        actual == manifest.files,
        "Release payload file set or integrity differs from its manifest"
    );
    let archive = fs::read(safe_path(&root, &manifest.environment.archive)?)?;
    let sri = format!("sha512-{}", STANDARD.encode(Sha512::digest(&archive)));
    ensure!(
        sri == manifest.environment.integrity,
        "Environment archive integrity mismatch"
    );
    if manifest.files.contains_key("package.json") {
        let package: serde_json::Value = read_json(&safe_path(&root, "package.json")?)?;
        ensure!(
            package["name"] == "@docouno/devkit" && package["version"] == manifest.version,
            "CLI package identity does not match release"
        );
    }
    ensure!(
        identity(&safe_path(&root, "release.json")?)?.as_ref() == Some(&before),
        "Release manifest changed during admission"
    );
    Ok(AdmittedRelease {
        identity: before.sha256,
        root,
        manifest,
    })
}
pub fn read_release(root: &Path) -> Result<Release> {
    Ok(admit_release(root)?.manifest)
}
impl AdmittedRelease {
    pub fn bytes(&self, name: &str) -> Result<Vec<u8>> {
        let expected = self
            .manifest
            .files
            .get(name)
            .with_context(|| format!("Release is missing {name}"))?;
        let path = safe_path(&self.root, name)?;
        ensure!(
            identity(&path)?.as_ref() == Some(expected),
            "Release payload changed: {name}"
        );
        let bytes = fs::read(path)?;
        ensure!(
            digest(&bytes) == expected.sha256,
            "Release payload changed: {name}"
        );
        Ok(bytes)
    }
    pub fn recheck(&self) -> Result<()> {
        let now = admit_release(&self.root)?;
        ensure!(
            now.identity == self.identity,
            "Release changed while staging"
        );
        Ok(())
    }
}
/// npm preserves bytes but may restrict mode bits under umask077. Verify every
/// regular byte and executable class before normalizing a private copy.
pub fn import_npm_payload(source: &Path, target: &Path) -> Result<AdmittedRelease> {
    let source = project_root(source)?;
    let manifest = parse_release(&source)?;
    let mut actual = file_map(&source)?;
    actual.remove("release.json");
    ensure!(
        actual.keys().eq(manifest.files.keys()),
        "Npm payload file set differs from its manifest"
    );
    for (name, expected) in &manifest.files {
        let found = &actual[name];
        ensure!(
            found.sha256 == expected.sha256
                && (found.mode & 0o111 != 0) == (expected.mode & 0o111 != 0),
            "Npm payload integrity/executable class mismatch: {name}"
        );
    }
    let identity = release_identity(&source)?;
    ensure!(
        !exists(target)? || sorted_entries(target)?.is_empty(),
        "Npm import destination must be empty"
    );
    fs::create_dir_all(target)?;
    for (name, expected) in &manifest.files {
        let bytes = fs::read(safe_path(&source, name)?)?;
        ensure!(
            digest(&bytes) == expected.sha256,
            "Npm payload changed during import: {name}"
        );
        write_atomic(&safe_path(target, name)?, &bytes, expected.mode)?;
    }
    let bytes = fs::read(safe_path(&source, "release.json")?)?;
    ensure!(
        digest(&bytes) == identity,
        "Npm manifest changed during import"
    );
    write_atomic(&safe_path(target, "release.json")?, &bytes, 0o644)?;
    admit_release(target)
}
