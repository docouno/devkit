//! Author-only collection of the license text belonging to the actual Cargo
//! resolution and the pinned Rust standard library. No license text is guessed
//! from an SPDX identifier.
use crate::{
    Result,
    archive::publish_new_directory,
    distribution::{BuildInfo, TARGET, build_info},
    files::*,
    model::FileIdentity,
    processes::{OutputMode, ProcessRunner},
};
use anyhow::{Context, ensure};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const MAX_CRATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CRATE_EXPANDED: u64 = 256 * 1024 * 1024;
const MAX_NOTICE_BYTES: u64 = 16 * 1024 * 1024;

pub type NoticeTexts = BTreeMap<String, Vec<u8>>;

pub struct NoticeOptions {
    pub root: PathBuf,
    pub cargo_home: PathBuf,
    pub rust_docs: PathBuf,
    pub output: PathBuf,
    pub target: String,
    pub rust_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoticeSource {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageNotices {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub license: Option<String>,
    pub archive_sha256: String,
    pub notices: BTreeMap<String, NoticeSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoticeSet {
    pub schema_version: u32,
    pub target: String,
    pub rust_version: String,
    pub source_cargo_lock_sha256: String,
    pub generator: BuildInfo,
    pub cargo_metadata_sha256: String,
    pub resolved_ids: Vec<String>,
    pub roots: Vec<String>,
    pub packages: Vec<PackageNotices>,
    pub standard_library: BTreeMap<String, NoticeSource>,
    pub files: BTreeMap<String, FileIdentity>,
}

fn new_output(root: &Path, output: &Path) -> Result<PathBuf> {
    let output = project_root(output)?;
    ensure!(!exists(&output)?, "Notice output must be a new directory");
    ensure!(
        output.parent().is_some_and(Path::is_dir),
        "Notice output parent must already exist"
    );
    ensure!(
        !root.starts_with(&output) && output != root,
        "Notice output overlaps its source"
    );
    Ok(output)
}

fn text_output(command: &mut Command, runner: &ProcessRunner) -> Result<String> {
    let output = runner.checked_with_timeout(
        command,
        OutputMode::Capture(1024 * 1024),
        Duration::from_secs(30),
    )?;
    Ok(output.text()?.trim().to_owned())
}

fn lock_checksums(root: &Path) -> Result<BTreeMap<(String, String, String), String>> {
    let lock: toml::Value = toml::from_str(&fs::read_to_string(safe_path(root, "Cargo.lock")?)?)?;
    let mut result = BTreeMap::new();
    for package in lock
        .get("package")
        .and_then(toml::Value::as_array)
        .context("Cargo.lock package table is missing")?
    {
        let value = |key| -> Result<String> {
            Ok(package
                .get(key)
                .and_then(toml::Value::as_str)
                .with_context(|| format!("Cargo.lock package {key} is missing"))?
                .to_owned())
        };
        if package.get("source").is_some() {
            let key = (value("name")?, value("version")?, value("source")?);
            let checksum = value("checksum")?;
            ensure!(
                SHA256_RE.is_match(&checksum),
                "Invalid Cargo archive checksum"
            );
            ensure!(
                result.insert(key, checksum).is_none(),
                "Duplicate Cargo.lock package"
            );
        }
    }
    Ok(result)
}

/// Cargo's target-filtered resolved edges remain the authority. This walk does
/// not attempt to interpret target expressions or resolve versions itself.
pub fn resolved_packages(metadata: &Value) -> Result<(Vec<String>, Vec<Value>)> {
    let packages = metadata["packages"]
        .as_array()
        .context("Cargo metadata packages missing")?;
    let package_map = packages
        .iter()
        .map(|p| Ok((text(&p["id"], "Cargo package id")?.to_owned(), p.clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("Cargo resolved nodes missing")?;
    let node_map = nodes
        .iter()
        .map(|node| Ok((text(&node["id"], "Cargo node id")?.to_owned(), node)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut roots = metadata["workspace_members"]
        .as_array()
        .context("Cargo workspace members missing")?
        .iter()
        .map(|id| Ok(text(id, "Cargo workspace member")?.to_owned()))
        .collect::<Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    ensure!(!roots.is_empty(), "Cargo workspace has no roots");
    let mut queue = VecDeque::from(roots.clone());
    let mut seen = BTreeSet::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure!(
            seen.len() <= 4096,
            "Cargo notice closure exceeds package budget"
        );
        let node = node_map
            .get(&id)
            .context("Resolved Cargo node is missing")?;
        ensure!(
            package_map.contains_key(&id),
            "Resolved Cargo package is missing"
        );
        for dep in node["deps"].as_array().context("Cargo node deps missing")? {
            let kinds = dep["dep_kinds"]
                .as_array()
                .context("Cargo dependency kinds missing")?;
            let relevant = kinds
                .iter()
                .any(|kind| kind["kind"].is_null() || kind["kind"].as_str() == Some("build"));
            if relevant {
                queue.push_back(text(&dep["pkg"], "Cargo dependency package")?.to_owned());
            }
        }
    }
    let resolved = seen
        .iter()
        .map(|id| {
            package_map
                .get(id)
                .cloned()
                .context("Cargo package disappeared")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((roots, resolved))
}

fn is_notice(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "license",
        "licence",
        "copying",
        "copyright",
        "notice",
        "authors",
        "unlicense",
    ]
    .iter()
    .any(|prefix| {
        name == *prefix
            || name.starts_with(&format!("{prefix}."))
            || name.starts_with(&format!("{prefix}-"))
            || name.starts_with(&format!("{prefix}_"))
    })
}

pub fn read_crate_notices(
    archive: &Path,
    expected: &str,
    name: &str,
    version: &str,
) -> Result<(Option<String>, NoticeTexts)> {
    let meta = fs::symlink_metadata(archive)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_CRATE_BYTES,
        "Invalid Cargo archive: {}",
        archive.display()
    );
    let bytes = fs::read(archive)?;
    ensure!(
        digest(&bytes) == expected,
        "Cargo archive checksum mismatch: {name}@{version}"
    );
    let prefix = format!("{name}-{version}");
    let decoded = GzDecoder::new(Cursor::new(bytes.as_slice()));
    let mut archive = tar::Archive::new(decoded.take(MAX_CRATE_EXPANDED + 1));
    let mut candidates = BTreeMap::new();
    let mut package_toml = None;
    let mut all_paths = BTreeSet::new();
    for (index, entry) in archive.entries()?.enumerate() {
        ensure!(index < 100_000, "Cargo archive exceeds entry budget");
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        ensure!(
            !path.is_absolute()
                && !path.components().any(|c| matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )),
            "Unsafe Cargo archive path"
        );
        let relative = path
            .strip_prefix(&prefix)
            .context("Cargo archive prefix differs")?;
        let relative = relative
            .to_str()
            .context("Non-UTF8 Cargo archive path")?
            .to_owned();
        ensure!(
            all_paths.insert(relative.clone()),
            "Duplicate Cargo archive path"
        );
        let selected =
            relative == "Cargo.toml" || relative.rsplit('/').next().is_some_and(is_notice);
        if !selected {
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file(),
            "Cargo notice is not a regular file"
        );
        ensure!(
            entry.size() <= MAX_NOTICE_BYTES,
            "Cargo notice exceeds byte budget"
        );
        let mut value = Vec::new();
        entry.read_to_end(&mut value)?;
        if relative == "Cargo.toml" {
            package_toml = Some(value);
        } else {
            ensure!(!value.is_empty(), "Empty Cargo notice: {name}/{relative}");
            candidates.insert(relative, value);
        }
    }
    let mut rest = archive.into_inner();
    let mut tail = Vec::new();
    rest.read_to_end(&mut tail)?;
    ensure!(
        rest.limit() > 0,
        "Cargo archive exceeds expanded byte budget"
    );
    let manifest: toml::Value = toml::from_str(std::str::from_utf8(
        &package_toml.context("Cargo archive lacks Cargo.toml")?,
    )?)?;
    let package = manifest
        .get("package")
        .context("Cargo archive package missing")?;
    ensure!(
        package.get("name").and_then(toml::Value::as_str) == Some(name)
            && package.get("version").and_then(toml::Value::as_str) == Some(version),
        "Cargo archive package identity differs"
    );
    if let Some(requested) = package.get("license-file").and_then(toml::Value::as_str) {
        let requested_path = Path::new(requested);
        ensure!(
            !requested_path.is_absolute()
                && !requested_path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_))),
            "Unsafe declared Cargo license-file"
        );
        if !candidates.contains_key(requested) {
            // A declared license-file need not have a conventional basename or
            // appear after Cargo.toml in its archive. Re-read the pinned bytes.
            let decoded = GzDecoder::new(Cursor::new(bytes.as_slice()));
            let mut second = tar::Archive::new(decoded.take(MAX_CRATE_EXPANDED + 1));
            for entry in second.entries()? {
                let mut entry = entry?;
                if entry.path()?.as_ref() == Path::new(&prefix).join(requested_path) {
                    ensure!(
                        entry.header().entry_type().is_file() && entry.size() <= MAX_NOTICE_BYTES,
                        "Declared Cargo license-file is not a bounded regular file"
                    );
                    let mut text = Vec::new();
                    entry.read_to_end(&mut text)?;
                    ensure!(!text.is_empty(), "Declared Cargo license-file is empty");
                    candidates.insert(requested.to_owned(), text);
                    break;
                }
            }
        }
        ensure!(
            candidates.contains_key(requested),
            "Declared license-file is absent: {name}/{requested}"
        );
    }
    ensure!(
        !candidates.is_empty(),
        "No actual license/notice text in resolved crate {name}@{version}"
    );
    Ok((
        package
            .get("license")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
        candidates,
    ))
}

fn std_sources(root: &Path) -> Result<BTreeMap<String, FileIdentity>> {
    let mut result = BTreeMap::new();
    let copyright = safe_path(root, "COPYRIGHT-library.html")?;
    let copyright_id = identity(&copyright)?.context("Rust COPYRIGHT-library.html missing")?;
    let bytes = fs::read(copyright)?;
    ensure!(
        bytes.len() > 1024
            && String::from_utf8_lossy(&bytes)
                .to_ascii_lowercase()
                .contains("musl"),
        "Rust library copyright bundle lacks the musl notices"
    );
    result.insert("COPYRIGHT-library.html".to_owned(), copyright_id);
    let licenses = safe_path(root, "licenses")?;
    let entries = file_map(&licenses)?;
    ensure!(
        !entries.is_empty() && entries.len() <= 128,
        "Rust standard-library license bundle missing or oversized"
    );
    for (name, id) in entries {
        ensure!(
            fs::metadata(safe_path(&licenses, &name)?)?.len() > 0,
            "Empty Rust license: {name}"
        );
        result.insert(format!("licenses/{name}"), id);
    }
    Ok(result)
}

pub fn collect_notices(options: &NoticeOptions, runner: &ProcessRunner) -> Result<Value> {
    ensure!(options.target == TARGET, "Unsupported notice target");
    version(&options.rust_version)?;
    let root = project_root(&options.root)?;
    let cargo_home = project_root(&options.cargo_home)?;
    let rust_docs = project_root(&options.rust_docs)?;
    let output = new_output(&root, &options.output)?;
    ensure!(
        !cargo_home.starts_with(&output) && !rust_docs.starts_with(&output),
        "Notice output overlaps source inputs"
    );
    let rust_version = text_output(Command::new("rustc").arg("--version"), runner)?;
    ensure!(
        rust_version.starts_with(&format!("rustc {} ", options.rust_version)),
        "Notice collector requires the pinned Rust toolchain"
    );
    let sysroot = PathBuf::from(text_output(
        Command::new("rustc").args(["--print", "sysroot"]),
        runner,
    )?);
    ensure!(
        fs::canonicalize(&rust_docs)? == fs::canonicalize(sysroot.join("share/doc/rust"))?,
        "Rust notices must come from the active exact toolchain sysroot"
    );
    let source_lock = identity(&safe_path(&root, "Cargo.lock")?)?.context("Missing Cargo.lock")?;
    let checksums = lock_checksums(&root)?;
    let standard_sources = std_sources(&rust_docs)?;
    let raw = runner.checked_with_timeout(
        Command::new("cargo")
            .args([
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--offline",
                "--filter-platform",
                &options.target,
            ])
            .env("CARGO_HOME", &cargo_home)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .current_dir(&root),
        OutputMode::Capture(16 * 1024 * 1024),
        Duration::from_secs(90),
    )?;
    let metadata: Value = serde_json::from_slice(&raw.stdout)?;
    let (raw_roots, resolved) = resolved_packages(&metadata)?;
    let mut local_ids = BTreeMap::new();
    for package in &resolved {
        if package["source"].is_null() {
            let manifest = PathBuf::from(text(&package["manifest_path"], "Cargo manifest path")?);
            let relative = manifest.strip_prefix(&root).context(
                "Local third-party source outside this repository has no notice authority",
            )?;
            local_ids.insert(
                text(&package["id"], "Cargo package id")?.to_owned(),
                format!(
                    "workspace+{}#{}@{}",
                    relative.to_string_lossy(),
                    text(&package["name"], "Cargo package name")?,
                    text(&package["version"], "Cargo package version")?
                ),
            );
        }
    }
    let roots = raw_roots
        .iter()
        .map(|id| {
            local_ids
                .get(id)
                .cloned()
                .context("Cargo workspace root has no repository source identity")
        })
        .collect::<Result<Vec<_>>>()?;
    let stage = tempfile::Builder::new()
        .prefix(".devkit-notices-")
        .tempdir_in(output.parent().context("Notice output parent")?)?;
    let payload = stage.path().join("payload");
    fs::create_dir(&payload)?;
    let mut package_notices = Vec::new();
    let mut resolved_ids = Vec::new();
    for package in resolved {
        runner.check_cancelled()?;
        let id = text(&package["id"], "Cargo package id")?.to_owned();
        resolved_ids.push(local_ids.get(&id).cloned().unwrap_or_else(|| id.clone()));
        let manifest = PathBuf::from(text(&package["manifest_path"], "Cargo manifest path")?);
        if package["source"].is_null() {
            ensure!(
                manifest.starts_with(&root),
                "Local third-party source outside this repository has no notice authority"
            );
            continue;
        }
        let name = text(&package["name"], "Cargo package name")?;
        let package_version = text(&package["version"], "Cargo package version")?;
        let source = text(&package["source"], "Cargo package source")?;
        ensure!(
            source.starts_with("registry+"),
            "Unsupported non-registry third-party notice source: {id}"
        );
        let expected = checksums
            .get(&(
                name.to_owned(),
                package_version.to_owned(),
                source.to_owned(),
            ))
            .context("Resolved Cargo package checksum is absent from Cargo.lock")?;
        let relative = manifest
            .strip_prefix(cargo_home.join("registry/src"))
            .context("Cargo manifest is outside the requested source cache")?;
        let components = relative.components().collect::<Vec<_>>();
        ensure!(
            components.len() == 3,
            "Unexpected Cargo registry source layout"
        );
        let archive = cargo_home
            .join("registry/cache")
            .join(components[0].as_os_str())
            .join(format!("{name}-{package_version}.crate"));
        project_root(&archive)?;
        let (license, notices) = read_crate_notices(&archive, expected, name, package_version)?;
        ensure!(
            license.as_deref() == package["license"].as_str(),
            "Cargo source metadata license differs from its verified archive"
        );
        let folder = format!(
            "cargo/{name}-{}-{}",
            package_version.replace('+', "-"),
            &digest(id.as_bytes())[..8]
        );
        relative_path(&folder)?;
        let mut notice_map = BTreeMap::new();
        for (index, (original, bytes)) in notices.into_iter().enumerate() {
            let output_name = format!("{folder}/notice-{:03}.txt", index + 1);
            write_atomic(&safe_path(&payload, &output_name)?, &bytes, 0o644)?;
            notice_map.insert(
                output_name,
                NoticeSource {
                    path: original,
                    sha256: digest(&bytes),
                },
            );
        }
        package_notices.push(PackageNotices {
            id,
            name: name.to_owned(),
            version: package_version.to_owned(),
            source: source.to_owned(),
            license,
            archive_sha256: expected.clone(),
            notices: notice_map,
        });
    }
    let mut standard_library = BTreeMap::new();
    for (source, expected) in &standard_sources {
        let bytes = fs::read(safe_path(&rust_docs, source)?)?;
        ensure!(
            digest(&bytes) == expected.sha256,
            "Rust standard-library notices changed"
        );
        let target = format!("rust/{source}");
        write_atomic(&safe_path(&payload, &target)?, &bytes, 0o644)?;
        standard_library.insert(
            target,
            NoticeSource {
                path: format!("share/doc/rust/{source}"),
                sha256: expected.sha256.clone(),
            },
        );
    }
    package_notices.sort_by(|a, b| a.id.cmp(&b.id));
    resolved_ids.sort();
    let mut description = format!(
        "Third-party notices\n\nRust standard library {}: rust/COPYRIGHT-library.html and rust/licenses/.\n\nCargo packages resolved for {}:\n",
        options.rust_version, options.target
    );
    for package in &package_notices {
        description.push_str(&format!(
            "\n{} {} ({})\n",
            package.name,
            package.version,
            package.license.as_deref().unwrap_or("license-file")
        ));
        for (path, original) in &package.notices {
            description.push_str(&format!("  {path} — {}\n", original.path));
        }
    }
    write_atomic(&payload.join("NOTICE.txt"), description.as_bytes(), 0o644)?;
    let receipt = NoticeSet {
        schema_version: 1,
        target: options.target.clone(),
        rust_version: options.rust_version.clone(),
        source_cargo_lock_sha256: source_lock.sha256.clone(),
        generator: build_info(),
        cargo_metadata_sha256: digest(&raw.stdout),
        resolved_ids,
        roots,
        packages: package_notices,
        standard_library,
        files: file_map(&payload)?,
    };
    write_json(&payload.join("notice-set.json"), &receipt)?;
    verify_notice_set(&payload, &root, &options.target, &options.rust_version)?;
    ensure!(
        identity(&safe_path(&root, "Cargo.lock")?)?.as_ref() == Some(&source_lock),
        "Cargo.lock changed during notice collection"
    );
    ensure!(
        std_sources(&rust_docs)? == standard_sources,
        "Rust standard-library notices changed during collection"
    );
    runner.check_cancelled()?;
    publish_new_directory(&payload, &output)?;
    Ok(serde_json::to_value(receipt)?)
}

pub fn verify_notice_set(
    directory: &Path,
    root: &Path,
    target: &str,
    rust_version: &str,
) -> Result<()> {
    let directory = project_root(directory)?;
    let mut actual = file_map(&directory)?;
    let receipt: NoticeSet = read_json(&safe_path(&directory, "notice-set.json")?)?;
    ensure!(
        receipt.schema_version == 1
            && receipt.target == target
            && target == TARGET
            && receipt.rust_version == rust_version,
        "Notice bundle target/toolchain differs"
    );
    let lock = identity(&safe_path(root, "Cargo.lock")?)?.context("Missing source Cargo.lock")?;
    ensure!(
        lock.sha256 == receipt.source_cargo_lock_sha256,
        "Notice bundle belongs to another Cargo.lock"
    );
    actual.remove("notice-set.json");
    ensure!(
        actual == receipt.files,
        "Notice bundle file set/hash/mode differs"
    );
    ensure!(
        !receipt.roots.is_empty()
            && !receipt.resolved_ids.is_empty()
            && receipt
                .roots
                .iter()
                .all(|r| receipt.resolved_ids.contains(r))
            && receipt.resolved_ids.iter().collect::<BTreeSet<_>>().len()
                == receipt.resolved_ids.len(),
        "Notice bundle lacks its Cargo roots or duplicates resolved IDs"
    );
    ensure!(
        SHA256_RE.is_match(&receipt.cargo_metadata_sha256),
        "Invalid Cargo metadata identity"
    );
    let checksums = lock_checksums(root)?;
    let mut ids = BTreeSet::new();
    let mut named = BTreeSet::from(["NOTICE.txt".to_owned()]);
    for package in &receipt.packages {
        ensure!(
            ids.insert(&package.id) && receipt.resolved_ids.contains(&package.id),
            "Duplicate/unresolved notice package"
        );
        ensure!(
            checksums.get(&(
                package.name.clone(),
                package.version.clone(),
                package.source.clone()
            )) == Some(&package.archive_sha256),
            "Notice package differs from Cargo.lock"
        );
        ensure!(
            !package.notices.is_empty(),
            "Resolved package has no actual notices"
        );
        for (path, source) in &package.notices {
            ensure!(
                path.starts_with("cargo/")
                    && named.insert(path.clone())
                    && receipt
                        .files
                        .get(path)
                        .is_some_and(|file| file.mode == 0o644 && file.sha256 == source.sha256),
                "Cargo notice source/output identity differs"
            );
        }
    }
    let expected_packages = receipt
        .resolved_ids
        .iter()
        .filter(|id| id.starts_with("registry+"))
        .collect::<BTreeSet<_>>();
    ensure!(
        ids == expected_packages,
        "Notice bundle omits a resolved registry package"
    );
    ensure!(
        receipt
            .standard_library
            .contains_key("rust/COPYRIGHT-library.html")
            && receipt
                .standard_library
                .keys()
                .any(|p| p.starts_with("rust/licenses/")),
        "Rust standard-library notices are incomplete"
    );
    for (path, source) in &receipt.standard_library {
        ensure!(
            path.starts_with("rust/")
                && named.insert(path.clone())
                && receipt
                    .files
                    .get(path)
                    .is_some_and(|file| file.mode == 0o644 && file.sha256 == source.sha256),
            "Rust notice source/output identity differs"
        );
    }
    ensure!(
        named == receipt.files.keys().cloned().collect(),
        "Notice bundle has unexplained files"
    );
    Ok(())
}
