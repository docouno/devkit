//! Same-mount dependency stores and the minimal host path spine exposed during
//! installation. Daily quality gates never mount this external store.
use crate::{Result, files::*};
use anyhow::{Context, ensure};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

pub fn directory(path: &Path) -> Result<()> {
    let path = project_root(path)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_dir(),
            "Expected directory, refusing symlink runtime path: {}",
            path.display()
        );
    }
    create_dir_all_durable(&path)
}

const MASKED_CACHE_DIRS: [&str; 8] = [
    ".tmp",
    "empty-mask",
    ".locks",
    "engines",
    "tools",
    "releases",
    "refs",
    "cargo-receipts",
];
const MASKED_CACHE_FILES: [&str; 3] = ["empty-file-mask", "port-leases.json", "port-leases.lock"];

/// Create private bind sources before enumerating the siblings on a shared mount.
pub fn prepare_mount_masks(cache: &Path) -> Result<()> {
    crate::cache::ensure_cache_root(cache)?;
    let tmp = safe_path(cache, ".tmp")?;
    directory(&tmp)?;
    if fs::symlink_metadata(&tmp)?.mode() & 0o7777 != 0o700 {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700))?;
    }
    for name in MASKED_CACHE_DIRS {
        if name != ".tmp" {
            directory(&safe_path(cache, name)?)?;
        }
    }
    ensure!(
        fs::read_dir(cache.join("empty-mask"))?.next().is_none(),
        "Shared empty-mask directory must be empty"
    );
    let path = safe_path(cache, "empty-file-mask")?;
    if !exists(&path)? {
        let file = tempfile::NamedTempFile::new_in(&tmp)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o444))?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&path) {
            Ok(_) => sync_directory(cache)?,
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(err) => return Err(err.error.into()),
        }
    }
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.len() == 0 && metadata.mode() & 0o7777 == 0o444,
        "Invalid shared empty-file-mask: {}",
        path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn volume_is_case_sensitive(path: &Path) -> Result<bool> {
    unsafe extern "C" {
        fn devkit_volume_case_sensitive(path: *const libc::c_char) -> libc::c_int;
    }
    let existing = nearest_existing(path)?;
    let encoded = CString::new(existing.as_os_str().as_bytes())?;
    // SAFETY: encoded is a live NUL-terminated pathname.
    Ok(unsafe { devkit_volume_case_sensitive(encoded.as_ptr()) } == 1)
}

#[cfg(not(target_os = "macos"))]
fn volume_is_case_sensitive(_path: &Path) -> Result<bool> {
    Ok(true)
}

#[cfg(target_os = "macos")]
pub fn mount_identity(path: &Path) -> Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut attrs: libc::attrlist = unsafe { std::mem::zeroed() };
    attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    attrs.commonattr = libc::ATTR_CMN_DEVID | libc::ATTR_CMN_FSID;
    // getattrlist(2) requires a 4-byte aligned buffer: u32 size, dev_t, fsid_t.
    // It needs only metadata access and never opens a FIFO or follows a link.
    #[repr(C, align(4))]
    struct MountBuffer([u8; 16]);
    let mut buffer = MountBuffer([0u8; 16]);
    let bytes = &mut buffer.0;
    if unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut attrs).cast(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::FSOPT_NOFOLLOW,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        u32::from_ne_bytes(bytes[..4].try_into()?) as usize == bytes.len(),
        "Incomplete filesystem identity"
    );
    let device = u32::from_ne_bytes(bytes[4..8].try_into()?);
    let fsid = u64::from_ne_bytes(bytes[8..].try_into()?);
    Ok((fsid, u64::from(device)))
}

#[cfg(target_os = "linux")]
pub fn mount_identity(path: &Path) -> Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut info = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: statx writes one live statx structure; CString outlives the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID | libc::STATX_BASIC_STATS,
            info.as_mut_ptr(),
        )
    };
    ensure!(
        result == 0,
        "Cannot inspect filesystem mount: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a successful statx initialized the structure.
    let info = unsafe { info.assume_init() };
    Ok((
        if info.stx_mask & libc::STATX_MNT_ID != 0 {
            info.stx_mnt_id
        } else {
            0
        },
        ((info.stx_dev_major as u64) << 32) | info.stx_dev_minor as u64,
    ))
}

