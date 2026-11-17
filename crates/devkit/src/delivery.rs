//! Native engine identity and the two minimal initial loaders.
use crate::{
    Result,
    distribution::*,
    files::*,
    processes::{OutputMode, ProcessRunner},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnginePin {
    pub version: String,
    pub target: String,
    pub sha256: String,
}
impl EnginePin {
    pub fn validate(&self) -> Result<()> {
        version(&self.version)?;
        ensure!(
            self.target == TARGET,
            "Unsupported native target: {}",
            self.target
        );
        ensure!(
            SHA256_RE.is_match(&self.sha256),
            "Invalid native engine SHA256"
        );
        Ok(())
    }
}

pub fn file_digest(path: &Path) -> Result<String> {
    let path = project_root(path)?;
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    let mut file = fs::File::open(path)?;
    hash_reader(&mut file)
}
fn hash_reader(file: &mut fs::File) -> Result<String> {
    use sha2::{Digest, Sha256};
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn verify_engine(path: &Path, pin: &EnginePin) -> Result<()> {
    pin.validate()?;
    verify_static_elf_file(path, &pin.sha256, false)
}

/// Verify an executable without retaining its contents in memory. Open once so
/// the checksum and ELF inspection always refer to the same inode.
pub fn verify_static_elf_file(
    path: &Path,
    expected_sha256: &str,
    require_mode_0755: bool,
) -> Result<()> {
    ensure!(
        SHA256_RE.is_match(expected_sha256),
        "Invalid executable SHA256"
    );
    let path = project_root(path)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Native engine must be a regular file");
    ensure!(
        !require_mode_0755 || metadata.permissions().mode() & 0o7777 == 0o755,
        "Cached executable mode differs from canonical 0755"
    );
    executable_access(&path)?;
    ensure!(
        hash_reader(&mut file)? == expected_sha256,
        "Native engine checksum differs from accepted pin"
    );
    verify_elf_reader(&mut file)
}
fn executable_access(path: &Path) -> Result<()> {
    let encoded = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: one live C path; faccessat evaluates the current effective user's
    // execution permission, including ACLs. A 0700 npm installation is valid.
    ensure!(
        unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                encoded.as_ptr(),
                libc::X_OK,
                libc::AT_EACCESS,
            )
        } == 0,
        "Native engine is not executable by the current user"
    );
    Ok(())
}
fn verify_elf_reader(file: &mut fs::File) -> Result<()> {
    let metadata = file.metadata()?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0_u8; 64];
    file.read_exact(&mut bytes)
        .context("Truncated native ELF header")?;
    ensure!(
        bytes.len() >= 64
            && &bytes[..7] == b"\x7fELF\x02\x01\x01"
            && u16::from_le_bytes([bytes[18], bytes[19]]) == 62,
        "Native engine must be a Linux x86_64 ELF"
    );
    let offset = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    let entry_size = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as u64;
    ensure!(
        entry_size >= 56 && count <= 128,
        "Invalid native ELF program headers"
    );
    for n in 0..count {
        let start = offset
            .checked_add(n * entry_size)
            .context("ELF header overflow")?;
        file.seek(SeekFrom::Start(start))?;
        let mut record = [0_u8; 56];
        file.read_exact(&mut record)
            .context("Truncated native ELF")?;
        let kind = u32::from_le_bytes(record[..4].try_into().unwrap());
        ensure!(kind != 3, "Native engine has a dynamic program interpreter");
        if kind == 2 {
            let start = u64::from_le_bytes(record[8..16].try_into().unwrap());
            let length = u64::from_le_bytes(record[32..40].try_into().unwrap());
            let end = start
                .checked_add(length)
                .context("ELF dynamic segment overflow")?;
            ensure!(
                end <= metadata.len() && length % 16 == 0,
                "Truncated ELF dynamic segment"
            );
            file.seek(SeekFrom::Start(start))?;
            for _ in 0..length / 16 {
                let mut item = [0_u8; 16];
                file.read_exact(&mut item)?;
                ensure!(
                    u64::from_le_bytes(item[..8].try_into().unwrap()) != 1,
                    "Native engine requires a dynamic library"
                );
            }
        }
    }
    Ok(())
}

