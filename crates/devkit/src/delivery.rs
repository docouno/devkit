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
    io::Read,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
    process::Command,
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
    use sha2::{Digest, Sha256};
    let path = project_root(path)?;
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    let mut file = fs::File::open(path)?;
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
    let path = project_root(path)?;
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Native engine must be a regular file"
    );
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
    ensure!(
        file_digest(&path)? == pin.sha256,
        "Native engine checksum differs from accepted pin"
    );
    let bytes = fs::read(&path)?;
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
            .context("ELF header overflow")? as usize;
        let record = bytes
            .get(start..start + 56)
            .context("Truncated native ELF")?;
        let kind = u32::from_le_bytes(record[..4].try_into().unwrap());
        ensure!(kind != 3, "Native engine has a dynamic program interpreter");
        if kind == 2 {
            let start = u64::from_le_bytes(record[8..16].try_into().unwrap()) as usize;
            let length = u64::from_le_bytes(record[32..40].try_into().unwrap()) as usize;
            let end = start
                .checked_add(length)
                .context("ELF dynamic segment overflow")?;
            for item in bytes
                .get(start..end)
                .context("Truncated ELF dynamic segment")?
                .as_chunks::<16>()
                .0
            {
                ensure!(
                    u64::from_le_bytes(item[..8].try_into().unwrap()) != 1,
                    "Native engine requires a dynamic library"
                );
            }
        }
    }
    Ok(())
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