fn nearest_existing(path: &Path) -> Result<PathBuf> {
    let mut candidate = path.to_owned();
    while !candidate.exists() {
        candidate = candidate
            .parent()
            .context("No existing cache ancestor")?
            .to_owned();
    }
    Ok(candidate)
}

fn common_ancestor(a: &Path, b: &Path) -> Option<PathBuf> {
    let path: PathBuf = a
        .components()
        .zip(b.components())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    (path.parent().is_some() && !path.as_os_str().is_empty()).then_some(path)
}

fn git_common_dir(checkout: &Path) -> Result<Option<PathBuf>> {
    let git = input_path(checkout, ".git")?;
    let Ok(metadata) = fs::symlink_metadata(&git) else {
        return Ok(None);
    };
    if metadata.is_dir() {
        return Ok(Some(git));
    }
    let (bytes, _) = read_regular_file(&git, 16 * 1024, "Git worktree pointer")?
        .context("Missing Git worktree pointer")?;
    let value = std::str::from_utf8(&bytes).context("Invalid Git worktree pointer")?;
    let path = value
        .trim()
        .strip_prefix("gitdir: ")
        .context("Invalid Git worktree pointer")?;
    let directory = project_root(&checkout.join(path))?;
    let common = directory.join("commondir");
    if let Some((bytes, _)) = read_regular_file(&common, 16 * 1024, "Git common directory pointer")?
    {
        let value = std::str::from_utf8(&bytes).context("Invalid Git common directory pointer")?;
        Ok(Some(project_root(&directory.join(value.trim()))?))
    } else {
        Ok(Some(directory))
    }
}

#[derive(Clone, Debug)]
pub struct UnifiedStorePlan {
    pub checkout: PathBuf,
    pub store: PathBuf,
    pub ancestor: PathBuf,
    pub checkout_container: PathBuf,
    pub store_container: PathBuf,
    masks: Vec<(PathBuf, bool)>,
    cache: PathBuf,
}
impl UnifiedStorePlan {
    fn candidate(checkout: &Path, store: &Path, cache: &Path) -> Result<Option<Self>> {
        let checkout = project_root(checkout)?;
        let case_sensitive = volume_is_case_sensitive(&checkout)?;
        Self::candidate_with_case_sensitivity(&checkout, store, cache, case_sensitive)
    }

