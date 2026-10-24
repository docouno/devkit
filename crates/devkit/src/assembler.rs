//! Author-only assembly of one immutable set of native release artifacts.
use crate::{
    Result,
    archive::{create_archive, extract_archive, publish_new_directory},
    delivery::{
        EnginePin, engine_build_info, file_digest, render_bootstrap, render_project_loader,
        verify_engine,
    },
    distribution::{AssetIdentity, ENGINE_ASSET, NpmArtifact, ReleaseIndex, TARGET},
    files::*,
    model::{Environment, FileIdentity, Release},
    notices::verify_notice_set,
    processes::{OutputMode, OwnedContainer, ProcessRunner},
    release::{admit_release, import_npm_payload},
};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const NODE_TEMPLATES: [&str; 4] = ["node-api", "react-web", "electron-react", "react-client"];
const TEMPLATES: [&str; 5] = [
    "node-api",
    "react-web",
    "rust-api",
    "electron-react",
    "react-client",
];
const RUNTIME_FILES: [&str; 8] = [
    "devkit.mk",
    "compose.tools.yaml",
    "rust.Dockerfile",
    "desktop.Dockerfile",
    "desktop-seccomp.json",
    "licenses/Apache-2.0.txt",
    "licenses/moby-profiles.NOTICE",
    "devkit.sh",
];

pub struct AssemblyOptions {
    pub root: PathBuf,
    pub engine: PathBuf,
    pub npm_inputs: PathBuf,
    pub output: PathBuf,
    pub notices: PathBuf,
    pub notice_set_sha256: String,
    pub source_commit: String,
    pub source_tree: String,
    pub build_run: String,
    pub local: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateInputs {
    pub manifest: String,
    pub lock: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NpmInputs {
    pub schema_version: u32,
    pub version: String,
    pub source_version: String,
    pub source_environment_version: String,
    pub environment: Environment,
    pub templates: BTreeMap<String, TemplateInputs>,
    pub sources: BTreeMap<String, FileIdentity>,
    pub files: BTreeMap<String, FileIdentity>,
}

fn ignored(name: &str) -> bool {
    name.split('/').any(|part| {
        [
            "node_modules",
            "target",
            "dist",
            ".artifacts",
            ".tmp",
            ".git",
        ]
        .contains(&part)
    })
}

fn selected_sources(
    root: &Path,
    directory: &str,
    files: &mut BTreeMap<String, FileIdentity>,
) -> Result<()> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, FileIdentity>) -> Result<()> {
        let name = relative_name(root, path)?;
        if ignored(&name) {
            return Ok(());
        }
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "Source contains a symlink: {name}"
        );
        if meta.is_dir() {
            for entry in sorted_entries(path)? {
                visit(root, &entry.path(), files)?;
            }
        } else {
            ensure!(
                meta.is_file(),
                "Source contains an unsupported object: {name}"
            );
            files.insert(name, identity(path)?.context("Source disappeared")?);
        }
        Ok(())
    }
    visit(root, &safe_path(root, directory)?, files)
}

fn npm_sources(root: &Path) -> Result<BTreeMap<String, FileIdentity>> {
    let mut files = BTreeMap::new();
    for name in ["VERSION", "LICENSE"] {
        files.insert(
            name.to_owned(),
            identity(&safe_path(root, name)?)?.context("Missing npm source")?,
        );
    }
    selected_sources(root, "packages/env", &mut files)?;
    ensure!(
        !files.keys().any(|p| p.rsplit('/').next() == Some(".npmrc")),
        "Package .npmrc is not an accepted npm preparation input"
    );
    for name in NODE_TEMPLATES {
        for file in ["package.json", "package-lock.json"] {
            let path = format!("templates/{name}/{file}");
            files.insert(
                path.clone(),
                identity(&safe_path(root, &path)?)?.context("Missing template npm source")?,
            );
        }
    }
    Ok(files)
}

fn sri(bytes: &[u8]) -> String {
    format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes)))
}

fn npm_name(location: &str) -> Result<String> {
    let name = location
        .rsplit("node_modules/")
        .next()
        .context("Missing native lock location")?;
    ensure!(
        location.starts_with("node_modules/") && !name.is_empty() && !name.contains("node_modules"),
        "Invalid native lock package location"
    );
    ensure!(
        name.split('/').count() == if name.starts_with('@') { 2 } else { 1 },
        "Invalid native lock package name"
    );
    ensure!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@/._-".contains(&b)),
        "Invalid native lock package name"
    );
    Ok(name.to_owned())
}

