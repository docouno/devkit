use crate::{Result, model::FileIdentity};
use anyhow::{Context, bail, ensure};
use regex::Regex;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
};

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
pub static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?$").unwrap()
});
pub static SLUG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9]*(-[a-z0-9]+)*$").unwrap());
pub static RELATIVE_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._/-]+$").unwrap());
pub static SHA256_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-f0-9]{64}$").unwrap());
pub static GENERATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-f0-9-]{36}$").unwrap());
pub static TOOL_DEPENDENCY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:@[a-z0-9-]+/)?[a-z0-9][a-z0-9._-]*$").unwrap());
pub static MANAGED_NPM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\.devkit/npm/[a-z0-9.-]+\.tgz$").unwrap());
pub static MANAGED_RUST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(apps|packages)/[a-z][a-z0-9-]*/rust-toolchain\.toml$").unwrap()
});
pub fn version(value: &str) -> Result<&str> {
    ensure!(
        VERSION_RE.is_match(value),
        "Invalid release version: {value}"
    );
    Ok(value)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrereleaseId {
    Numeric(u64),
    AlphaNumeric(String),
}
impl Ord for PrereleaseId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
            (Self::Numeric(_), Self::AlphaNumeric(_)) => std::cmp::Ordering::Less,
            (Self::AlphaNumeric(_), Self::Numeric(_)) => std::cmp::Ordering::Greater,
            (Self::AlphaNumeric(a), Self::AlphaNumeric(b)) => a.cmp(b),
        }
    }
}
impl PartialOrd for PrereleaseId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<PrereleaseId>,
}
impl SemVer {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty() && value.len() <= 128,
            "Invalid SemVer version: {value}"
        );
        let (without_build, build) = match value.split_once('+') {
            Some((left, right)) => (left, Some(right)),
            None => (value, None),
        };
        if let Some(build) = build {
            ensure!(
                !build.is_empty()
                    && build.split('.').all(|part| {
                        !part.is_empty()
                            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    }),
                "Invalid SemVer build metadata: {value}"
            );
        }
        let (core, pre_raw) = match without_build.split_once('-') {
            Some((left, right)) => (left, Some(right)),
            None => (without_build, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        ensure!(parts.len() == 3, "Invalid SemVer version: {value}");
        let parse_num = |s: &str| -> Result<u64> {
            ensure!(
                !s.is_empty()
                    && (s == "0" || !s.starts_with('0'))
                    && s.bytes().all(|b| b.is_ascii_digit()),
                "Invalid SemVer numeric identifier: {value}"
            );
            s.parse::<u64>()
                .with_context(|| format!("SemVer numeric overflow: {value}"))
        };
        let major = parse_num(parts[0])?;
        let minor = parse_num(parts[1])?;
        let patch = parse_num(parts[2])?;
        let mut prerelease = Vec::new();
        if let Some(pre) = pre_raw {
            ensure!(!pre.is_empty(), "Invalid SemVer prerelease: {value}");
            for part in pre.split('.') {
                ensure!(
                    !part.is_empty()
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "Invalid SemVer prerelease identifier: {value}"
                );
                if part.bytes().all(|b| b.is_ascii_digit()) {
                    ensure!(
                        part == "0" || !part.starts_with('0'),
                        "Invalid SemVer leading zero in prerelease: {value}"
                    );
                    prerelease
                        .push(PrereleaseId::Numeric(part.parse::<u64>().with_context(
                            || format!("SemVer prerelease overflow: {value}"),
                        )?));
                } else {
                    prerelease.push(PrereleaseId::AlphaNumeric(part.to_owned()));
                }
            }
        }
        Ok(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }
}
impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    (false, false) => self.prerelease.cmp(&other.prerelease),
                },
            )
    }
}
impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
pub fn slug(value: &str) -> Result<&str> {
    ensure!(
        value.len() <= 63 && SLUG_RE.is_match(value),
        "Expected a lower-case name containing letters, digits and hyphens: {value}"
    );
    Ok(value)
}
pub fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{label} must be an object"))
}
pub fn text<'a>(value: &'a Value, label: &str) -> Result<&'a str> {
    value
        .as_str()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("{label} must be a nonempty string"))
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid JSON {}", path.display()))
}
pub fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut b = serde_json::to_vec_pretty(value)?;
    b.push(b'\n');
    Ok(b)
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_atomic(path, &json_bytes(value)?, 0o644)
}
pub fn relative_path(value: &str) -> Result<&str> {
    ensure!(
        RELATIVE_PATH_RE.is_match(value)
            && !value
                .split('/')
                .any(|p| ["", ".", "..", "__proto__", "prototype", "constructor"].contains(&p)),
        "Unsafe release path: {value}"
    );
    Ok(value)
}
fn absent(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}
pub fn lexical(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                ensure!(result.pop(), "Path escapes its root: {}", path.display());
            }
            Component::CurDir => {}
            _ => result.push(c.as_os_str()),
        }
    }
    Ok(result)
}
#[cfg(target_os = "macos")]
mod darwin;