    fn candidate_with_case_sensitivity(
        checkout: &Path,
        store: &Path,
        cache: &Path,
        case_sensitive: bool,
    ) -> Result<Option<Self>> {
        let checkout = project_root(checkout)?;
        let store = project_root(store)?;
        let Some(ancestor) = common_ancestor(&checkout, &store) else {
            return Ok(None);
        };
        ensure!(
            !checkout.starts_with(&store),
            "Dependency store cannot contain the checkout"
        );
        // On case-insensitive host shares (default macOS APFS + VirtioFS),
        // Linux VFS bind masks only match exact casing, so never mount an
        // ancestor above the checkout root (such as $HOME).
        if !case_sensitive && ancestor != checkout {
            return Ok(None);
        }
        let expected = mount_identity(&checkout)?;
        let mut spine = BTreeSet::new();
        for endpoint in [&checkout, &store] {
            let mut current = nearest_existing(endpoint)?;
            loop {
                if mount_identity(&current)? != expected {
                    return Ok(None);
                }
                if current != checkout && !current.starts_with(&checkout) && current != store {
                    spine.insert(current.clone());
                }
                if current == ancestor {
                    break;
                }
                current = current
                    .parent()
                    .context("Invalid dependency path spine")?
                    .to_owned();
            }
        }
        let mut masks = Vec::new();
        for parent in spine {
            for entry in sorted_entries(&parent)? {
                let path = entry.path();
                if checkout.starts_with(&path) || store.starts_with(&path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() {
                    continue;
                }
                masks.push((
                    Path::new("/devkit/unified").join(path.strip_prefix(&ancestor)?),
                    metadata.is_dir(),
                ));
                if masks.len() > 256 {
                    return Ok(None);
                }
            }
        }
        directory(&store)?;
        Ok(Some(Self {
            cache: cache.to_owned(),
            checkout_container: Path::new("/devkit/unified")
                .join(checkout.strip_prefix(&ancestor)?),
            store_container: Path::new("/devkit/unified").join(store.strip_prefix(&ancestor)?),
            checkout,
            store,
            ancestor,
            masks,
        }))
    }
    pub fn resolve_with_probe(
        checkout: &Path,
        cache: &Path,
        mut probe: impl FnMut(&Self) -> Result<bool>,
    ) -> Result<Self> {
        let cache = project_root(cache)?;
        prepare_mount_masks(&cache)?;
        let mut candidate = |store: PathBuf| -> Result<Option<Self>> {
            if let Some(plan) = Self::candidate(checkout, &store, &cache)?
                && probe(&plan)?
            {
                return Ok(Some(plan));
            }
            Ok(None)
        };
        if let Some(plan) = candidate(cache.join("pnpm-store"))? {
            return Ok(plan);
        }
        if let Some(git) = git_common_dir(checkout)?
            && let Some(plan) = candidate(git.join("devkit-store"))?
        {
            eprintln!(
                "devkit: note: using repository-shared pnpm store at {}",
                plan.store.display()
            );
            return Ok(plan);
        }
        let store = checkout.join(".devkit/cache/pnpm-store");
        let plan = candidate(store.clone())?.with_context(|| format!(
            "Cannot create hardlinks between {} and checkout node_modules on an isolated bind mount; check Docker file sharing", store.display()
        ))?;
        eprintln!(
            "devkit: warning: falling back to checkout-local pnpm store at {}",
            store.display()
        );
        Ok(plan)
    }
    pub fn mount(
        &self,
        command: &mut Command,
        empty: &Path,
        setup: bool,
        writable: &[PathBuf],
    ) -> Result<()> {
        mount(command, &self.ancestor, Path::new("/devkit/unified"), false)?;
        let file_mask = self.cache.join("empty-file-mask");
        for (target, is_directory) in &self.masks {
            mount(
                command,
                if *is_directory { empty } else { &file_mask },
                target,
                true,
            )?;
        }
        if setup {
            let mut protected = writable.to_vec();
            protected.push(self.store.clone());
            self.readonly_sources(command, &self.checkout, &protected)?;
        }
        self.mask_checkout_files(command, &self.checkout_container)
    }
    fn readonly_sources(
        &self,
        command: &mut Command,
        path: &Path,
        writable: &[PathBuf],
    ) -> Result<()> {
        for entry in sorted_entries(path)? {
            let source = entry.path();
            let metadata = fs::symlink_metadata(&source)?;
            if metadata.file_type().is_symlink()
                || self.cache_mask(&source)
                || (path == self.checkout
                    && secret_name(&canonical_secret_name(
                        &entry.file_name().to_string_lossy(),
                        cfg!(target_os = "macos"),
                    )))
            {
                continue;
            }
            if writable.contains(&source) {
                continue;
            }
            if writable.iter().any(|p| p.starts_with(&source)) {
                ensure!(
                    metadata.is_dir(),
                    "Writable install path has a non-directory ancestor"
                );
                self.readonly_sources(command, &source, writable)?;
            } else {
                let target = self
                    .checkout_container
                    .join(source.strip_prefix(&self.checkout)?);
                mount(command, &source, &target, true)?;
            }
        }
        Ok(())
    }
    fn cache_mask(&self, path: &Path) -> bool {
        path.strip_prefix(&self.cache)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|p| MASKED_CACHE_DIRS.contains(&p) || MASKED_CACHE_FILES.contains(&p))
    }
    pub fn mask_checkout_files(&self, command: &mut Command, target_root: &Path) -> Result<()> {
        mask_checkout_secrets(command, &self.checkout, &self.cache, target_root)
    }
}

