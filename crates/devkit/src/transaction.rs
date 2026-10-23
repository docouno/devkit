use crate::{Result, catalog::valid_component_path, files::*, model::*, state::*};
use anyhow::{Context, ensure};
use std::{collections::BTreeSet, fs, path::Path};

pub trait MutationHooks {
    fn after_stage(&self) -> Result<()> {
        Ok(())
    }
    fn after_write(&self, _count: usize, _path: &str) -> Result<()> {
        Ok(())
    }
}
pub fn transaction_path<'a>(name: &'a str, created: &[String]) -> Result<&'a str> {
    relative_path(name)?;
    if name == MANIFEST_PATH
        || ["package.json", "package-lock.json"].contains(&name)
        || {
            let parts = name.split('/').collect::<Vec<_>>();
            parts.len() == 3
                && valid_component_path(&format!("{}/{}", parts[0], parts[1])).is_ok()
                && ["package.json", "package-lock.json"].contains(&parts[2])
        }
        || created.iter().any(|d| name.starts_with(&format!("{d}/")))
    {
        return Ok(name);
    }
    managed_path(name)
}
fn validate_journal(tx: &UpdateJournal) -> Result<()> {
    ensure!(
        [1, 2].contains(&tx.schema_version),
        "Unsupported update journal"
    );
    let suffix = tx
        .stage
        .strip_prefix(".devkit/state/update-")
        .context("Invalid update stage")?;
    ensure!(GENERATION_RE.is_match(suffix), "Invalid update stage");
    let mut dirs = BTreeSet::new();
    for dir in &tx.created_directories {
        valid_component_path(dir)?;
        ensure!(dirs.insert(dir), "Duplicate created directory");
    }
    let mut names = BTreeSet::new();
    for (i, file) in tx.files.iter().enumerate() {
        transaction_path(&file.path, &tx.created_directories)?;
        ensure!(
            names.insert(&file.path),
            "Duplicate journal target: {}",
            file.path
        );
        ensure!(
            file.before != file.after,
            "Unchanged journal target: {}",
            file.path
        );
        if let Some(id) = &file.before {
            validate_identity(id)?;
        }
        if let Some(id) = &file.after {
            validate_identity(id)?;
        }
        ensure!(
            !tx.created_directories
                .iter()
                .any(|d| file.path.starts_with(&format!("{d}/")))
                || file.before.is_none(),
            "New component journal overwrites existing source"
        );
        let backup = file
            .before
            .as_ref()
            .map(|_| format!("{}/backup/{i}", tx.stage));
        let staged = file
            .after
            .as_ref()
            .map(|_| format!("{}/staged/{i}", tx.stage));
        ensure!(
            file.backup == backup && file.staged == staged,
            "Invalid journal saved path: {}",
            file.path
        );
    }
    ensure!(
        !tx.files.is_empty() && tx.files.last().is_some_and(|f| f.path == MANIFEST_PATH),
        "Journal is missing final accepted manifest"
    );
    let mut parents = BTreeSet::new();
    for parent in &tx.created_parents {
        relative_path(parent)?;
        ensure!(
            parents.insert(parent)
                && tx
                    .files
                    .iter()
                    .any(|f| f.path.starts_with(&format!("{parent}/"))),
            "Invalid created parent directory"
        );
    }
    Ok(())
}
pub fn read_journal(root: &Path) -> Result<UpdateJournal> {
    let tx: UpdateJournal = read_json(&safe_path(root, PENDING_PATH)?)?;
    validate_journal(&tx)?;
    Ok(tx)
}
fn verify_saved(root: &Path, file: &PlannedFile) -> Result<()> {
    if let Some(p) = &file.backup {
        ensure!(
            identity(&safe_path(root, p)?)? == file.before,
            "Update backup integrity mismatch: {}",
            file.path
        );
    }
    if let Some(p) = &file.staged {
        ensure!(
            identity(&safe_path(root, p)?)? == file.after,
            "Update staged integrity mismatch: {}",
            file.path
        );
    }
    Ok(())
}
fn expected_new_directories(tx: &UpdateJournal) -> BTreeSet<String> {
    let mut directories = BTreeSet::new();
    for dir in &tx.created_directories {
        directories.insert(dir.clone());
        for file in &tx.files {
            if file.path.starts_with(&format!("{dir}/")) {
                let mut parent = Path::new(&file.path).parent();
                while let Some(p) = parent {
                    let n = p.to_string_lossy().to_string();
                    if n.len() < dir.len() {
                        break;
                    }
                    directories.insert(n);
                    parent = p.parent();
                }
            }
        }
    }
    directories
}
fn verify_new_sources(root: &Path, tx: &UpdateJournal) -> Result<()> {
    fn walk(root: &Path, path: &Path, tx: &UpdateJournal, dirs: &BTreeSet<String>) -> Result<()> {
        for entry in sorted_entries(path)? {
            let p = entry.path();
            let n = relative_name(root, &p)?;
            let m = fs::symlink_metadata(&p)?;
            ensure!(
                !m.file_type().is_symlink(),
                "Recovery conflict: unknown symlink {n}"
            );
            if m.is_dir() {
                ensure!(
                    dirs.contains(&n),
                    "Recovery conflict: unknown directory {n}"
                );
                walk(root, &p, tx, dirs)?;
            } else {
                ensure!(
                    m.is_file() && tx.files.iter().any(|f| f.path == n),
                    "Recovery conflict: unknown file {n}"
                );
            }
        }
        Ok(())
    }
    let dirs = expected_new_directories(tx);
    for dir in &tx.created_directories {
        let p = safe_path(root, dir)?;
        if exists(&p)? {
            ensure!(
                p.is_dir(),
                "Recovery conflict: component is not a directory: {dir}"
            );
            walk(root, &p, tx, &dirs)?;
        }
    }
    Ok(())
}
pub fn recover_project(root: &Path) -> Result<bool> {
    let lock = CheckoutLock::acquire(root)?;
    recover_locked(&lock.root)
}
pub(crate) fn recover_locked(root: &Path) -> Result<bool> {
    let pending = identity(&safe_path(root, PENDING_PATH)?)?;
    if pending.is_none() {
        return Ok(false);
    }
    let tx = read_journal(root)?;
    // No rollback writes until every saved/current identity and every created
    // source path has passed preflight.
    verify_new_sources(root, &tx)?;
    for file in &tx.files {
        verify_saved(root, file)?;
        let actual = identity(&safe_path(root, &file.path)?)?;
        ensure!(
            actual == file.before || actual == file.after,
            "Recovery conflict: {}; restore a known before/after state",
            file.path
        );
    }
    ensure!(
        identity(&safe_path(root, PENDING_PATH)?)? == pending,
        "Update journal changed during recovery"
    );
    for file in tx.files.iter().rev() {
        let p = safe_path(root, &file.path)?;
        let actual = identity(&p)?;
        if actual == file.before {
            continue;
        }
        ensure!(
            actual == file.after,
            "Recovery conflict during rollback: {}",
            file.path
        );
        verify_saved(root, file)?;
        if let Some(backup) = &file.backup {
            write_atomic(
                &p,
                &fs::read(safe_path(root, backup)?)?,
                file.before.as_ref().unwrap().mode,
            )?;
        } else {
            remove_file(&p)?;
        }
    }
    let mut dirs = expected_new_directories(&tx);
    dirs.extend(tx.created_parents.iter().cloned());
    let mut dirs = dirs.into_iter().collect::<Vec<_>>();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.split('/').count()));
    for dir in dirs {
        let p = safe_path(root, &dir)?;
        if exists(&p)? && sorted_entries(&p)?.is_empty() {
            remove_empty_dir_durable(&p)?;
        }
    }
    remove_file(&safe_path(root, PENDING_PATH)?)?;
    remove_tree_durable(&safe_path(root, &tx.stage)?)?;
    Ok(true)
}
/// An unlink that returned an fsync error must not make saved data disposable.
pub(crate) fn cleanup_unpublished_stage(root: &Path, stage: &Path) -> Result<()> {
    let pending = safe_path(root, PENDING_PATH)?;
    ensure!(
        identity(&pending)?.is_none(),
        "Cannot discard a pending recovery stage"
    );
    sync_directory(pending.parent().context("Pending marker parent")?)?;
    remove_tree_durable(stage)
}
fn sync_stage_ancestry(root: &Path, stage: &str) -> Result<()> {
    let mut directory = safe_path(root, stage)?;
    loop {
        sync_directory(&directory)?;
        if directory == root {
            break;
        }
        directory = directory
            .parent()
            .context("Recovery stage parent")?
            .to_path_buf();
        ensure!(
            directory.starts_with(root),
            "Recovery stage escaped checkout"
        );
    }
    Ok(())
}
pub struct PreparedTransaction {
    pub journal: UpdateJournal,
}
impl PreparedTransaction {
    pub fn prepare(
        root: &Path,
        stage: &str,
        staged_root: &Path,
        paths: &BTreeSet<String>,
        created: Vec<String>,
    ) -> Result<Self> {
        let mut paths = paths.iter().cloned().collect::<Vec<_>>();
        paths.sort_by(|a, b| {
            if a == MANIFEST_PATH {
                std::cmp::Ordering::Greater
            } else if b == MANIFEST_PATH {
                std::cmp::Ordering::Less
            } else {
                a.cmp(b)
            }
        });
        let mut files = vec![];
        let mut created_parents = BTreeSet::new();
        for name in paths {
            transaction_path(&name, &created)?;
            let before = identity(&safe_path(root, &name)?)?;
            let after = identity(&safe_path(staged_root, &name)?)?;
            if before == after {
                continue;
            }
            let i = files.len();
            let backup = before.as_ref().map(|_| format!("{stage}/backup/{i}"));
            let staged = after.as_ref().map(|_| format!("{stage}/staged/{i}"));
            if let Some(p) = &backup {
                write_atomic(
                    &safe_path(root, p)?,
                    &fs::read(safe_path(root, &name)?)?,
                    before.as_ref().unwrap().mode,
                )?;
            }
            if let Some(p) = &staged {
                write_atomic(
                    &safe_path(root, p)?,
                    &fs::read(safe_path(staged_root, &name)?)?,
                    after.as_ref().unwrap().mode,
                )?;
                let mut parent = Path::new(&name).parent();
                while let Some(p) = parent {
                    let n = p.to_str().context("Non-UTF8 transaction path")?;
                    if n.is_empty() {
                        break;
                    }
                    if !exists(&safe_path(root, n)?)? {
                        created_parents.insert(n.to_string());
                    }
                    parent = p.parent();
                }
            }
            files.push(PlannedFile {
                path: name,
                before,
                after,
                backup,
                staged,
            });
        }
        let journal = UpdateJournal {
            schema_version: 2,
            stage: stage.to_string(),
            files,
            // Component-internal parents are already derived from the exact
            // created source set. Avoid redundant entries in public journals.
            created_parents: created_parents
                .into_iter()
                .filter(|p| {
                    !created
                        .iter()
                        .any(|d| p == d || p.starts_with(&format!("{d}/")))
                })
                .collect(),
            created_directories: created,
        };
        validate_journal(&journal)?;
        Ok(Self { journal })
    }
    pub fn commit(
        &self,
        root: &Path,
        hooks: Option<&dyn MutationHooks>,
        preflight: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        if let Some(h) = hooks {
            h.after_stage()?;
        }
        preflight()?;
        for file in &self.journal.files {
            ensure!(
                identity(&safe_path(root, &file.path)?)? == file.before,
                "Concurrent update conflict: {}",
                file.path
            );
            verify_saved(root, file)?;
        }
        assert_no_pending(root)?;
        // Also bind already-existing ancestry up to the admitted checkout.
        // It may have been created by an older engine without directory fsync.
        sync_stage_ancestry(root, &self.journal.stage)?;
        write_json(&safe_path(root, PENDING_PATH)?, &self.journal)?;
        let result = (|| {
            for (i, file) in self.journal.files.iter().enumerate() {
                let p = safe_path(root, &file.path)?;
                ensure!(
                    identity(&p)? == file.before,
                    "Concurrent update conflict: {}",
                    file.path
                );
                verify_saved(root, file)?;
                if let Some(staged) = &file.staged {
                    write_atomic(
                        &p,
                        &fs::read(safe_path(root, staged)?)?,
                        file.after.as_ref().unwrap().mode,
                    )?;
                } else {
                    remove_file(&p)?;
                }
                if let Some(h) = hooks {
                    h.after_write(i + 1, &file.path)?;
                }
            }
            remove_file(&safe_path(root, PENDING_PATH)?)?;
            Ok(())
        })();
        if let Err(error) = result {
            match recover_locked(root) {
                Ok(_) => return Err(error),
                Err(rollback) => anyhow::bail!(
                    "{error}; automatic rollback was blocked: {rollback}. Run devkit recover."
                ),
            }
        }
        Ok(())
    }
}
