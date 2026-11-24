//! Obtain the spelling used by APFS without following user links or opening a FIFO.
use super::*;
use std::{
    ffi::{CStr, CString, OsStr},
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
};

pub(super) fn system_alias(path: PathBuf) -> Result<PathBuf> {
    for name in ["tmp", "var", "etc"] {
        let alias = Path::new("/").join(name);
        if let Ok(rest) = path.strip_prefix(&alias) {
            let expected = Path::new("private").join(name);
            match fs::read_link(&alias) {
                Ok(link) if link == expected || link == Path::new("/").join(&expected) => {
                    return Ok(Path::new("/").join(expected).join(rest));
                }
                Ok(_) => bail!("Unexpected system alias: {}", alias.display()),
                Err(error) if error.kind() == io::ErrorKind::InvalidInput || absent(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(path)
}

pub(super) fn spelling(path: &Path) -> Result<PathBuf> {
    let mut directory = path;
    let mut leaf = None;
    loop {
        match fs::symlink_metadata(directory) {
            Ok(meta) if meta.is_dir() => break,
            Ok(meta) if directory == path => leaf = Some(meta),
            Ok(_) => bail!("Non-directory path ancestor: {}", directory.display()),
            Err(error) if absent(&error) => {}
            Err(error) => return Err(error.into()),
        }
        directory = directory.parent().context("No existing parent directory")?;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_EVTONLY)
        .open(directory)?;
    let mut bytes = [0; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH takes a PATH_MAX-byte buffer and a live directory descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    let mut canonical = PathBuf::from(OsStr::from_bytes(
        CStr::from_bytes_until_nul(&bytes)?.to_bytes(),
    ));
    let rest = path.strip_prefix(directory)?;
    if rest.as_os_str().is_empty() {
        return Ok(canonical);
    }
    if let Some(meta) = leaf && meta.nlink() == 1
        && let Some(name) = entry_name(path)?
        && let Some(original) = path.file_name()
        && Path::new(&name).file_name() == Some(name.as_os_str())
        // Non-ASCII APFS spellings can also differ in normalization. For a
        // single link, the inode check below uniquely identifies the entry.
        && (!original.as_bytes().is_ascii() || name.as_bytes().eq_ignore_ascii_case(original.as_bytes()))
    {
        let candidate = canonical.join(name);
        let now = fs::symlink_metadata(&candidate)?;
        ensure!(
            now.dev() == meta.dev() && now.ino() == meta.ino(),
            "File changed while resolving {}",
            path.display()
        );
        return Ok(candidate);
    }
    canonical.push(rest);
    Ok(canonical)
}

fn entry_name(path: &Path) -> Result<Option<std::ffi::OsString>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: attrlist is a plain C structure with zero meaning no attributes selected.
    let mut attrs: libc::attrlist = unsafe { std::mem::zeroed() };
    attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    attrs.commonattr = libc::ATTR_CMN_NAME;
    // getattrlist returns u32 byte count, then attrreference (i32 relative offset,
    // u32 length), then the NUL-terminated directory entry name.
    let mut bytes = [0u8; 12 + libc::PATH_MAX as usize];
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut attrs).cast(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOSYS)) {
            return Ok(None);
        }
        return Err(error.into());
    }
    let used = u32::from_ne_bytes(bytes[..4].try_into()?) as usize;
    let offset = i32::from_ne_bytes(bytes[4..8].try_into()?);
    let length = u32::from_ne_bytes(bytes[8..12].try_into()?) as usize;
    ensure!(
        offset >= 8 && (12..=bytes.len()).contains(&used),
        "Invalid filesystem name attributes"
    );
    let start = 4 + offset as usize;
    let value = bytes[..used]
        .get(start..start + length)
        .context("Invalid filesystem name length")?;
    Ok(Some(
        OsStr::from_bytes(CStr::from_bytes_with_nul(value)?.to_bytes()).to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn system_aliases_existing_spelling_and_missing_tail() {
        let t = tempfile::tempdir_in("/tmp").unwrap();
        let dir = t.path().join("RealDir");
        fs::create_dir(&dir).unwrap();
        let canon = fs::canonicalize(&dir).unwrap();
        assert_eq!(
            project_root(&dir.join("Missing/Leaf")).unwrap(),
            canon.join("Missing/Leaf")
        );
        let alternate = t.path().join("realdir");
        if alternate.is_dir() {
            fs::write(dir.join("MyFile"), b"one link").unwrap();
            assert_eq!(
                project_root(&alternate.join("myfile")).unwrap(),
                canon.join("MyFile")
            );
        }
        symlink(&dir, t.path().join("alias")).unwrap();
        assert!(project_root(&t.path().join("alias/new")).is_err());
    }

    #[test]
    fn search_only_directories_fifos_and_hardlink_names() {
        let t = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(t.path()).unwrap();
        let dir = root.join("restricted");
        fs::create_dir(&dir).unwrap();
        let fifo = dir.join("pipe");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        fs::write(dir.join("first"), b"bytes").unwrap();
        fs::hard_link(dir.join("first"), dir.join("second")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o111)).unwrap();
        let results = [
            project_root(&dir),
            project_root(&fifo),
            project_root(&dir.join("second")),
        ];
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        for (result, expected) in results
            .into_iter()
            .zip([dir.clone(), fifo, dir.join("second")])
        {
            assert_eq!(result.unwrap(), expected);
        }
    }
}