pub fn project_root(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let result = lexical(&absolute)?;
    #[cfg(target_os = "macos")]
    let result = darwin::system_alias(result)?;
    check_ancestors(&result)?;
    #[cfg(target_os = "macos")]
    let result = darwin::spelling(&result)?;
    Ok(result)
}
fn check_ancestors(path: &Path) -> Result<()> {
    check_ancestors_from(Path::new(""), path)
}
fn check_ancestors_from(base: &Path, path: &Path) -> Result<()> {
    let suffix = if base.as_os_str().is_empty() {
        path
    } else {
        path.strip_prefix(base)?
    };
    let mut current = base.to_path_buf();
    let count = suffix.components().count();
    for (i, c) in suffix.components().enumerate() {
        current.push(c.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(m) => {
                ensure!(
                    !m.file_type().is_symlink(),
                    "Refusing a symlink in path: {}",
                    current.display()
                );
                ensure!(
                    i + 1 == count || m.is_dir(),
                    "Non-directory path ancestor: {}",
                    current.display()
                );
            }
            Err(e) if absent(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn input_path(root: &Path, name: impl AsRef<Path>) -> Result<PathBuf> {
    let root = project_root(root)?;
    let path = lexical(&root.join(name.as_ref()))?;
    ensure!(
        path != root && path.starts_with(&root),
        "Native input escapes the project: {}",
        name.as_ref().display()
    );
    check_ancestors_from(&root, &path)?;
    Ok(path)
}
pub fn safe_path(root: &Path, name: &str) -> Result<PathBuf> {
    relative_path(name)?;
    input_path(root, name)
}
pub fn relative_name(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)?
        .to_str()
        .context("Non-UTF8 path")?
        .to_owned())
}
pub fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if absent(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub fn identity(path: &Path) -> Result<Option<FileIdentity>> {
    identity_for(path, path)
}

pub(crate) const TOOLCHAIN_PINS_MAX_BYTES: usize = 64 * 1024;
pub(crate) const SECCOMP_PROFILE_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const PNPM_YAML_MAX_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn is_pnpm_yaml(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("pnpm-lock.yaml" | "pnpm-workspace.yaml")
    )
}

fn input_budget(path: &Path) -> Option<(u64, &'static str)> {
    if is_pnpm_yaml(path) {
        return Some((PNPM_YAML_MAX_BYTES as u64, "pnpm YAML"));
    }
    if path.ends_with(".devkit/runtime/toolchains.env") {
        return Some((TOOLCHAIN_PINS_MAX_BYTES as u64, "toolchain pins"));
    }
    if path.ends_with(".devkit/runtime/desktop-seccomp.json") {
        return Some((SECCOMP_PROFILE_MAX_BYTES as u64, "desktop seccomp profile"));
    }
    if path.ends_with(".devkit/runtime/compose.tools.yaml") {
        return Some((
            crate::compose::MAX_YAML_BYTES as u64,
            "Tool Compose projection",
        ));
    }
    let cargo_config = path.ends_with(".cargo/config") || path.ends_with(".cargo/config.toml");
    (cargo_config
        || matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("Cargo.toml" | "Cargo.lock" | "rustfmt.toml" | "rust-toolchain.toml")
        ))
    .then_some((32 * 1024 * 1024, "Cargo metadata"))
}

/// Saved transaction slots retain the acquisition policy of their original input.
pub(crate) fn identity_for(path: &Path, original: &Path) -> Result<Option<FileIdentity>> {
    if let Some((limit, label)) = input_budget(original) {
        return Ok(read_regular_file(path, limit, label)?.map(|(_, identity)| identity));
    }
    let info = match fs::symlink_metadata(path) {
        Ok(i) => i,
        Err(e) if absent(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        info.is_file() && !info.file_type().is_symlink(),
        "Expected a regular file: {}",
        path.display()
    );
    Ok(Some(FileIdentity {
        sha256: digest(fs::read(path)?),
        mode: info.permissions().mode() & 0o777,
    }))
}

pub(crate) fn read_input_bytes(path: &Path) -> Result<Vec<u8>> {
    read_input_bytes_for(path, path)
}

pub(crate) fn read_input_bytes_for(path: &Path, original: &Path) -> Result<Vec<u8>> {
    if let Some((limit, label)) = input_budget(original) {
        return Ok(read_regular_file(path, limit, label)?
            .with_context(|| format!("Missing {label} input: {}", path.display()))?
            .0);
    }
    Ok(fs::read(path)?)
}

/// Acquire bounded generated/native metadata without opening pipes or links.
pub(crate) fn read_regular_file(
    path: &Path,
    maximum: u64,
    label: &str,
) -> Result<Option<(Vec<u8>, FileIdentity)>> {
    Ok(read_regular_snapshot(path, maximum, label)?.map(|(bytes, id, _)| (bytes, id)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp(u64, u64, u64, i64, i64, i64, i64);

impl FileStamp {
    pub(crate) fn of(meta: &fs::Metadata) -> Self {
        Self(
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    }

    pub(crate) fn matches(&self, path: &Path) -> Result<bool> {
        let meta = fs::symlink_metadata(path)?;
        Ok(meta.is_file() && *self == Self::of(&meta))
    }
}

pub(crate) fn read_regular_snapshot(
    path: &Path,
    maximum: u64,
    label: &str,
) -> Result<Option<(Vec<u8>, FileIdentity, FileStamp)>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "{label} must be a regular file: {}",
        path.display()
    );
    let too_large = || {
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("{label} exceeds byte budget: {}", path.display()),
        )
    };
    if metadata.len() > maximum {
        return Err(too_large().into());
    }
    let mut bytes = Vec::new();
    (&file)
        .take(maximum.checked_add(1).context("Invalid file byte budget")?)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(too_large().into());
    }
    let identity = FileIdentity {
        sha256: digest(&bytes),
        mode: metadata.permissions().mode() & 0o777,
    };
    let stamp = FileStamp::of(&metadata);
    ensure!(
        bytes.len() as u64 == metadata.len()
            && stamp == FileStamp::of(&file.metadata()?)
            && stamp.matches(path)?,
        "{label} changed while reading: {}",
        path.display()
    );
    Ok(Some((bytes, identity, stamp)))
}
pub fn validate_identity(value: &FileIdentity) -> Result<()> {
    ensure!(
        SHA256_RE.is_match(&value.sha256) && value.mode <= 0o777,
        "Invalid file identity"
    );
    Ok(())
}
pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("Cannot open directory for durability: {}", path.display()))?
        .sync_all()
        .with_context(|| format!("Cannot synchronize directory: {}", path.display()))
}