fn lock_integrity(value: &str) -> bool {
    let Some((algorithm, encoded)) = value.split_once('-') else {
        return false;
    };
    let length = match algorithm {
        "sha256" => 32,
        "sha384" => 48,
        "sha512" => 64,
        _ => return false,
    };
    STANDARD
        .decode(encoded)
        .is_ok_and(|raw| raw.len() == length && STANDARD.encode(raw) == encoded)
}

fn verify_template_lock(
    root: &Path,
    directory: &Path,
    name: &str,
    version: &str,
    environment_sri: &str,
) -> Result<()> {
    let original_manifest: Value =
        read_json(&safe_path(root, &format!("templates/{name}/package.json"))?)?;
    let mut expected_manifest = original_manifest;
    object(
        &expected_manifest["devDependencies"],
        "Source template devDependencies",
    )?;
    expected_manifest["devDependencies"]["@docouno/env"] = json!(version);
    let manifest: Value = read_json(&safe_path(
        directory,
        &format!("templates/{name}/package.json"),
    )?)?;
    ensure!(
        manifest == expected_manifest,
        "Prepared template manifest changed fields other than the environment version: {name}"
    );
    let original: Value = read_json(&safe_path(
        root,
        &format!("templates/{name}/package-lock.json"),
    )?)?;
    let lock: Value = read_json(&safe_path(
        directory,
        &format!("templates/{name}/package-lock.json"),
    )?)?;
    ensure!(
        lock["lockfileVersion"] == 3,
        "Default template must use lockfileVersion3"
    );
    let packages = object(&lock["packages"], "Template lock packages")?;
    let old_packages = object(&original["packages"], "Source template lock packages")?;
    let lock_root = packages.get("").context("Template lock root is missing")?;
    for field in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        ensure!(
            lock_root[field] == manifest[field],
            "Prepared template root dependency field differs: {name}/{field}"
        );
    }
    for (location, value) in packages {
        if location.is_empty() {
            continue;
        }
        let package = object(value, "Template package identity")?;
        let package_name = npm_name(location)?;
        let selected = text(&value["version"], "Template package version")?;
        ensure!(
            !selected.is_empty()
                && selected.len() <= 128
                && selected
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)),
            "Invalid selected template version"
        );
        let base = package_name
            .rsplit('/')
            .next()
            .context("Package basename")?;
        let expected_url =
            format!("https://registry.npmjs.org/{package_name}/-/{base}-{selected}.tgz");
        ensure!(
            value["resolved"].as_str() == Some(expected_url.as_str())
                && value["integrity"].as_str().is_some_and(lock_integrity),
            "Incomplete/non-registry default template identity: {location}"
        );
        ensure!(
            !package.get("link").is_some_and(|v| v != &json!(false))
                && !package.get("inBundle").is_some_and(|v| v != &json!(false)),
            "Linked/bundled default template identity: {location}"
        );
        if package_name == "@docouno/env" {
            ensure!(
                selected == version && value["integrity"].as_str() == Some(environment_sri),
                "Prepared environment identity differs"
            );
        } else if let Some(old) = old_packages.get(location) {
            for field in [
                "version",
                "resolved",
                "integrity",
                "dev",
                "optional",
                "devOptional",
                "peer",
                "inBundle",
                "link",
            ] {
                if old.get(field).is_some() {
                    ensure!(
                        old[field] == value[field],
                        "Selected external template identity changed: {location}/{field}"
                    );
                }
            }
        } else {
            anyhow::bail!("Prepared template added an external identity: {location}");
        }
    }
    for location in old_packages
        .keys()
        .filter(|p| !p.is_empty() && p.as_str() != "node_modules/@docouno/env")
    {
        ensure!(
            packages.contains_key(location),
            "Prepared template removed an external identity: {location}"
        );
    }
    ensure!(
        packages.contains_key("node_modules/@docouno/env"),
        "Template lock lacks environment package"
    );
    Ok(())
}