// This authority lives only in the executing process. The descriptor pins the
// inode and every reuse checks its physical identity and timestamps, the path,
// and /proc/self/exe. No environment flag or persistent stat cache grants trust.
#[derive(Debug, PartialEq, Eq)]
struct ExecutableStamp(u64, u64, u64, i64, i64, i64, i64);
fn executable_stamp(m: &fs::Metadata) -> ExecutableStamp {
    ExecutableStamp(
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}
struct CurrentExecutable {
    file: fs::File,
    path: PathBuf,
    stamp: ExecutableStamp,
    sha256: String,
}
static CURRENT_EXECUTABLE: Mutex<Option<CurrentExecutable>> = Mutex::new(None);
fn with_current<T>(read: impl FnOnce(&CurrentExecutable) -> Result<T>) -> Result<T> {
    let mut cached = CURRENT_EXECUTABLE
        .lock()
        .map_err(|_| anyhow::anyhow!("Current executable verification failed"))?;
    if cached.is_none() {
        let path = fs::canonicalize(std::env::current_exe()?)?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "Current engine is not a regular executable"
        );
        let stamp = executable_stamp(&metadata);
        let sha256 = hash_reader(&mut file)?;
        verify_elf_reader(&mut file)?;
        *cached = Some(CurrentExecutable {
            file,
            path,
            stamp,
            sha256,
        });
    }
    let current = cached.as_ref().expect("verified executable");
    let path = fs::symlink_metadata(&current.path)?;
    let executing = fs::metadata("/proc/self/exe")?;
    ensure!(
        path.is_file()
            && executable_stamp(&path) == current.stamp
            && executable_stamp(&current.file.metadata()?) == current.stamp
            && (executing.dev(), executing.ino()) == (path.dev(), path.ino()),
        "Current native engine changed during operation"
    );
    executable_access(&current.path)?;
    read(current)
}
pub fn current_engine_digest() -> Result<String> {
    with_current(|current| Ok(current.sha256.clone()))
}
pub fn current_engine_path() -> Result<PathBuf> {
    with_current(|current| Ok(current.path.clone()))
}
pub fn verify_current_engine(pin: &EnginePin) -> Result<PathBuf> {
    pin.validate()?;
    with_current(|current| {
        ensure!(
            current.sha256 == pin.sha256,
            "Native engine checksum differs from accepted pin"
        );
        Ok(current.path.clone())
    })
}

/// Engine-cache files are published with one canonical mode. Unlike a source
/// npm tree, this private copy is never restricted by npm's install umask.
pub fn verify_cached_engine(path: &Path, pin: &EnginePin) -> Result<()> {
    let path = project_root(path)?;
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o7777 == 0o755,
        "Cached native engine mode differs from canonical 0755; restore the accepted executable before retrying"
    );
    verify_engine(&path, pin)
}

pub fn engine_build_info(path: &Path, runner: &ProcessRunner) -> Result<BuildInfo> {
    let result = runner.checked_with_timeout(
        Command::new(path).args(["internal", "build-info"]),
        OutputMode::Capture(64 * 1024),
        std::time::Duration::from_secs(5),
    )?;
    let info: BuildInfo = serde_json::from_slice(&result.stdout)?;
    ensure!(
        info.schema_version == 1 && info.target == TARGET,
        "Unsupported native engine build"
    );
    version(&info.version)?;
    Ok(info)
}

fn render(
    source: &str,
    mode: &str,
    version: &str,
    engine_sha: &str,
    target: &str,
) -> Result<String> {
    EnginePin {
        version: version.to_owned(),
        target: target.to_owned(),
        sha256: engine_sha.to_owned(),
    }
    .validate()?;
    let mut result = source.to_owned();
    for (token, value) in [
        ("@@DEVKIT_MODE@@", mode),
        ("@@DEVKIT_VERSION@@", version),
        ("@@DEVKIT_ENGINE_SHA256@@", engine_sha),
        ("@@DEVKIT_ENGINE_TARGET@@", target),
    ] {
        ensure!(
            result.matches(token).count() == 1,
            "Invalid loader placeholder count: {token}"
        );
        result = result.replace(token, value);
    }
    ensure!(
        !result.contains("@@"),
        "Unresolved native loader placeholder"
    );
    Ok(result)
}
pub fn render_project_loader(version: &str, engine_sha: &str, target: &str) -> Result<String> {
    render(
        include_str!("../../../runtime/devkit.sh"),
        "project",
        version,
        engine_sha,
        target,
    )
}
pub fn render_bootstrap(version: &str, engine_sha: &str, target: &str) -> Result<String> {
    render(
        include_str!("../../../runtime/devkit.sh"),
        "bootstrap",
        version,
        engine_sha,
        target,
    )
}
