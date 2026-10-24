//! Immutable release/engine cache. Only explicit lifecycle acquisition uses the
//! network; accepted daily commands select a verified local engine.
use crate::{
    Result,
    archive::{self, publish_new_directory},
    delivery::*,
    distribution::*,
    files::*,
    model::Release,
    processes::{OutputMode, ProcessRunner},
    release::{self, AdmittedRelease},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

pub fn legacy(version: &str) -> bool {
    version.starts_with("0.2.")
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseRef {
    schema_version: u32,
    version: String,
    identity: String,
}

pub struct ReleaseCache<'a> {
    pub root: PathBuf,
    runner: &'a ProcessRunner,
    executable: PathBuf,
}

struct CacheLock(File);
impl Drop for CacheLock {
    fn drop(&mut self) {
        // SAFETY: this descriptor belongs to this lock instance.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn cache_root(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = if let Some(path) = explicit {
        path.to_owned()
    } else if let Some(path) = std::env::var_os("DEVKIT_CACHE_DIR").filter(|s| !s.is_empty()) {
        PathBuf::from(path)
    } else {
        let base = if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty())
        {
            PathBuf::from(path)
        } else {
            PathBuf::from(
                std::env::var_os("HOME")
                    .context("Set HOME or XDG_CACHE_HOME for the Devkit cache")?,
            )
            .join(".cache")
        };
        base.join("docouno/devkit")
    };
    ensure!(
        path.is_absolute(),
        "The Devkit cache directory must be absolute"
    );
    let path = project_root(&path)?;
    ensure!(
        path.parent().is_some(),
        "The filesystem root cannot be the Devkit cache"
    );
    if exists(&path)? {
        ensure!(
            fs::symlink_metadata(&path)?.is_dir(),
            "Devkit cache root is not a directory"
        );
    }
    Ok(path)
}