pub fn verify_npm_inputs(root: &Path, directory: &Path) -> Result<NpmInputs> {
    let directory = project_root(directory)?;
    let mut actual = file_map(&directory)?;
    let inputs: NpmInputs = read_json(&safe_path(&directory, "inputs.json")?)?;
    ensure!(
        inputs.schema_version == 1,
        "Unsupported npm inputs schemaVersion"
    );
    version(&inputs.version)?;
    ensure!(
        inputs.source_version == fs::read_to_string(safe_path(root, "VERSION")?)?.trim(),
        "Npm inputs source version differs"
    );
    let environment: Value = read_json(&safe_path(root, "packages/env/package.json")?)?;
    ensure!(
        environment["name"] == "@docouno/env"
            && environment["version"] == inputs.source_environment_version,
        "Npm source environment identity differs"
    );
    ensure!(
        npm_sources(root)? == inputs.sources,
        "Npm source file set/hash/mode differs from the frozen inputs"
    );
    actual.remove("inputs.json");
    ensure!(
        actual == inputs.files,
        "Npm prepared file set/hash/mode differs"
    );
    let expected_archive = format!("npm/docouno-env-{}.tgz", inputs.version);
    ensure!(
        inputs.environment.name == "@docouno/env"
            && inputs.environment.version == inputs.version
            && inputs.environment.archive == expected_archive,
        "Prepared environment package identity differs"
    );
    let archive = safe_path(&directory, &expected_archive)?;
    ensure!(
        fs::metadata(&archive)?.len() <= 64 * 1024 * 1024,
        "Environment archive exceeds byte budget"
    );
    ensure!(
        sri(&fs::read(&archive)?) == inputs.environment.integrity,
        "Prepared environment SRI differs"
    );
    let mut expected = BTreeSet::from([expected_archive]);
    ensure!(
        inputs.templates.len() == NODE_TEMPLATES.len(),
        "Unexpected prepared template set"
    );
    for name in NODE_TEMPLATES {
        let template = inputs
            .templates
            .get(name)
            .context("Missing npm template snapshot")?;
        ensure!(
            template.manifest == format!("templates/{name}/package.json")
                && template.lock == format!("templates/{name}/package-lock.json"),
            "Unexpected template snapshot paths"
        );
        expected.insert(template.manifest.clone());
        expected.insert(template.lock.clone());
        verify_template_lock(
            root,
            &directory,
            name,
            &inputs.version,
            &inputs.environment.integrity,
        )?;
    }
    ensure!(
        expected == inputs.files.keys().cloned().collect(),
        "Unexpected npm preparation files"
    );
    Ok(inputs)
}

#[derive(Deserialize)]
struct PackResult {
    name: String,
    version: String,
    filename: String,
    size: u64,
    integrity: String,
    shasum: String,
}

pub fn verify_npm_pack(
    raw: &[u8],
    directory: &Path,
    name: &str,
    version: &str,
) -> Result<NpmArtifact> {
    let results: Vec<PackResult> = serde_json::from_slice(raw)?;
    ensure!(
        results.len() == 1,
        "npm pack returned an unexpected result count"
    );
    let entry = &results[0];
    let expected_name = format!(
        "{}-{version}.tgz",
        name.trim_start_matches('@').replace('/', "-")
    );
    ensure!(
        entry.name == name && entry.version == version && entry.filename == expected_name,
        "npm pack returned a conflicting package identity/filename"
    );
    let archive = safe_path(directory, &entry.filename)?;
    let metadata = fs::symlink_metadata(&archive)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 128 * 1024 * 1024,
        "npm pack archive is not a bounded regular file"
    );
    let bytes = fs::read(&archive)?;
    ensure!(
        entry.size == bytes.len() as u64
            && entry.integrity == sri(&bytes)
            && entry.shasum == format!("{:x}", Sha1::digest(&bytes)),
        "npm pack metadata differs from the actual bytes"
    );
    ensure!(
        file_map(directory)?
            .keys()
            .map(String::as_str)
            .eq([entry.filename.as_str()]),
        "npm pack produced unexpected files"
    );
    Ok(NpmArtifact {
        name: entry.name.clone(),
        version: entry.version.clone(),
        archive: entry.filename.clone(),
        integrity: entry.integrity.clone(),
        shasum: entry.shasum.clone(),
    })
}