/// Flush dirty file data pages from kernel UBC to the storage controller on
/// macOS without issuing a per-file F_FULLFSYNC hardware cache flush. Pair
/// with a single `sync_directory` (`F_FULLFSYNC`) barrier at the end of a batch.
#[cfg(target_os = "macos")]
pub(crate) fn flush_file_pages(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: `file` owns a live open descriptor for the duration of the call.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}
/// Install each new directory entry durably, not only the final file's parent.
/// A journal may be published from a different directory than its saved data.
pub fn create_dir_all_durable(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = if path.as_os_str().is_empty() {
        Path::new(".").to_path_buf()
    } else {
        path.to_path_buf()
    };
    #[cfg(target_os = "macos")]
    {
        current = darwin::system_alias(current)?;
    }
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Expected a regular directory: {}",
                    current.display()
                );
                check_ancestors(&current)?;
                break;
            }
            Err(error) if absent(&error) => {
                missing.push(current.clone());
                current = current
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."))
                    .to_path_buf();
            }
            Err(error) => return Err(error.into()),
        }
    }
    for directory in missing.into_iter().rev() {
        match fs::create_dir(&directory) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&directory)?;
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Concurrent non-directory creation: {}",
                    directory.display()
                );
            }
            Err(error) => return Err(error.into()),
        }
        sync_directory(&directory)?;
        sync_directory(
            directory
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
    }
    Ok(())
}
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_dir_all_durable(parent)?;
    let temp = parent.join(format!(".devkit-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}
pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => (),
        Err(e) if absent(&e) => (),
        Err(e) => return Err(e.into()),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if exists(parent)? {
        sync_directory(parent)?;
    }
    Ok(())
}
pub fn remove_empty_dir_durable(path: &Path) -> Result<()> {
    fs::remove_dir(path)?;
    sync_directory(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
}
/// Prune only directories below the managed runtime. Recovery recreates each
/// removed parent durably when restoring its first backed-up file.
pub(crate) fn remove_file_and_empty_runtime_parents(root: &Path, name: &str) -> Result<()> {
    remove_file(&safe_path(root, name)?)?;
    for parent in Path::new(name).ancestors().skip(1) {
        let name = parent.to_str().context("Non-UTF8 runtime path")?;
        if !name.starts_with(".devkit/runtime/") {
            break;
        }
        let path = safe_path(root, name)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(meta.is_dir(), "Expected a regular directory: {name}"),
            Err(e) if absent(&e) => break,
            Err(e) => return Err(e.into()),
        }
        if !sorted_entries(&path)?.is_empty() {
            break;
        }
        remove_empty_dir_durable(&path)?;
    }
    Ok(())
}
pub fn remove_tree_durable(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => (),
        Err(error) if absent(&error) => (),
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if exists(parent)? {
        sync_directory(parent)?;
    }
    Ok(())
}
pub fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut v = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    v.sort_by_key(|e| e.file_name());
    Ok(v)
}
pub fn file_map(root: &Path) -> Result<BTreeMap<String, FileIdentity>> {
    fn visit(root: &Path, dir: &Path, result: &mut BTreeMap<String, FileIdentity>) -> Result<()> {
        for e in sorted_entries(dir)? {
            let p = e.path();
            let n = relative_name(root, &p)?;
            relative_path(&n)?;
            let m = fs::symlink_metadata(&p)?;
            ensure!(
                !m.file_type().is_symlink(),
                "Release payload contains a symlink: {n}"
            );
            if m.is_dir() {
                visit(root, &p, result)?;
            } else {
                result.insert(n, identity(&p)?.context("File disappeared")?);
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result)?;
    Ok(result)
}
pub fn copy_tree(
    source: &Path,
    target: &Path,
    ignore: &dyn Fn(&str) -> bool,
    preserve_links: bool,
) -> Result<()> {
    copy_tree_inner(source, target, ignore, preserve_links, true)
}

/// Fast symlink-safe copy for unpublished scratch staging trees that are never
/// committed in place. Final transaction durability lives in PreparedTransaction.
pub(crate) fn copy_tree_scratch(
    source: &Path,
    target: &Path,
    ignore: &dyn Fn(&str) -> bool,
    preserve_links: bool,
) -> Result<()> {
    copy_tree_inner(source, target, ignore, preserve_links, false)
}

fn copy_file_scratch(source: &Path, target: &Path, mode: u32) -> Result<()> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)
        .with_context(|| format!("Cannot open {}", source.display()))?;
    ensure!(
        input.metadata()?.is_file(),
        "Expected regular file: {}",
        source.display()
    );
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(target)
        .with_context(|| format!("Cannot create staged copy {}", target.display()))?;
    io::copy(&mut input, &mut output)?;
    output.set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn copy_tree_inner(
    source: &Path,
    target: &Path,
    ignore: &dyn Fn(&str) -> bool,
    preserve_links: bool,
    durable: bool,
) -> Result<()> {
    fn copy(
        root: &Path,
        p: &Path,
        t: &Path,
        ignore: &dyn Fn(&str) -> bool,
        links: bool,
        durable: bool,
    ) -> Result<()> {
        let n = relative_name(root, p)?;
        if !n.is_empty() && ignore(&n) {
            return Ok(());
        }
        let info = fs::symlink_metadata(p)?;
        if info.file_type().is_symlink() {
            ensure!(links, "Refusing payload symlink: {}", p.display());
            fs::create_dir_all(t.parent().context("Parent")?)?;
            symlink(fs::read_link(p)?, t)?;
        } else if info.is_dir() {
            fs::create_dir_all(t)?;
            for e in sorted_entries(p)? {
                copy(
                    root,
                    &e.path(),
                    &t.join(e.file_name()),
                    ignore,
                    links,
                    durable,
                )?;
            }
        } else if info.is_file() {
            let mode = info.permissions().mode() & 0o777;
            if durable {
                write_atomic(t, &read_input_bytes(p)?, mode)?;
            } else {
                copy_file_scratch(p, t, mode)?;
            }
        } else if links && (info.file_type().is_socket() || info.file_type().is_fifo()) {
            return Ok(());
        } else {
            bail!("Unsupported input file type: {}", p.display());
        }
        Ok(())
    }
    copy(source, source, target, ignore, preserve_links, durable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_runtime_limits_apply_to_files_and_their_transaction_slots() -> Result<()> {
        let project = tempfile::tempdir()?;
        let slot = project.path().join("saved-file");
        let file = File::create(&slot)?;
        for (name, limit) in [
            ("toolchains.env", TOOLCHAIN_PINS_MAX_BYTES),
            ("desktop-seccomp.json", SECCOMP_PROFILE_MAX_BYTES),
            ("compose.tools.yaml", crate::compose::MAX_YAML_BYTES),
        ] {
            file.set_len(limit as u64 + 1)?;
            let relative = Path::new(".devkit/runtime").join(name);
            let managed = project.path().join(&relative);
            create_dir_all_durable(managed.parent().unwrap())?;
            fs::hard_link(&slot, &managed)?;

            for result in [
                identity(&managed).map(|_| ()),
                read_input_bytes(&managed).map(|_| ()),
                identity_for(&slot, &relative).map(|_| ()),
                read_input_bytes_for(&slot, &relative).map(|_| ()),
            ] {
                let error = result.unwrap_err().downcast::<io::Error>()?;
                assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
            }

            // The same name at the project root is a project-owned file.
            let product = project.path().join(name);
            fs::hard_link(&slot, &product)?;
            assert_eq!(read_input_bytes(&product)?.len(), limit + 1);
        }
        Ok(())
    }
}
