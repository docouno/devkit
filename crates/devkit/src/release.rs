use crate::{
    Result,
    files::*,
    model::{FileIdentity, MigrationEntry, Release, ToolingProfile},
};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use sha2::{Digest, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub fn markdown_heading_anchors(markdown: &str) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut in_fence: Option<char> = None;
    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim_start();
        if let Some(fence_char) = in_fence {
            if trimmed.starts_with(&fence_char.to_string().repeat(3)) {
                in_fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            in_fence = Some('`');
            continue;
        }
        if trimmed.starts_with("~~~") {
            in_fence = Some('~');
            continue;
        }
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if !(1..=6).contains(&hashes) {
            continue;
        }
        let after = &trimmed[hashes..];
        if !after.starts_with(' ') && !after.starts_with('\t') {
            continue;
        }
        let heading = after.trim().trim_end_matches('#').trim();
        if heading.is_empty() {
            continue;
        }
        let mut base = String::new();
        for ch in heading.chars() {
            for lower in ch.to_lowercase() {
                if lower.is_alphanumeric() || lower == '-' || lower == '_' {
                    base.push(lower);
                } else if lower.is_whitespace() {
                    base.push('-');
                }
            }
        }
        if base.is_empty() {
            continue;
        }
        let entry = counts.entry(base.clone()).or_insert(0);
        let slug = if *entry == 0 {
            base
        } else {
            format!("{base}-{}", *entry)
        };
        *entry += 1;
        anchors.insert(slug);
    }
    anchors
}

pub fn validate_migrations(
    release_version: &str,
    entries: &[MigrationEntry],
    root: &Path,
    files: Option<&BTreeMap<String, FileIdentity>>,
    strict_profiles: bool,
) -> Result<()> {
    let target_semver = SemVer::parse(release_version)?;
    let mut seen_ids = BTreeSet::new();
    for entry in entries {
        slug(&entry.id).with_context(|| format!("Invalid migration id: {}", entry.id))?;
        ensure!(
            seen_ids.insert(entry.id.as_str()),
            "Duplicate migration id: {}",
            entry.id
        );
        let entry_semver = SemVer::parse(&entry.version).with_context(|| {
            format!(
                "Invalid migration version in {}: {}",
                entry.id, entry.version
            )
        })?;
        ensure!(
            entry_semver <= target_semver,
            "Migration {} targets version {} newer than release {}",
            entry.id,
            entry.version,
            release_version
        );
        ensure!(
            !entry.title.is_empty()
                && entry.title.chars().count() <= 120
                && !entry.title.contains(['\r', '\n']),
            "Migration {} title must be a single line of at most 120 characters",
            entry.id
        );
        ensure!(
            !entry.summary.is_empty()
                && entry.summary.chars().count() <= 240
                && !entry.summary.contains(['\r', '\n']),
            "Migration {} summary must be a single line of at most 240 characters",
            entry.id
        );
        let mut seen_profiles = BTreeSet::new();
        for profile in &entry.profiles {
            ensure!(
                !profile.is_empty() && seen_profiles.insert(profile.as_str()),
                "Migration {} contains empty or duplicate profile {}",
                entry.id,
                profile
            );
            if strict_profiles {
                serde_json::from_value::<ToolingProfile>(json!(profile)).with_context(|| {
                    format!(
                        "Migration {} references unknown profile {}",
                        entry.id, profile
                    )
                })?;
            }
        }
        relative_path(&entry.doc_path)?;
        ensure!(
            entry.doc_path.starts_with("docs/migrations/") && entry.doc_path.ends_with(".md"),
            "Migration {} docPath must be under docs/migrations/*.md: {}",
            entry.id,
            entry.doc_path
        );
        if let Some(file_set) = files {
            ensure!(
                file_set.contains_key(&entry.doc_path),
                "Migration {} docPath is absent from release manifest: {}",
                entry.id,
                entry.doc_path
            );
        }
        ensure!(
            !entry.anchor.is_empty()
                && !entry.anchor.starts_with('#')
                && entry
                    .anchor
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "Migration {} has invalid anchor: {}",
            entry.id,
            entry.anchor
        );
        let doc_file = safe_path(root, &entry.doc_path)?;
        ensure!(
            exists(&doc_file)?,
            "Migration {} document is missing: {}",
            entry.id,
            entry.doc_path
        );
        let markdown = fs::read_to_string(&doc_file)
            .with_context(|| format!("Cannot read migration document {}", entry.doc_path))?;
        let anchors = markdown_heading_anchors(&markdown);
        ensure!(
            anchors.contains(&entry.anchor),
            "Migration {} anchor #{} not found in {}",
            entry.id,
            entry.anchor,
            entry.doc_path
        );
    }
    Ok(())
}

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
    validate_migrations(
        &release.version,
        &release.migrations,
        root,
        Some(&release.files),
        false,
    )?;
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