fn pins(root: &Path) -> Result<BTreeMap<String, String>> {
    let raw = fs::read_to_string(safe_path(root, "toolchains.env")?)?;
    let mut pins = BTreeMap::new();
    for line in raw
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (key, value) = line
            .split_once('=')
            .context("Invalid source toolchain pin")?;
        ensure!(
            [
                "DEVKIT_NODE_IMAGE",
                "DEVKIT_NODE_VERSION",
                "DEVKIT_RUST_VERSION"
            ]
            .contains(&key)
                && !value.is_empty()
                && !value.chars().any(char::is_whitespace)
                && pins.insert(key.to_owned(), value.to_owned()).is_none(),
            "Unexpected/duplicate source toolchain pin"
        );
    }
    ensure!(pins.len() == 3, "Incomplete source toolchains.env");
    let (_, hash) = pins["DEVKIT_NODE_IMAGE"]
        .rsplit_once("@sha256:")
        .context("Node image is not digest-pinned")?;
    ensure!(SHA256_RE.is_match(hash), "Invalid Node image digest");
    version(&pins["DEVKIT_NODE_VERSION"])?;
    version(&pins["DEVKIT_RUST_VERSION"])?;
    Ok(pins)
}

pub fn verify_rust_template(root: &Path, rust_version: &str) -> Result<()> {
    version(rust_version)?;
    let path = safe_path(root, "templates/rust-api/rust-toolchain.toml")?;
    let toolchain: toml::Value = toml::from_str(&fs::read_to_string(path)?)?;
    let channel = toolchain
        .get("toolchain")
        .and_then(|value| value.get("channel"))
        .and_then(toml::Value::as_str);
    ensure!(
        channel == Some(rust_version),
        "Rust template channel differs from the release toolchain"
    );
    Ok(())
}

pub fn verify_runtime_notices(root: &Path) -> Result<()> {
    for name in ["Apache-2.0.txt", "moby-profiles.NOTICE"] {
        let path = safe_path(root, &format!("runtime/licenses/{name}"))?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Missing runtime notice: {name}"))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Runtime notice must be a regular file: {name}"
        );
        ensure!(metadata.len() > 0, "Runtime notice is empty: {name}");
    }
    Ok(())
}

fn git(root: &Path, args: &[&str], runner: &ProcessRunner) -> Result<String> {
    let mut command = source_git_command(root);
    let result = runner.checked_with_timeout(
        command.args(args),
        OutputMode::Capture(1024 * 1024),
        Duration::from_secs(15),
    )?;
    Ok(result.text()?.trim().to_owned())
}

fn source_git_command(root: &Path) -> Command {
    isolated_git_command(root, std::env::vars_os().map(|(name, _)| name))
}