pub(crate) fn mask_embedded_cache(
    command: &mut Command,
    checkout: &Path,
    cache: &Path,
    target_root: &Path,
) -> Result<()> {
    if let Ok(relative) = cache.strip_prefix(checkout) {
        let dir_mask = cache.join("empty-mask");
        let file_mask = cache.join("empty-file-mask");
        for name in MASKED_CACHE_DIRS {
            mount(
                command,
                &dir_mask,
                &target_root.join(relative).join(name),
                true,
            )?;
        }
        for name in MASKED_CACHE_FILES {
            if exists(&cache.join(name))? {
                mount(
                    command,
                    &file_mask,
                    &target_root.join(relative).join(name),
                    true,
                )?;
            }
        }
    }
    Ok(())
}

pub fn mask_checkout_secrets(
    command: &mut Command,
    checkout: &Path,
    cache: &Path,
    target_root: &Path,
) -> Result<()> {
    let file_mask = cache.join("empty-file-mask");
    for target in mask_targets(checkout, true, cfg!(target_os = "macos"))? {
        mount(command, &file_mask, &target_root.join(target), true)?;
    }
    mask_embedded_cache(command, checkout, cache, target_root)
}

fn canonical_secret_name(name: &str, fold_case: bool) -> std::borrow::Cow<'_, str> {
    if fold_case {
        name.to_ascii_lowercase().into()
    } else {
        name.into()
    }
}
fn secret_name(name: &str) -> bool {
    name == ".npmrc" || name == ".env" || (name.starts_with(".env.") && name != ".env.example")
}

fn mask_targets(root: &Path, all_secrets: bool, fold_case: bool) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for entry in sorted_entries(root)? {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        let canonical = canonical_secret_name(&text, fold_case);
        if (canonical == ".npmrc" || all_secrets && secret_name(&canonical))
            && entry.file_type()?.is_file()
        {
            paths.insert(PathBuf::from(&name));
            if canonical != text {
                // On case-sensitive shares, binding a missing alias would create it on the host.
                match fs::symlink_metadata(root.join(canonical.as_ref())) {
                    Ok(meta) if meta.is_file() => {
                        paths.insert(PathBuf::from(canonical.as_ref()));
                    }
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                        return Err(err.into());
                    }
                    _ => (),
                }
            }
        }
    }
    Ok(paths.into_iter().collect())
}

pub fn npmrc_targets(root: &Path) -> Result<Vec<PathBuf>> {
    mask_targets(root, false, cfg!(target_os = "macos"))
}

pub fn mount(command: &mut Command, source: &Path, target: &Path, readonly: bool) -> Result<()> {
    command
        .arg("--mount")
        .arg(crate::runtime::bind_mount(source, target, readonly)?);
    Ok(())
}

/// Run in the Linux helper. Only a filesystem limitation permits trying another store.
pub fn check_guest_hardlink(source: &Path, link: &Path, token: &str) -> Result<()> {
    let read_token = |p: &Path| -> Result<()> {
        let (bytes, _) =
            read_regular_file(p, 64, "Hardlink probe")?.context("Missing hardlink probe")?;
        ensure!(bytes == token.as_bytes(), "Hardlink probe contents changed");
        Ok(())
    };
    read_token(source)?;
    match fs::hard_link(source, link) {
        Err(err)
            if matches!(
                err.raw_os_error(),
                Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENOSYS | libc::EPERM)
            ) =>
        {
            println!("unsupported");
        }
        Err(err) => return Err(err.into()),
        Ok(()) => {
            let (a, b) = (fs::symlink_metadata(source)?, fs::symlink_metadata(link)?);
            ensure!(
                a.dev() == b.dev() && a.ino() == b.ino() && b.nlink() >= 2,
                "Docker mount did not preserve hardlink identity"
            );
            read_token(link)?;
            println!("supported");
        }
    }
    Ok(())
}

