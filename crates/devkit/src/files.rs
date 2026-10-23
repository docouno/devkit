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
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
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
pub fn project_root(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let result = lexical(&absolute)?;
    check_ancestors(&result)?;
    Ok(result)
}
fn check_ancestors(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    let count = path.components().count();
    for (i, c) in path.components().enumerate() {
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
    check_ancestors(&path)?;
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
pub fn validate_identity(value: &FileIdentity) -> Result<()> {
    ensure!(
        SHA256_RE.is_match(&value.sha256) && value.mode <= 0o777,
        "Invalid file identity"
    );
    Ok(())
}
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("Missing parent")?;
    fs::create_dir_all(parent)?;
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
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}
pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if absent(&e) => Ok(()),
        Err(e) => Err(e.into()),
    }
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
    fn copy(
        root: &Path,
        p: &Path,
        t: &Path,
        ignore: &dyn Fn(&str) -> bool,
        links: bool,
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
                copy(root, &e.path(), &t.join(e.file_name()), ignore, links)?;
            }
        } else if info.is_file() {
            write_atomic(t, &fs::read(p)?, info.permissions().mode() & 0o777)?;
        } else {
            bail!("Unsupported input file type: {}", p.display());
        }
        Ok(())
    }
    copy(source, source, target, ignore, preserve_links)
}