fn isolated_git_command(
    root: &Path,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> Command {
    let mut command = Command::new("git");
    // Ambient locator, object, index and config controls must not redirect any
    // source query to a different checkout or object database.
    for name in names {
        if name.as_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(["-c", "core.fsmonitor=false"])
        .current_dir(root);
    command
}

fn verify_git(options: &AssemblyOptions, root: &Path, runner: &ProcessRunner) -> Result<()> {
    if options.local {
        return Ok(());
    }
    ensure!(
        !options.build_run.is_empty(),
        "Published assembly requires a build-run identity"
    );
    verify_source_checkout(root, &options.source_commit, &options.source_tree, runner)
}

pub fn verify_source_checkout(
    root: &Path,
    source_commit: &str,
    source_tree: &str,
    runner: &ProcessRunner,
) -> Result<()> {
    let root = fs::canonicalize(root).context("Canonical assembly source root")?;
    ensure!(
        source_commit.len() == 40 && source_tree.len() == 40,
        "Published assembly requires exact source identities"
    );
    ensure!(
        fs::canonicalize(git(&root, &["rev-parse", "--show-toplevel"], runner)?)? == root,
        "Git checkout root differs from the assembly source root"
    );
    ensure!(
        git(&root, &["rev-parse", "HEAD"], runner)? == source_commit
            && git(&root, &["rev-parse", "HEAD^{tree}"], runner)? == source_tree,
        "Assembly source commit/tree differs from checkout"
    );
    ensure!(
        git(
            &root,
            &["status", "--porcelain", "--untracked-files=all"],
            runner
        )?
        .is_empty(),
        "Published assembly requires a clean tracked source tree"
    );
    // Index flags such as assume-unchanged can hide a working-tree edit from
    // git status. Compare each regular source byte to the actual HEAD blob too.
    let entries = git(&root, &["ls-tree", "-rz", "--full-tree", "HEAD"], runner)?;
    let mut count = 0;
    let mut total = 0_u64;
    let mut tracked = BTreeSet::new();
    for record in entries.split('\0').filter(|entry| !entry.is_empty()) {
        count += 1;
        ensure!(count <= 16_384, "Source tree exceeds its file budget");
        let (header, name) = record.split_once('\t').context("Invalid Git tree record")?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        ensure!(
            fields.len() == 3 && fields[1] == "blob" && ["100644", "100755"].contains(&fields[0]),
            "Published source must contain regular Git blobs"
        );
        ensure!(tracked.insert(name.to_owned()), "Duplicate Git source path");
        let path = safe_path(&root, name)?;
        let metadata = fs::symlink_metadata(&path)?;
        total = total
            .checked_add(metadata.len())
            .context("Source byte budget overflow")?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= 32 * 1024 * 1024
                && total <= 128 * 1024 * 1024,
            "Invalid or oversized tracked source: {name}"
        );
        let bytes = fs::read(&path)?;
        let mut blob = Sha1::new();
        blob.update(format!("blob {}\0", bytes.len()).as_bytes());
        blob.update(&bytes);
        ensure!(
            format!("{:x}", blob.finalize()) == fields[2]
                && (metadata.permissions().mode() & 0o111 != 0) == (fields[0] == "100755"),
            "Tracked source bytes/executable class differ from HEAD: {name}"
        );
        runner.check_cancelled()?;
    }
    ensure!(count > 0, "Published source tree is empty");
    for name in source_snapshot(&root)?.keys() {
        ensure!(
            tracked.contains(name),
            "Assembly source input is absent from HEAD: {name}"
        );
    }
    Ok(())
}

fn source_snapshot(root: &Path) -> Result<BTreeMap<String, FileIdentity>> {
    let mut result = BTreeMap::new();
    for name in [
        "VERSION",
        "LICENSE",
        "README.md",
        "toolchains.env",
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
    ] {
        result.insert(
            name.to_owned(),
            identity(&safe_path(root, name)?)?.context("Missing assembly source")?,
        );
    }
    for name in ["runtime", "templates", "packages/env"] {
        selected_sources(root, name, &mut result)?;
    }
    Ok(result)
}

fn mount(source: &Path, target: &str, readonly: bool) -> Result<String> {
    let source = source
        .to_str()
        .context("Non-UTF8 Docker mount source")?
        .replace('"', "\"\"");
    Ok(format!(
        "type=bind,\"source={source}\",target={target}{}",
        if readonly { ",readonly" } else { "" }
    ))
}

fn npm_pack(
    payload: &Path,
    directory: &Path,
    image: &str,
    runner: &ProcessRunner,
) -> Result<Vec<u8>> {
    fs::create_dir(directory)?;
    let token = uuid::Uuid::new_v4().to_string();
    let owned = OwnedContainer::new(format!("devkit-pack-{token}"), token);
    let meta = fs::metadata(payload)?;
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--pull",
        "never",
        "--rm",
        "--init",
        "--memory",
        "1g",
        "--cpus",
        "2",
        "--read-only",
        "--network",
        "none",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
        "--name",
        &owned.name,
        "--label",
        &format!("{}={}", owned.label, owned.token),
        "--user",
        &format!("{}:{}", meta.uid(), meta.gid()),
        "--mount",
        &mount(payload, "/payload", true)?,
        "--mount",
        &mount(directory, "/out", false)?,
        "--workdir",
        "/payload",
        "--tmpfs",
        "/tmp:mode=1777,size=256m",
    ]);
    for value in [
        "HOME=/tmp/devkit-home",
        "npm_config_cache=/tmp/npm-cache",
        "npm_config_userconfig=/tmp/user.npmrc",
        "npm_config_globalconfig=/tmp/global.npmrc",
        "npm_config_update_notifier=false",
        "npm_config_fund=false",
        "npm_config_audit=false",
        "npm_config_ignore_scripts=true",
    ] {
        command.args(["--env", value]);
    }
    command.arg(image).args([
        "npm",
        "pack",
        "--ignore-scripts",
        "--json",
        "--pack-destination",
        "/out",
    ]);
    let result = runner.run_owned_with_timeout(
        &mut command,
        OutputMode::Capture(8 * 1024 * 1024),
        &owned,
        Duration::from_secs(180),
    )?;
    let result = result.success("native npm pack")?;
    Ok(result.stdout)
}

fn asset(path: &Path) -> Result<AssetIdentity> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Release asset must be regular"
    );
    Ok(AssetIdentity {
        sha256: file_digest(path)?,
        size: meta.len(),
    })
}

