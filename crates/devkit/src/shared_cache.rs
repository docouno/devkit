//! Same-mount dependency stores and the minimal host path spine exposed during
//! installation. Daily quality gates never mount this external store.
use crate::{Result, files::*};
use anyhow::{Context, ensure};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
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

fn common_ancestor(a: &Path, b: &Path) -> Result<PathBuf> {
    let path: PathBuf = a
        .components()
        .zip(b.components())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    ensure!(
        path != Path::new("/") && !path.as_os_str().is_empty(),
        "An isolated dependency mount cannot expose the filesystem root"
    );
    Ok(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

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
        let temp = tempfile::TempDir::new().unwrap();
        let checkout = temp.path().join("checkout");
        let repository = temp.path().join("repo");
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
}

#[derive(Clone, Debug)]
pub struct UnifiedStorePlan {
    pub checkout: PathBuf,
    pub store: PathBuf,
    pub ancestor: PathBuf,
    pub checkout_container: PathBuf,
    pub store_container: PathBuf,
    masks: Vec<(PathBuf, bool)>,
}
impl UnifiedStorePlan {
    fn candidate(checkout: &Path, store: &Path) -> Result<Self> {
        let checkout = project_root(checkout)?;
        let store = project_root(store)?;
        let ancestor = common_ancestor(&checkout, &store)?;
        let expected = mount_identity(&checkout)?;
        let mut spine = BTreeSet::new();
        for endpoint in [&checkout, &store] {
            let mut current = nearest_existing(endpoint)?;
            loop {
                ensure!(
                    mount_identity(&current)? == expected,
                    "Dependency store crosses a filesystem mount"
                );
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
                ensure!(
                    masks.len() <= 256,
                    "Dependency path spine exceeds sibling mask budget"
                );
            }
        }
        directory(&store)?;
        Ok(Self {
            checkout_container: Path::new("/devkit/unified")
                .join(checkout.strip_prefix(&ancestor)?),
            store_container: Path::new("/devkit/unified").join(store.strip_prefix(&ancestor)?),
            checkout,
            store,
            ancestor,
            masks,
        })
    }
    pub fn resolve(checkout: &Path, cache: &Path) -> Result<Self> {
        if let Ok(plan) = Self::candidate(checkout, &cache.join("pnpm-store")) {
            return Ok(plan);
        }
        if let Some(git) = git_common_dir(checkout)?
            && let Ok(plan) = Self::candidate(checkout, &git.join("devkit-store"))
        {
            eprintln!(
                "devkit: note: shared cache cannot share an isolated mount with checkout; using repository-shared pnpm store at {}",
                plan.store.display()
            );
            return Ok(plan);
        }
        let plan = Self::candidate(checkout, &checkout.join(".devkit/cache/pnpm-store"))?;
        eprintln!(
            "devkit: warning: falling back to checkout-local pnpm store at .devkit/cache/pnpm-store"
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
        for (target, is_directory) in &self.masks {
            mount(
                command,
                if *is_directory {
                    empty
                } else {
                    Path::new("/dev/null")
                },
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
                || (path == self.checkout && secret_name(&entry.file_name().to_string_lossy()))
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
    pub fn mask_checkout_files(&self, command: &mut Command, target_root: &Path) -> Result<()> {
        for entry in sorted_entries(&self.checkout)? {
            if secret_name(&entry.file_name().to_string_lossy())
                && fs::symlink_metadata(entry.path())?.is_file()
            {
                mount(
                    command,
                    Path::new("/dev/null"),
                    &target_root.join(entry.file_name()),
                    true,
                )?;
            }
        }
        Ok(())
    }
}

fn secret_name(name: &str) -> bool {
    name == ".npmrc" || name == ".env" || (name.starts_with(".env.") && name != ".env.example")
}
pub fn mount(command: &mut Command, source: &Path, target: &Path, readonly: bool) -> Result<()> {
    command
        .arg("--mount")
        .arg(crate::runtime::bind_mount(source, target, readonly)?);
    Ok(())
}
