//! Canonical release archives. Archive metadata never creates links or devices.
use crate::{Result, files::*, processes::ProcessRunner};
use anyhow::{Context, ensure};
use flate2::{Compression, GzBuilder, read::MultiGzDecoder};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_COMPRESSED: u64 = 128 * 1024 * 1024;
pub const MAX_EXPANDED: u64 = 256 * 1024 * 1024;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;

/// Linux publication must not replace a concurrently created destination,
/// including an empty directory. Callers stage on the same filesystem.
pub fn publish_new_directory(stage: &Path, target: &Path) -> Result<()> {
    let stage = CString::new(stage.as_os_str().as_bytes())?;
    let destination = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both C strings live for the call; Linux is the supported host.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    ensure!(
        result == 0,
        "Cannot publish new directory {}: {}",
        target.display(),
        io::Error::last_os_error()
    );
    sync_directory(target.parent().context("Output parent")?)
}

fn prefix(value: &str) -> Result<()> {
    relative_path(value)?;
    ensure!(!value.contains('/'), "Archive requires one root directory");
    Ok(())
}

struct BudgetReader<'a, R> {
    reader: R,
    remaining: u64,
    deadline: Instant,
    runner: Option<&'a ProcessRunner>,
}
impl<R: Read> Read for BudgetReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other(
                "Archive operation exceeded its time budget",
            ));
        }
        if let Some(runner) = self.runner {
            runner
                .check_cancelled()
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        let limit = usize::try_from(self.remaining.min(buffer.len() as u64)).unwrap();
        if limit == 0 && !buffer.is_empty() {
            let mut extra = [0_u8; 1];
            ensure_io(
                self.reader.read(&mut extra)? == 0,
                "Archive exceeds its expanded byte budget",
            )?;
            return Ok(0);
        }
        let count = self.reader.read(&mut buffer[..limit])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}
fn ensure_io(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}

pub fn extract_archive(archive: &Path, destination: &Path, root_name: &str) -> Result<()> {
    extract_archive_with(archive, destination, root_name, None)
}

pub fn extract_archive_with(
    archive: &Path,
    destination: &Path,
    root_name: &str,
    runner: Option<&ProcessRunner>,
) -> Result<()> {
    prefix(root_name)?;
    let archive = project_root(archive)?;
    let source = fs::symlink_metadata(&archive)?;
    ensure!(
        source.is_file() && source.len() <= MAX_COMPRESSED,
        "Invalid or oversized release archive"
    );
    let destination = project_root(destination)?;
    ensure!(!exists(&destination)?, "Archive destination already exists");
    let parent = destination.parent().context("Archive destination parent")?;
    ensure!(parent.is_dir(), "Archive destination parent must exist");
    let stage = tempfile::Builder::new()
        .prefix(".devkit-extract-")
        .tempdir_in(parent)?;
    let payload = stage.path().join("payload");
    fs::create_dir(&payload)?;
    let mut stream = BudgetReader {
        reader: MultiGzDecoder::new(File::open(&archive)?),
        remaining: MAX_EXPANDED,
        deadline: Instant::now() + Duration::from_secs(30),
        runner,
    };
    let mut names = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut total = 0_u64;
    {
        let mut tar = tar::Archive::new(&mut stream);
        for entry in tar.entries()? {
            ensure!(
                names.len() < MAX_ENTRIES,
                "Archive exceeds its entry budget"
            );
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            ensure!(
                kind.is_file() || kind.is_dir(),
                "Release archive contains a link or special entry"
            );
            let path = entry.path_bytes().into_owned();
            ensure!(path.len() <= 4096, "Archive path exceeds its byte budget");
            let encoded = std::str::from_utf8(&path)?;
            let encoded = if kind.is_dir() {
                encoded.strip_suffix('/').unwrap_or(encoded)
            } else {
                encoded
            };
            relative_path(encoded)?;
            ensure!(
                encoded == root_name || encoded.starts_with(&format!("{root_name}/")),
                "Archive has an unexpected root: {encoded}"
            );
            ensure!(
                names.insert(encoded.to_owned()),
                "Duplicate archive entry: {encoded}"
            );
            let mode = entry.header().mode()?;
            let size = entry.header().size()?;
            if kind.is_dir() {
                ensure!(
                    size == 0 && mode == 0o755,
                    "Invalid archive directory: {encoded}"
                );
                if encoded != root_name {
                    directories.insert(encoded[root_name.len() + 1..].to_owned());
                }
                continue;
            }
            ensure!(
                encoded != root_name && [0o644, 0o755].contains(&mode),
                "Invalid archive regular file: {encoded}"
            );
            ensure!(
                size <= MAX_FILE,
                "Archive entry exceeds its file budget: {encoded}"
            );
            total = total.checked_add(size).context("Archive size overflow")?;
            ensure!(
                total <= MAX_EXPANDED,
                "Archive exceeds its payload byte budget"
            );
            let relative = &encoded[root_name.len() + 1..];
            let target = safe_path(&payload, relative)?;
            fs::create_dir_all(target.parent().context("Archive file parent")?)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .mode(0o600)
                .open(&target)?;
            ensure!(
                io::copy(&mut entry, &mut output)? == size,
                "Truncated archive entry: {encoded}"
            );
            output.set_permissions(fs::Permissions::from_mode(mode))?;
            output.sync_all()?;
            files.insert(relative.to_owned());
        }
    }
    // Consume gzip to its CRC/EOF, including zero tar padding. Concatenated
    // nonempty tar streams and trailing compressed garbage are rejected.
    let mut tail = [0_u8; 8192];
    loop {
        let count = stream.read(&mut tail)?;
        if count == 0 {
            break;
        }
        ensure!(
            tail[..count].iter().all(|v| *v == 0),
            "Unexpected data after tar end marker"
        );
    }
    ensure!(!files.is_empty(), "Release archive has no regular payload");
    for directory in directories {
        ensure!(
            files
                .iter()
                .any(|name| name.starts_with(&format!("{directory}/"))),
            "Unnecessary empty archive directory: {directory}"
        );
    }
    let now = fs::symlink_metadata(&archive)?;
    ensure!(
        now.is_file() && now.len() == source.len(),
        "Archive changed during extraction"
    );
    for name in &files {
        sync_directory(safe_path(&payload, name)?.parent().unwrap())?;
    }
    sync_directory(&payload)?;
    if let Some(runner) = runner {
        runner.check_cancelled()?;
    }
    publish_new_directory(&payload, &destination)
}