impl<'a> ReleaseCache<'a> {
    pub fn new(explicit: Option<&Path>, runner: &'a ProcessRunner) -> Result<Self> {
        Ok(Self {
            root: cache_root(explicit)?,
            runner,
            executable: fs::canonicalize(std::env::current_exe()?)?,
        })
    }
    fn initialize(&self) -> Result<()> {
        let absent = !exists(&self.root)?;
        create_dir_all_durable(&self.root)?;
        if absent {
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    fn lock(&self, key: &str) -> Result<CacheLock> {
        self.initialize()?;
        let directory = safe_path(&self.root, ".locks")?;
        create_dir_all_durable(&directory)?;
        let path = safe_path(&directory, &format!("{}.lock", digest(key)))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Cache lock must be a regular file"
        );
        let until = Instant::now() + Duration::from_secs(150);
        loop {
            self.runner.check_cancelled()?;
            // SAFETY: a live descriptor and nonblocking advisory exclusive lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(CacheLock(file));
            }
            let error = std::io::Error::last_os_error();
            ensure!(
                error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted,
                "Cannot lock cache: {error}"
            );
            ensure!(
                Instant::now() < until,
                "Timed out waiting for another release acquisition"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn release_path(&self, version: &str, identity: &str) -> Result<PathBuf> {
        crate::files::version(version)?;
        ensure!(
            SHA256_RE.is_match(identity),
            "Invalid accepted release identity"
        );
        safe_path(&self.root, &format!("releases/{version}/{identity}"))
    }
    pub fn engine_path(&self, pin: &EnginePin) -> Result<PathBuf> {
        pin.validate()?;
        safe_path(
            &self.root,
            &format!(
                "engines/{}/{}/{}/devkit",
                pin.target, pin.version, pin.sha256
            ),
        )
    }
    fn cached(&self, version: &str, identity: &str) -> Result<Option<AdmittedRelease>> {
        let path = self.release_path(version, identity)?;
        if !exists(&path)? {
            return Ok(None);
        }
        payload_budget(&path)?;
        let admitted = release::admit_release(&path).context(
            "Cached release integrity differs; remove the damaged cache entry before retrying",
        )?;
        ensure!(
            admitted.identity == identity && admitted.manifest.version == version,
            "Cached release identity differs"
        );
        Ok(Some(admitted))
    }
    fn reference(&self, version: &str) -> Result<Option<String>> {
        let path = safe_path(&self.root, &format!("refs/{version}.json"))?;
        if !exists(&path)? {
            return Ok(None);
        }
        ensure!(
            fs::symlink_metadata(&path)?.is_file() && fs::metadata(&path)?.len() <= 4096,
            "Invalid cached version reference"
        );
        let value: ReleaseRef = read_json(&path)?;
        ensure!(
            value.schema_version == 1
                && value.version == version
                && SHA256_RE.is_match(&value.identity),
            "Invalid cached version reference"
        );
        Ok(Some(value.identity))
    }
    fn remember(&self, release: &AdmittedRelease) -> Result<()> {
        write_json(
            &safe_path(
                &self.root,
                &format!("refs/{}.json", release.manifest.version),
            )?,
            &ReleaseRef {
                schema_version: 1,
                version: release.manifest.version.clone(),
                identity: release.identity.clone(),
            },
        )
    }
    pub fn prime_engine(&self, release: &AdmittedRelease) -> Result<()> {
        if legacy(&release.manifest.version) {
            return Ok(());
        }
        let pin = EnginePin {
            version: release.manifest.version.clone(),
            target: TARGET.to_owned(),
            sha256: release
                .manifest
                .files
                .get("devkit")
                .context("Native release has no engine")?
                .sha256
                .clone(),
        };
        let pins = crate::runtime::ToolPins::parse(&release.bytes("runtime/toolchains.env")?)?;
        ensure!(
            pins.engine.as_ref() == Some(&pin),
            "Release engine pin differs from its actual payload"
        );
        let _lock = self.lock(&format!(
            "engine:{}:{}:{}",
            pin.target, pin.version, pin.sha256
        ))?;
        let target = self.engine_path(&pin)?;
        if exists(&target)? {
            verify_engine(&target, &pin)?;
            return Ok(());
        }
        let parent = target
            .parent()
            .context("Engine cache parent")?
            .parent()
            .context("Engine cache namespace")?;
        create_dir_all_durable(parent)?;
        let stage = tempfile::Builder::new()
            .prefix(".engine-")
            .tempdir_in(parent)?;
        let payload = stage.path().join("payload");
        fs::create_dir(&payload)?;
        write_atomic(&payload.join("devkit"), &release.bytes("devkit")?, 0o755)?;
        verify_engine(&payload.join("devkit"), &pin)?;
        release.recheck()?;
        self.runner.check_cancelled()?;
        let destination = target.parent().unwrap();
        if exists(destination)? {
            // The initial loader creates this namespace before downloading.
            // A cancelled transfer may therefore leave a valid empty directory.
            // Link complete verified bytes without replacing any concurrent file.
            match fs::hard_link(payload.join("devkit"), &target) {
                Ok(()) => sync_directory(destination)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_engine(&target, &pin)?;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            publish_new_directory(&payload, destination)?;
        }
        Ok(())
    }
    pub fn accepted_engine(&self, pin: &EnginePin) -> Result<PathBuf> {
        let target = self.engine_path(pin)?;
        ensure!(
            exists(&target)?,
            "The accepted native engine is unavailable; run make setup"
        );
        verify_engine(&target, pin)?;
        Ok(target)
    }
    pub fn fetch(&self, url: &str, output: &Path, maximum: u64) -> Result<()> {
        self.runner.checked_with_timeout(
            Command::new(&self.executable)
                .args(["internal", "fetch", "--url"])
                .arg(url)
                .arg("--output")
                .arg(output)
                .arg("--max-bytes")
                .arg(maximum.to_string()),
            OutputMode::Capture(32 * 1024),
            Duration::from_secs(100),
        )?;
        self.runner.check_cancelled()?;
        ensure!(
            fs::metadata(output)?.len() <= maximum,
            "Downloaded artifact exceeded its byte budget"
        );
        Ok(())
    }
    pub fn acquire(
        &self,
        version: &str,
        expected: Option<&str>,
        explicit_source: Option<&Path>,
        offline: bool,
    ) -> Result<AdmittedRelease> {
        crate::files::version(version)?;
        if let Some(expected) = expected {
            ensure!(
                SHA256_RE.is_match(expected),
                "Invalid accepted release identity"
            );
        }
        self.runner.check_cancelled()?;
        if let Some(source) = explicit_source {
            return self.import(source, version, expected);
        }
        if let Some(identity) = expected
            && let Some(release) = self.cached(version, identity)?
        {
            self.prime_engine(&release)?;
            return Ok(release);
        }
        let adjacent = self
            .executable
            .parent()
            .context("Native executable parent")?;
        if version == env!("CARGO_PKG_VERSION")
            && adjacent.join("release.json").is_file()
            && expected.is_none_or(|identity| {
                release::release_identity(adjacent).is_ok_and(|actual| actual == identity)
            })
        {
            return self.import(adjacent, version, expected);
        }
        if expected.is_none()
            && let Some(identity) = self.reference(version)?
            && let Some(release) = self.cached(version, &identity)?
        {
            let engine_matches = version != env!("CARGO_PKG_VERSION")
                || legacy(version)
                || release.manifest.files.get("devkit").is_some_and(|id| {
                    file_digest(&self.executable).is_ok_and(|hash| hash == id.sha256)
                });
            if engine_matches {
                self.prime_engine(&release)?;
                return Ok(release);
            }
        }
        ensure!(
            !offline,
            "The exact release is not cached; acquire it explicitly before using offline mode"
        );
        self.initialize()?;
        let stage = tempfile::Builder::new()
            .prefix(".acquire-")
            .tempdir_in(&self.root)?;
        let base = format!("{RELEASE_BASE}/v{version}");
        let archive_name = format!("devkit-{version}.tar.gz");
        let archive = stage.path().join(&archive_name);
        let identity;
        let archive_hash;
        if legacy(version) {
            let manifest = stage.path().join("release.json");
            self.fetch(&format!("{base}/release.json"), &manifest, 4 * 1024 * 1024)?;
            identity = file_digest(&manifest)?;
            let checksum = stage.path().join("checksum");
            self.fetch(&format!("{base}/{archive_name}.sha256"), &checksum, 256)?;
            let value = fs::read_to_string(checksum)?;
            let (hash, name) = value
                .trim_end_matches('\n')
                .split_once("  ")
                .context("Invalid archive checksum record")?;
            ensure!(
                SHA256_RE.is_match(hash)
                    && name == archive_name
                    && value == format!("{hash}  {name}\n"),
                "Invalid archive checksum record"
            );
            archive_hash = hash.to_owned();
        } else {
            let index_path = stage.path().join("release-index.json");
            self.fetch(
                &format!("{base}/release-index.json"),
                &index_path,
                64 * 1024,
            )?;
            let index: ReleaseIndex = read_json(&index_path)?;
            index.validate()?;
            ensure!(
                index.version == version && index.publication_eligible,
                "Release index version/provenance differs"
            );
            if version == env!("CARGO_PKG_VERSION") && expected.is_none() {
                ensure!(
                    index.files[&index.engine].sha256 == file_digest(&self.executable)?,
                    "Published engine differs from the running release"
                );
            }
            identity = index.release_identity;
            archive_hash = index.files[&index.archive].sha256.clone();
        }
        ensure!(
            expected.is_none_or(|expected| expected == identity),
            "Downloaded release differs from the accepted project identity"
        );
        let _lock = self.lock(&format!("release:{version}:{identity}"))?;
        if let Some(release) = self.cached(version, &identity)? {
            drop(_lock);
            self.prime_engine(&release)?;
            return Ok(release);
        }
        self.fetch(
            &format!("{base}/{archive_name}"),
            &archive,
            archive::MAX_COMPRESSED,
        )?;
        ensure!(
            file_digest(&archive)? == archive_hash,
            "Downloaded archive checksum mismatch"
        );
        let unpacked = stage.path().join("payload");
        archive::extract_archive_with(
            &archive,
            &unpacked,
            &format!("devkit-{version}"),
            Some(self.runner),
        )?;
        payload_budget(&unpacked)?;
        let release = release::admit_release(&unpacked)?;
        ensure!(
            release.identity == identity && release.manifest.version == version,
            "Downloaded payload differs from release metadata"
        );
        let target = self.release_path(version, &identity)?;
        create_dir_all_durable(target.parent().unwrap())?;
        self.runner.check_cancelled()?;
        release.recheck()?;
        publish_new_directory(&unpacked, &target)?;
        let release = release::admit_release(&target)?;
        self.remember(&release)?;
        drop(_lock);
        self.prime_engine(&release)?;
        Ok(release)
    }
    pub fn import(
        &self,
        source: &Path,
        version: &str,
        expected: Option<&str>,
    ) -> Result<AdmittedRelease> {
        let source = project_root(source)?;
        payload_budget(&source)?;
        let manifest_path = safe_path(&source, "release.json")?;
        let info = fs::symlink_metadata(&manifest_path)?;
        ensure!(
            info.is_file()
                && info.len() <= 4 * 1024 * 1024
                && info.permissions().mode() & 0o111 == 0,
            "Invalid source release manifest"
        );
        let manifest: Release = read_json(&manifest_path)?;
        ensure!(
            manifest.version == version,
            "Source release version differs from requested version"
        );
        let identity = release::release_identity(&source)?;
        ensure!(
            expected.is_none_or(|expected| expected == identity),
            "Source release differs from accepted project identity"
        );
        let _lock = self.lock(&format!("release:{version}:{identity}"))?;
        let stage = tempfile::Builder::new()
            .prefix(".import-")
            .tempdir_in(&self.root)?;
        let payload = stage.path().join("payload");
        let admitted = release::import_npm_payload(&source, &payload)?;
        ensure!(
            admitted.identity == identity,
            "Source release changed during import"
        );
        let release = if let Some(release) = self.cached(version, &identity)? {
            release
        } else {
            let target = self.release_path(version, &identity)?;
            create_dir_all_durable(target.parent().unwrap())?;
            self.runner.check_cancelled()?;
            admitted.recheck()?;
            publish_new_directory(&payload, &target)?;
            release::admit_release(&target)?
        };
        self.remember(&release)?;
        drop(_lock);
        self.prime_engine(&release)?;
        Ok(release)
    }
}

fn payload_budget(root: &Path) -> Result<()> {
    let mut count = 0_usize;
    let mut bytes = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "Symlink in release source/cache: {}",
            path.display()
        );
        count += 1;
        ensure!(count <= 16_384, "Release source/cache exceeds entry budget");
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else {
            ensure!(
                meta.is_file() && meta.len() <= 64 * 1024 * 1024,
                "Invalid or oversized release source/cache entry"
            );
            bytes = bytes
                .checked_add(meta.len())
                .context("Release payload size overflow")?;
            ensure!(
                bytes <= archive::MAX_EXPANDED,
                "Release source/cache exceeds byte budget"
            );
        }
    }
    Ok(())
}