pub fn assemble(options: &AssemblyOptions, runner: &ProcessRunner) -> Result<ReleaseIndex> {
    runner.check_cancelled()?;
    let root = project_root(&options.root)?;
    let engine = project_root(&options.engine)?;
    let npm_inputs = project_root(&options.npm_inputs)?;
    let notices = project_root(&options.notices)?;
    let output = project_root(&options.output)?;
    ensure!(
        !exists(&output)? && output.parent().is_some_and(Path::is_dir),
        "Assembly output must be a new directory with an existing parent"
    );
    ensure!(
        !root.starts_with(&output)
            && !engine.starts_with(&output)
            && !npm_inputs.starts_with(&output)
            && !notices.starts_with(&output),
        "Assembly output overlaps its inputs"
    );
    verify_git(options, &root, runner)?;
    let source_before = source_snapshot(&root)?;
    verify_runtime_notices(&root)?;
    let mut settings = pins(&root)?;
    verify_notice_set(
        &notices,
        &root,
        TARGET,
        &settings["DEVKIT_RUST_VERSION"],
        &options.notice_set_sha256,
    )?;
    let npm_before = file_map(&npm_inputs)?;
    let notices_before = file_map(&notices)?;
    ensure!(
        notices_before
            .get("notice-set.json")
            .is_some_and(|file| file.sha256 == options.notice_set_sha256),
        "Notice bundle changed after producer verification"
    );
    let inputs = verify_npm_inputs(&root, &npm_inputs)?;
    let version = inputs.version.clone();
    verify_rust_template(&root, &settings["DEVKIT_RUST_VERSION"])?;
    let source_version = fs::read_to_string(safe_path(&root, "VERSION")?)?;
    let workspace: toml::Value =
        toml::from_str(&fs::read_to_string(safe_path(&root, "Cargo.toml")?)?)?;
    if !options.local {
        let root_package: Value = read_json(&safe_path(&root, "package.json")?)?;
        let cargo_version = workspace
            .get("workspace")
            .and_then(|v| v.get("package"))
            .and_then(|v| v.get("version"))
            .and_then(toml::Value::as_str);
        ensure!(
            source_version.trim() == version
                && inputs.source_environment_version == version
                && root_package["version"] == version
                && cargo_version == Some(version.as_str()),
            "Release source versions are not coordinated"
        );
    }
    let engine_sha = file_digest(&engine)?;
    let pin = EnginePin {
        version: version.clone(),
        target: TARGET.to_owned(),
        sha256: engine_sha.clone(),
    };
    verify_engine(&engine, &pin)?;
    let build = engine_build_info(&engine, runner)?;
    ensure!(
        build.schema_version == 1 && build.version == version && build.target == TARGET,
        "ELF build version/target differs from release"
    );
    if !options.local {
        ensure!(
            build.source_commit.as_deref() == Some(&options.source_commit)
                && build.source_tree.as_deref() == Some(&options.source_tree),
            "ELF source attribution differs from assembly source"
        );
    }
    if !options.local {
        let receipt: crate::notices::NoticeSet =
            read_json(&safe_path(&notices, "notice-set.json")?)?;
        ensure!(
            receipt.generator == build,
            "Notice collector source/build identity differs from the assembled engine"
        );
    }
    let stage = tempfile::Builder::new()
        .prefix(".devkit-build-set-")
        .tempdir_in(output.parent().context("Build output parent")?)?;
    let environment_package = stage.path().join("environment-package");
    extract_archive(
        &safe_path(&npm_inputs, &inputs.environment.archive)?,
        &environment_package,
        "package",
    )?;
    let environment_manifest: Value = read_json(&safe_path(&environment_package, "package.json")?)?;
    ensure!(
        environment_manifest["name"] == "@docouno/env"
            && environment_manifest["version"] == version,
        "Environment archive package identity differs"
    );
    let build_set = stage.path().join("set");
    fs::create_dir(&build_set)?;
    let payload = build_set.join("payload");
    fs::create_dir(&payload)?;
    write_atomic(&payload.join("devkit"), &fs::read(&engine)?, 0o755)?;
    for file in RUNTIME_FILES {
        if file == "devkit.sh" {
            continue;
        }
        write_atomic(
            &safe_path(&payload, &format!("runtime/{file}"))?,
            &fs::read(safe_path(&root, &format!("runtime/{file}"))?)?,
            0o644,
        )?;
    }
    verify_runtime_notices(&payload)?;
    write_atomic(
        &safe_path(&payload, "runtime/devkit.sh")?,
        render_project_loader(&version, &engine_sha, TARGET)?.as_bytes(),
        0o755,
    )?;
    write_atomic(
        &safe_path(&payload, "runtime/LICENSE")?,
        &fs::read(safe_path(&root, "LICENSE")?)?,
        0o644,
    )?;
    settings.insert("DEVKIT_VERSION".to_owned(), version.clone());
    settings.insert(
        "DEVKIT_RUST_RECIPE_HASH".to_owned(),
        file_digest(&safe_path(&payload, "runtime/rust.Dockerfile")?)?,
    );
    settings.insert(
        "DEVKIT_DESKTOP_RECIPE_HASH".to_owned(),
        file_digest(&safe_path(&payload, "runtime/desktop.Dockerfile")?)?,
    );
    settings.insert("DEVKIT_ENGINE_SHA256".to_owned(), engine_sha.clone());
    settings.insert("DEVKIT_ENGINE_TARGET".to_owned(), TARGET.to_owned());
    let encoded_pins = settings
        .iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect::<String>();
    write_atomic(
        &safe_path(&payload, "runtime/toolchains.env")?,
        encoded_pins.as_bytes(),
        0o644,
    )?;
    for template in TEMPLATES {
        copy_tree(
            &safe_path(&root, &format!("templates/{template}"))?,
            &safe_path(&payload, &format!("templates/{template}"))?,
            &ignored,
            false,
        )?;
        let license = safe_path(&payload, &format!("templates/{template}/LICENSE"))?;
        if !exists(&license)? {
            write_atomic(&license, &fs::read(safe_path(&root, "LICENSE")?)?, 0o644)?;
        }
    }
    for name in inputs.files.keys() {
        write_atomic(
            &safe_path(&payload, name)?,
            &fs::read(safe_path(&npm_inputs, name)?)?,
            0o644,
        )?;
    }
    copy_tree(&notices, &payload.join("notices"), &|_| false, false)?;
    for name in ["README.md", "LICENSE"] {
        write_atomic(
            &payload.join(name),
            &fs::read(safe_path(&root, name)?)?,
            0o644,
        )?;
    }
    write_json(
        &payload.join("package.json"),
        &json!({"name":"@docouno/devkit","version":version,"description":"Pinned development environments and modular project templates for TypeScript and Rust","license":"MIT","os":["linux"],"cpu":["x64"],"bin":{"devkit":"devkit"},"files":["devkit","runtime","templates","npm","notices","release.json","README.md","LICENSE"],"repository":{"type":"git","url":"git+https://github.com/docouno/devkit.git"}}),
    )?;
    for (name, file) in file_map(&payload)? {
        ensure!(!name.split('/').any(|p| p.eq_ignore_ascii_case("AGENTS.md") || p.eq_ignore_ascii_case("CLAUDE.md")), "Private agent instructions are forbidden in the release payload");
        let mode = if file.mode & 0o111 != 0 { 0o755 } else { 0o644 };
        if file.mode != mode {
            let path = safe_path(&payload, &name)?;
            write_atomic(&path, &fs::read(&path)?, mode)?;
        }
    }
    let manifest = Release {
        schema_version: 1,
        version: version.clone(),
        files: file_map(&payload)?,
        environment: inputs.environment.clone(),
        rust_version: settings["DEVKIT_RUST_VERSION"].clone(),
    };
    write_json(&payload.join("release.json"), &manifest)?;
    let admitted = admit_release(&payload)?;
    let pack_directory = stage.path().join("npm-pack");
    let packed = npm_pack(
        &payload,
        &pack_directory,
        &settings["DEVKIT_NODE_IMAGE"],
        runner,
    )?;
    let cli = verify_npm_pack(&packed, &pack_directory, "@docouno/devkit", &version)?;
    let npm_extracted = stage.path().join("npm-extracted");
    extract_archive(
        &pack_directory.join(&cli.archive),
        &npm_extracted,
        "package",
    )?;
    let npm_normalized = stage.path().join("npm-normalized");
    let npm_release = import_npm_payload(&npm_extracted, &npm_normalized)?;
    ensure!(
        npm_release.identity == admitted.identity
            && npm_release.manifest.files == admitted.manifest.files,
        "Native npm payload differs from canonical release"
    );
    let release_archive = format!("devkit-{version}.tar.gz");
    create_archive(
        &payload,
        &format!("devkit-{version}"),
        &build_set.join(&release_archive),
    )?;
    let archive_extracted = stage.path().join("archive-extracted");
    extract_archive(
        &build_set.join(&release_archive),
        &archive_extracted,
        &format!("devkit-{version}"),
    )?;
    ensure!(
        admit_release(&archive_extracted)?.identity == admitted.identity,
        "Tar payload differs from canonical release"
    );
    let environment_filename = Path::new(&inputs.environment.archive)
        .file_name()
        .context("Environment filename")?
        .to_str()
        .context("Environment filename UTF8")?
        .to_owned();
    let environment_bytes = fs::read(safe_path(&npm_inputs, &inputs.environment.archive)?)?;
    let environment = NpmArtifact {
        name: "@docouno/env".to_owned(),
        version: version.clone(),
        archive: environment_filename.clone(),
        integrity: sri(&environment_bytes),
        shasum: format!("{:x}", Sha1::digest(&environment_bytes)),
    };
    write_atomic(
        &build_set.join(&cli.archive),
        &fs::read(pack_directory.join(&cli.archive))?,
        0o644,
    )?;
    write_atomic(
        &build_set.join(&environment_filename),
        &environment_bytes,
        0o644,
    )?;
    write_atomic(&build_set.join(ENGINE_ASSET), &fs::read(&engine)?, 0o755)?;
    write_atomic(
        &build_set.join("release.json"),
        &fs::read(payload.join("release.json"))?,
        0o644,
    )?;
    let bootstrap = render_bootstrap(&version, &engine_sha, TARGET)?;
    for name in ["install.sh", "bootstrap.sh"] {
        write_atomic(&build_set.join(name), bootstrap.as_bytes(), 0o755)?;
    }
    let mut assets = BTreeMap::new();
    for entry in sorted_entries(&build_set)? {
        if entry.file_name() == "payload" {
            continue;
        }
        assets.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Non-UTF8 asset name"))?,
            asset(&entry.path())?,
        );
    }
    let checksums = assets
        .iter()
        .map(|(name, identity)| format!("{}  {name}\n", identity.sha256))
        .collect::<String>();
    write_atomic(
        &build_set.join("checksums.txt"),
        checksums.as_bytes(),
        0o644,
    )?;
    assets.insert(
        "checksums.txt".to_owned(),
        asset(&build_set.join("checksums.txt"))?,
    );
    let index = ReleaseIndex {
        schema_version: 1,
        version: version.clone(),
        target: TARGET.to_owned(),
        source_commit: options.source_commit.clone(),
        source_tree: options.source_tree.clone(),
        build_run: options.build_run.clone(),
        publication_eligible: !options.local,
        release_identity: admitted.identity.clone(),
        engine: ENGINE_ASSET.to_owned(),
        archive: release_archive,
        packages: BTreeMap::from([
            ("cli".to_owned(), cli),
            ("environment".to_owned(), environment),
        ]),
        files: assets,
    };
    index.validate()?;
    runner.check_cancelled()?;
    admitted.recheck()?;
    ensure!(
        source_snapshot(&root)? == source_before
            && file_map(&npm_inputs)? == npm_before
            && file_map(&notices)? == notices_before
            && file_digest(&engine)? == engine_sha,
        "Assembly inputs changed while staging"
    );
    verify_git(options, &root, runner)?;
    // Written last, outside the payload and both package archives.
    write_json(&build_set.join("release-index.json"), &index)?;
    runner.check_cancelled()?;
    publish_new_directory(&build_set, &output)?;
    Ok(index)
}

#[cfg(test)]
mod source_git_tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    #[test]
    fn every_ambient_git_control_is_removed_before_explicit_read_only_defaults() {
        let controls = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CEILING_DIRECTORIES",
            "GIT_SHALLOW_FILE",
            "GIT_NAMESPACE",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
            "GIT_OPTIONAL_LOCKS",
            "GIT_NO_REPLACE_OBJECTS",
            "GIT_FUTURE_CONTROL",
        ];
        let command = isolated_git_command(Path::new("/source"), controls.map(OsString::from));
        let configured = command.get_envs().collect::<BTreeMap<_, _>>();
        for name in controls {
            let expected = match name {
                "GIT_CONFIG_NOSYSTEM" | "GIT_NO_REPLACE_OBJECTS" => Some(OsStr::new("1")),
                "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_GLOBAL" => Some(OsStr::new("/dev/null")),
                "GIT_OPTIONAL_LOCKS" => Some(OsStr::new("0")),
                _ => None,
            };
            assert_eq!(configured.get(OsStr::new(name)), Some(&expected), "{name}");
        }
    }
}