pub fn create_archive(source: &Path, root_name: &str, target: &Path) -> Result<()> {
    prefix(root_name)?;
    let source = project_root(source)?;
    let before = file_map(&source)?;
    ensure!(
        !before.is_empty() && before.len() <= MAX_ENTRIES / 2,
        "Invalid archive source file count"
    );
    let target = project_root(target)?;
    ensure!(!exists(&target)?, "Archive output already exists");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600)
        .open(&target)?;
    let result = (|| -> Result<()> {
        let gzip = GzBuilder::new()
            .mtime(0)
            .write(&mut output, Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let mut directories = BTreeSet::from([root_name.to_owned()]);
        for name in before.keys() {
            let mut parent = Path::new(name).parent();
            while let Some(path) = parent {
                if path.as_os_str().is_empty() {
                    break;
                }
                directories.insert(format!(
                    "{root_name}/{}",
                    path.to_str().context("Non-UTF8 release path")?
                ));
                parent = path.parent();
            }
        }
        for name in directories {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, &name, io::empty())?;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut total = 0_u64;
        for (name, id) in &before {
            ensure!(
                [0o644, 0o755].contains(&id.mode),
                "Noncanonical archive source mode: {name}"
            );
            let file = File::open(safe_path(&source, name)?)?;
            let size = file.metadata()?.len();
            ensure!(
                size <= MAX_FILE,
                "Archive source entry is too large: {name}"
            );
            total = total
                .checked_add(size)
                .context("Archive source size overflow")?;
            ensure!(total <= MAX_EXPANDED, "Archive source exceeds byte budget");
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(id.mode);
            header.set_size(size);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("{root_name}/{name}"),
                BudgetReader {
                    reader: file,
                    remaining: size,
                    deadline,
                    runner: None,
                },
            )?;
        }
        tar.into_inner()?.finish()?;
        ensure!(
            file_map(&source)? == before,
            "Archive source changed during packing"
        );
        ensure!(
            output.metadata()?.len() <= MAX_COMPRESSED,
            "Compressed archive exceeds byte budget"
        );
        output.set_permissions(fs::Permissions::from_mode(0o644))?;
        output.flush()?;
        output.sync_all()?;
        sync_directory(target.parent().context("Archive output parent")?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&target);
    }
    result
}
