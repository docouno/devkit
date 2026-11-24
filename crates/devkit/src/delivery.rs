//! Native engine identity and the two minimal initial loaders.
use crate::{
    Result,
    distribution::*,
    files::*,
    processes::{OutputMode, ProcessRunner},
};
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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

mod executable;
use executable::verify_format;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnginePin {
    pub version: String,
    pub target: String,
    pub sha256: String,
}
impl EnginePin {
    pub fn validate(&self) -> Result<()> {
        version(&self.version)?;
        Target::parse(&self.target)?;
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
    verify_executable(path, &pin.sha256, Target::parse(&pin.target)?, false)
}

/// Verify an executable without retaining its contents in memory. Open once so
/// the checksum and format inspection always refer to the same inode.
pub fn verify_executable(
    path: &Path,
    expected_sha256: &str,
    target: Target,
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
    verify_format(&mut file, target)?;
    ensure!(
        executable_stamp(&fs::symlink_metadata(&path)?) == executable_stamp(&metadata)
            && executable_stamp(&file.metadata()?) == executable_stamp(&metadata),
        "Native executable changed while verifying"
    );
    Ok(())
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
#[cfg(target_os = "linux")]
fn mapped_executable_identity() -> Result<(u64, u64)> {
    let executable = fs::metadata("/proc/self/exe")?;
    Ok((executable.dev(), executable.ino()))
}

#[cfg(target_os = "macos")]
fn mapped_executable_identity() -> Result<(u64, u64)> {
    unsafe extern "C" {
        fn devkit_mapped_executable_identity(device: *mut u32, inode: *mut u64) -> libc::c_int;
    }
    let (mut device, mut inode) = (0, 0);
    // SAFETY: the SDK adapter writes to two caller-owned valid integers.
    let error = unsafe { devkit_mapped_executable_identity(&mut device, &mut inode) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error))
            .context("Cannot identify the mapped native executable");
    }
    Ok((u64::from(device), inode))
}

// The descriptor pins the inode. Every reuse checks its physical identity
// and timestamps against the file on disk and the executable mapped by the OS.
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
        verify_format(&mut file, Target::host()?)?;
        *cached = Some(CurrentExecutable {
            file,
            path,
            stamp,
            sha256,
        });
    }
    let current = cached.as_ref().expect("verified executable");
    let path = fs::symlink_metadata(&current.path)?;
    let executing = mapped_executable_identity()?;
    ensure!(
        path.is_file()
            && executable_stamp(&path) == current.stamp
            && executable_stamp(&current.file.metadata()?) == current.stamp
            && executing == (path.dev(), path.ino()),
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
    ensure!(
        pin.target == Target::host()?.triple(),
        "Accepted native target does not match this host"
    );
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
    pin.validate()?;
    verify_executable(path, &pin.sha256, Target::parse(&pin.target)?, true)
}

pub fn engine_build_info(path: &Path, runner: &ProcessRunner) -> Result<BuildInfo> {
    let result = runner.checked_with_timeout(
        Command::new(path).args(["internal", "build-info"]),
        OutputMode::Capture(64 * 1024),
        std::time::Duration::from_secs(5),
    )?;
    let info: BuildInfo = serde_json::from_slice(&result.stdout)?;
    ensure!(
        info.schema_version == 1 && info.target == Target::host()?.triple(),
        "Unsupported native engine build"
    );
    version(&info.version)?;
    Ok(info)
}

fn render_host_selection(source: &str, pins: &BTreeMap<String, String>) -> Result<String> {
    let mut cases = Vec::new();
    for target in Target::ALL {
        if let Some(sha256) = pins.get(target.triple()) {
            ensure!(SHA256_RE.is_match(sha256), "Invalid loader engine SHA256");
            let pattern = match target {
                Target::LinuxX64 => "Linux:x86_64",
                Target::LinuxArm64 => "Linux:aarch64|Linux:arm64",
                Target::DarwinArm64 => "Darwin:arm64",
            };
            cases.push(format!(
                "{pattern}) target={}; sha256={sha256}; payload={} ;;",
                target.triple(),
                target.payload_name()
            ));
        }
    }
    ensure!(
        !cases.is_empty() && cases.len() == pins.len(),
        "Invalid loader target set"
    );
    let token = "# @@DEVKIT_ENGINE_PINS@@";
    ensure!(
        source.matches(token).count() == 1,
        "Invalid loader placeholder count: {token}"
    );
    let script = source.replace(token, &cases.join("\n"));
    ensure!(
        !script.contains("@@"),
        "Unresolved native loader placeholder"
    );
    Ok(script)
}

fn render(mode: &str, release_version: &str, pins: &BTreeMap<String, String>) -> Result<String> {
    version(release_version)?;
    let mut script = include_str!("../../../runtime/devkit.sh").to_owned();
    for (token, value) in [
        ("@@DEVKIT_MODE@@", mode),
        ("@@DEVKIT_VERSION@@", release_version),
    ] {
        ensure!(
            script.matches(token).count() == 1,
            "Invalid loader placeholder count: {token}"
        );
        script = script.replace(token, value);
    }
    render_host_selection(&script, pins)
}

pub fn render_project_loader_pins(
    version: &str,
    pins: &BTreeMap<String, String>,
) -> Result<String> {
    render("project", version, pins)
}
pub fn render_bootstrap_pins(version: &str, pins: &BTreeMap<String, String>) -> Result<String> {
    render("bootstrap", version, pins)
}
pub fn render_npm_launcher(pins: &BTreeMap<String, String>) -> Result<String> {
    render_host_selection(include_str!("../../../runtime/npm-launcher.sh"), pins)
}