fn contains_exact_path(text: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(rel) = text[start..].find(path) {
        let idx = start + rel;
        let before = text[..idx].chars().next_back();
        let after = text[idx + path.len()..].chars().next();
        let left_ok = before.is_none_or(|c| c.is_whitespace() || matches!(c, ':' | '"' | '\''));
        let right_ok =
            after.is_none_or(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '.' | ',' | ':'));
        if left_ok && right_ok {
            return true;
        }
        start = idx + text[idx..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn guest_hardlinks_supported(
    output: crate::processes::ProcessOutput,
    ancestor: &Path,
) -> Result<bool> {
    if output.code == 125 && output.stdout.is_empty() {
        let stderr = std::str::from_utf8(&output.stderr)?;
        let ancestor_text = ancestor.to_str().context("Non-UTF8 dependency mount")?;
        let lower = stderr.to_ascii_lowercase();
        let daemon_mount_denial = lower.contains("error response from daemon:")
            && contains_exact_path(stderr, ancestor_text)
            && [
                "bind source path does not exist",
                "mounts denied",
                "is not shared from the host",
                "path is not shared",
            ]
            .iter()
            .any(|phrase| lower.contains(phrase));
        if daemon_mount_denial {
            // The candidate must still be an ordinary directory on the host.
            ensure!(
                project_root(ancestor)? == ancestor && fs::symlink_metadata(ancestor)?.is_dir(),
                "Dependency mount ancestor changed during the Docker probe"
            );
            return Ok(false);
        }
    }
    match output.success("Docker hardlink probe")?.text()?.trim() {
        "supported" => Ok(true),
        "unsupported" => Ok(false),
        _ => anyhow::bail!("Unexpected Docker hardlink probe result"),
    }
}

pub fn probe_guest_hardlinks(
    plan: &UnifiedStorePlan,
    empty: &Path,
    mut docker: Command,
    helper: &Path,
    node_image: &str,
    platform: &str,
    runner: &crate::processes::ProcessRunner,
) -> Result<bool> {
    use crate::processes::{OutputMode, OwnedContainer};
    use std::io::Write;
    let modules = input_path(&plan.checkout, "node_modules")?;
    directory(&modules)?;
    let token = uuid::Uuid::new_v4().to_string();
    let owned = OwnedContainer::new(format!("devkit-hardlink-{token}"), token.clone());
    let mut source = tempfile::Builder::new()
        .prefix(".devkit-hardlink-")
        .tempfile_in(&plan.store)?;
    source.write_all(token.as_bytes())?;
    source.flush()?;
    source
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    let destination = tempfile::Builder::new()
        .prefix(".devkit-hardlink-")
        .tempdir_in(&modules)?;
    fs::set_permissions(destination.path(), fs::Permissions::from_mode(0o755))?;
    let link = destination.path().join("link");
    docker.env_remove("DOCKER_DEFAULT_PLATFORM").args([
        "run",
        "--pull",
        "never",
        "--platform",
        platform,
        "--rm",
        "--init",
        "--name",
        &owned.name,
        "--label",
        &format!("{}={token}", owned.label),
        "--user",
        &format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
            libc::getegid()
        }),
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
    ]);
    plan.mount(&mut docker, empty, true, &[modules])?;
    mount(&mut docker, helper, Path::new("/devkit/devkit"), true)?;
    docker
        .arg(node_image)
        .args(["/devkit/devkit", "internal", "probe-hardlinks"])
        .arg(
            plan.store_container
                .join(source.path().strip_prefix(&plan.store)?),
        )
        .arg(
            plan.checkout_container
                .join(link.strip_prefix(&plan.checkout)?),
        )
        .arg(&token);
    let output = runner.run_owned_unchecked(
        &mut docker,
        OutputMode::Capture(8192),
        &owned,
        Some(std::time::Duration::from_secs(30)),
    )?;
    runner.check_cancelled()?;
    let supported = guest_hardlinks_supported(output, &plan.ancestor)?;
    if supported {
        let (bytes, _) = read_regular_file(&link, 64, "Docker hardlink probe")?
            .context("Docker hardlink missing on host")?;
        ensure!(
            bytes == token.as_bytes(),
            "Docker hardlink contents differ on host"
        );
    }
    destination.close()?;
    source.close()?;
    Ok(supported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn cold_mount_masks_exist_before_selection_and_fallback_stops_on_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = project_root(&tmp.path().join("checkout")).unwrap();
        let cache = project_root(&tmp.path().join("cache")).unwrap();
        directory(&checkout.join(".git")).unwrap();
        let mut attempts = Vec::new();
        let plan = UnifiedStorePlan::resolve_with_probe(&checkout, &cache, |p| {
            if attempts.is_empty() {
                for name in MASKED_CACHE_DIRS
                    .iter()
                    .chain(std::iter::once(&"empty-file-mask"))
                {
                    assert!(p.masks.iter().any(
                        |(target, _)| target == &Path::new("/devkit/unified/cache").join(name)
                    ));
                }
            }
            attempts.push(p.store.clone());
            Ok(p.store == checkout.join(".devkit/cache/pnpm-store"))
        })
        .unwrap();
        assert_eq!(fs::metadata(&cache).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(cache.join(".tmp")).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            attempts,
            [
                cache.join("pnpm-store"),
                checkout.join(".git/devkit-store"),
                plan.store
            ]
        );
        let mut count = 0;
        assert!(
            UnifiedStorePlan::resolve_with_probe(&checkout, &cache, |_| {
                count += 1;
                anyhow::bail!("guest probe failed")
            })
            .is_err()
        );
        assert_eq!(count, 1);
        assert!(
            UnifiedStorePlan::resolve_with_probe(&checkout, &cache, |_| Ok(false))
                .unwrap_err()
                .to_string()
                .contains("Cannot create hardlinks")
        );
        let mask = cache.join("empty-file-mask");
        assert_eq!(fs::metadata(&mask).unwrap().mode() & 0o777, 0o444);
        fs::remove_file(&mask).unwrap();
        symlink(checkout.join("missing"), &mask).unwrap();
        assert!(prepare_mount_masks(&cache).is_err());
    }

    #[test]
    fn case_insensitive_volumes_forbid_ancestors_outside_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = project_root(&tmp.path().join("dev/app")).unwrap();
        let cache = project_root(&tmp.path().join("cache")).unwrap();
        directory(&checkout).unwrap();
        prepare_mount_masks(&cache).unwrap();
        assert!(
            UnifiedStorePlan::candidate_with_case_sensitivity(
                &checkout,
                &cache.join("pnpm-store"),
                &cache,
                false,
            )
            .unwrap()
            .is_none()
        );
        let local = UnifiedStorePlan::candidate_with_case_sensitivity(
            &checkout,
            &checkout.join(".devkit/cache/pnpm-store"),
            &cache,
            false,
        )
        .unwrap()
        .expect("checkout-local store remains allowed");
        assert_eq!(local.ancestor, checkout);
    }

    #[test]
    fn only_the_unshared_candidate_ancestor_allows_mount_fallback() {
        use crate::processes::ProcessOutput;
        let tmp = tempfile::tempdir().unwrap();
        let root = project_root(tmp.path()).unwrap();
        let checkout = root.join("checkout");
        let cache = root.join("cache");
        directory(&checkout.join(".git")).unwrap();
        let diagnostic = |code, stderr: String| ProcessOutput {
            code,
            stdout: Vec::new(),
            stderr: stderr.into_bytes(),
        };
        let missing = |p: &Path| {
            format!(
                "docker: Error response from daemon: invalid mount config for type \"bind\": bind source path does not exist: {}\n\nRun 'docker run --help' for more information\n",
                p.display()
            )
        };
        let denied = format!(
            "docker: Error response from daemon: Mounts denied:\nThe path {} is not shared from the host and is not known to Docker.\n",
            root.display()
        );
        let lima_denied = format!(
            "docker: Error response from daemon: mounts denied: path {} is not shared\n",
            root.display()
        );
        for message in [missing(&root), denied, lima_denied] {
            let plan = UnifiedStorePlan::resolve_with_probe(&checkout, &cache, |plan| {
                if plan.store == cache.join("pnpm-store") {
                    guest_hardlinks_supported(diagnostic(125, message.clone()), &plan.ancestor)
                } else {
                    Ok(true)
                }
            })
            .unwrap();
            assert_eq!(plan.store, checkout.join(".git/devkit-store"));
        }
        for output in [
            diagnostic(125, missing(&root.join("other"))),
            diagnostic(1, missing(&root)),
            diagnostic(125, "Docker connection unavailable".into()),
            diagnostic(1, "Hardlink probe contents changed".into()),
        ] {
            assert!(guest_hardlinks_supported(output, &root).is_err());
        }
        let absent = root.join("absent");
        assert!(guest_hardlinks_supported(diagnostic(125, missing(&absent)), &absent).is_err());
    }

    #[test]
    fn npmrc_mask_preserves_environment_for_daily_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for file in [".NPMRC", ".Env.Local", ".env.example"] {
            fs::write(root.join(file), []).unwrap();
        }
        let alias_exists = root.join(".npmrc").exists();
        let mut npmrc = vec![PathBuf::from(".NPMRC")];
        let mut secrets = vec![PathBuf::from(".Env.Local"), PathBuf::from(".NPMRC")];
        if alias_exists {
            npmrc.push(".npmrc".into());
            secrets.extend([PathBuf::from(".env.local"), PathBuf::from(".npmrc")]);
        }
        assert_eq!(mask_targets(root, false, true).unwrap(), npmrc);
        assert_eq!(mask_targets(root, true, true).unwrap(), secrets);
        assert!(mask_targets(root, false, false).unwrap().is_empty());
        assert!(mask_targets(root, true, false).unwrap().is_empty());
        assert_eq!(root.join(".npmrc").exists(), alias_exists);
        if !alias_exists {
            fs::write(root.join(".npmrc"), []).unwrap();
            assert_eq!(mask_targets(root, false, true).unwrap().len(), 2);
        }
    }

    #[test]
    fn git_pointer_metadata_is_bounded_before_any_identity() {
        const WORKER: &str = "DEVKIT_TEST_GIT_POINTER_BUDGET";
        if std::env::var(WORKER).as_deref() != Ok("1") {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shared_cache::tests::git_pointer_metadata_is_bounded_before_any_identity",
                    "--test-threads=1",
                ])
                .env(WORKER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let pages: u64 = fs::read_to_string("/proc/self/statm")
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            // SAFETY: this disposable worker limits only its own address space.
            let maximum =
                pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64 + 64 * 1024 * 1024;
            let limit = libc::rlimit {
                rlim_cur: maximum,
                rlim_max: maximum,
            };
            // SAFETY: limit is a live rlimit and no other process is modified.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) }, 0);
        }
        let temp = tempfile::TempDir::new().unwrap();
        let checkout = temp.path().join("checkout");
        let repository = project_root(&temp.path().join("repo")).unwrap();
        let gitdir = repository.join("worktrees/check");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir_all(&gitdir).unwrap();
        let pointer = checkout.join(".git");
        let common = gitdir.join("commondir");
        fs::write(&pointer, "gitdir: ../repo/worktrees/check\n").unwrap();
        fs::write(&common, "../..\n").unwrap();
        assert_eq!(git_common_dir(&checkout).unwrap(), Some(repository.clone()));
        for path in [&pointer, &common] {
            let original = fs::read(path).unwrap();
            for size in [16 * 1024 + 1, 1024 * 1024 * 1024] {
                fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_len(size)
                    .unwrap();
                assert!(
                    git_common_dir(&checkout)
                        .unwrap_err()
                        .to_string()
                        .contains("exceeds byte budget")
                );
                fs::write(path, &original).unwrap();
            }
        }
        fs::remove_file(&common).unwrap();
        symlink(&pointer, &common).unwrap();
        assert!(git_common_dir(&checkout).is_err());
        fs::remove_file(&common).unwrap();
        let name = CString::new(common.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid terminated pathname, in the disposable fixture only.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(git_common_dir(&checkout).is_err());
        fs::remove_file(&common).unwrap();
        fs::write(&common, "../..\n").unwrap();
        assert_eq!(git_common_dir(&checkout).unwrap(), Some(repository));
    }

    #[test]
    fn contains_exact_path_preserves_left_boundary_across_repeated_segments() {
        assert!(contains_exact_path(
            "bind source path does not exist: /work",
            "/work"
        ));
        assert!(contains_exact_path("path \"/work\" is not shared", "/work"));
        assert!(!contains_exact_path(
            "bind source path does not exist: /work/work",
            "/work"
        ));
        assert!(!contains_exact_path(
            "bind source path does not exist: /work/sub",
            "/work"
        ));
        assert!(!contains_exact_path("foo/a/a bar", "/a"));
        assert!(!contains_exact_path("anything", ""));
    }
}
