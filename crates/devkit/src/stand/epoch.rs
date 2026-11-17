//! Physical accepted-runtime history, independent of port lease incarnations.
use crate::{Result, files::*, state::*};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const RECEIPT: &str = ".devkit/state/stand-protocol/receipt.json";
const TRANSFER: &str = ".devkit/state/stand-protocol/transfer.json";
pub const CAPABILITY: &str = "devkit-stand-protocol-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Physical {
    dev: u64,
    ino: u64,
}
impl Physical {
    pub fn of(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
    pub fn at(path: &Path) -> Result<Self> {
        Ok(Self::of(&fs::symlink_metadata(path)?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Witness {
    physical: Physical,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    sha256: String,
}
impl Witness {
    fn read(file: &mut File) -> Result<Self> {
        use std::io::{Seek, SeekFrom};
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() <= MAX_MANIFEST_BYTES,
            "Invalid epoch manifest"
        );
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        let value = Self {
            physical: Physical::of(&meta),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            sha256: digest(&bytes),
        };
        let after = file.metadata()?;
        ensure!(
            bytes.len() as u64 == meta.len()
                && Physical::of(&after) == value.physical
                && (after.ctime(), after.ctime_nsec()) == value.ctime
                && (after.mtime(), after.mtime_nsec()) == value.mtime,
            "Epoch manifest changed while reading"
        );
        Ok(value)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    protocol_version: u32,
    root: Physical,
    state: Physical,
    lock: Physical,
    witness_name: String,
    witness: Witness,
}

fn regular(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?)
}

pub fn read_bounded<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<Option<T>> {
    let Some((bytes, _)) = read_regular_file(path, max, "stand state")? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_slice(&bytes).context("Invalid stand state JSON")?,
    ))
}

fn manifest(root: &Path) -> Result<Option<(File, Witness)>> {
    if !exists(&safe_path(root, MANIFEST_PATH)?)? {
        return Ok(None);
    }
    let state = read_project_state(root)?;
    if SemVer::parse(&state.version)? < SemVer::parse("0.5.0")? {
        return Ok(None);
    }
    let loader = ".devkit/runtime/devkit.sh";
    let Some(id) = state.managed.get(loader) else {
        return Ok(None);
    };
    let path = safe_path(root, loader)?;
    let Some((bytes, actual)) = read_regular_file(&path, 64 * 1024, "stand protocol loader")?
    else {
        return Ok(None);
    };
    ensure!(
        actual.matches_git_tracked(id),
        "Managed stand protocol loader changed"
    );
    if !bytes
        .windows(CAPABILITY.len())
        .any(|s| s == CAPABILITY.as_bytes())
    {
        return Ok(None);
    }
    let path = safe_path(root, MANIFEST_PATH)?;
    let mut file = regular(&path)?;
    let witness = Witness::read(&mut file)?;
    ensure!(
        Physical::at(&path)? == witness.physical,
        "Epoch manifest replaced"
    );
    Ok(Some((file, witness)))
}

/// This ticket cannot be reconstructed from persistent state. Its directory FD
/// holds the exact successful mkdir incarnation until acquisition finishes.
pub struct Creator {
    root: Physical,
    directory: File,
    manifest: File,
    before: Witness,
}

pub fn before_lock(root: &Path) -> Result<Option<Creator>> {
    if fs::symlink_metadata(safe_path(root, ".devkit/state")?).is_ok() {
        return Ok(None);
    }
    let Some((file, before)) = manifest(root)? else {
        return Ok(None);
    };
    let state = safe_path(root, ".devkit/state")?;
    match fs::create_dir(&state) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    sync_directory(state.parent().context("State parent")?)?;
    let directory = regular(&state)?;
    Ok(Some(Creator {
        root: Physical::at(root)?,
        directory,
        manifest: file,
        before,
    }))
}

pub fn trusted(root: &Path) -> Result<bool> {
    let Some(receipt): Option<Receipt> = read_bounded(&safe_path(root, RECEIPT)?, 65536)? else {
        return Ok(false);
    };
    ensure!(
        receipt.protocol_version == 1,
        "Unsupported stand protocol version"
    );
    ensure!(
        GENERATION_RE.is_match(&receipt.witness_name),
        "Invalid stand witness name"
    );
    if receipt.root != Physical::at(root)?
        || receipt.state != Physical::at(&safe_path(root, ".devkit/state")?)?
        || receipt.lock != Physical::at(&safe_path(root, ".devkit/state/task.lock")?)?
    {
        return Ok(false);
    }
    let current = safe_path(root, MANIFEST_PATH)?;
    let witness_path = safe_path(
        root,
        &format!(".devkit/state/stand-protocol/{}", receipt.witness_name),
    )?;
    let mut linked = match regular(&witness_path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if !linked.metadata()?.is_file() || Physical::at(&current)? != receipt.witness.physical {
        return Ok(false);
    }
    Ok(Witness::read(&mut linked)? == receipt.witness)
}

/// Runs under the stable lock, before other operation-specific state effects.
pub fn after_lock(root: &Path, creator: Option<Creator>) -> Result<()> {
    if trusted(root)? {
        return Ok(());
    }
    let Some(mut ticket) = creator else {
        return Ok(());
    };
    if ticket.root != Physical::at(root)?
        || Physical::of(&ticket.directory.metadata()?)
            != Physical::at(&safe_path(root, ".devkit/state")?)?
        || Witness::read(&mut ticket.manifest)? != ticket.before
        || Physical::at(&safe_path(root, MANIFEST_PATH)?)? != ticket.before.physical
    {
        return Ok(());
    }
    assert_no_pending(root)?;
    for item in fs::read_dir(safe_path(root, ".devkit/state")?)? {
        let name = item?.file_name();
        if name != "task.lock" && name != "task.owner.json" {
            return Ok(());
        }
    }
    anchor(root)
}

/// Caller has either an unbroken producer ticket, a verified transaction
/// transfer, or a complete empty stand observation. This does not clear history.
pub fn anchor(root: &Path) -> Result<()> {
    let Some((mut file, before)) = manifest(root)? else {
        return Ok(());
    };
    let directory = safe_path(root, ".devkit/state/stand-protocol")?;
    create_dir_all_durable(&directory)?;
    let name = uuid::Uuid::new_v4().to_string();
    let source = safe_path(root, MANIFEST_PATH)?;
    fs::hard_link(&source, directory.join(&name))
        .context("Cannot witness accepted runtime; stand history remains unknown")?;
    sync_directory(&directory)?;
    let witness = Witness::read(&mut file)?;
    ensure!(
        witness.physical == before.physical
            && witness.sha256 == before.sha256
            && Physical::at(&source)? == witness.physical,
        "Accepted runtime changed while anchoring epoch"
    );
    write_json(
        &safe_path(root, RECEIPT)?,
        &Receipt {
            protocol_version: 1,
            root: Physical::at(root)?,
            state: Physical::at(&safe_path(root, ".devkit/state")?)?,
            lock: Physical::at(&safe_path(root, ".devkit/state/task.lock")?)?,
            witness_name: name,
            witness,
        },
    )
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Transfer {
    journal: String,
    seed: TransferSeed,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransferSeed {
    receipt: String,
    history: std::collections::BTreeMap<String, Option<crate::model::FileIdentity>>,
}
fn history(
    root: &Path,
) -> Result<std::collections::BTreeMap<String, Option<crate::model::FileIdentity>>> {
    [super::ledger::ACTIVE, super::ports::RECEIPT]
        .into_iter()
        .map(|name| Ok((name.to_owned(), identity(&safe_path(root, name)?)?)))
        .collect()
}

/// Prepare before publication of the pending marker; commit inside its window.
pub fn prepare_transfer(root: &Path) -> Result<Option<TransferSeed>> {
    if trusted(root)? {
        Ok(Some(TransferSeed {
            receipt: digest(fs::read(safe_path(root, RECEIPT)?)?),
            history: history(root)?,
        }))
    } else {
        Ok(None)
    }
}
pub fn begin_transfer(root: &Path, journal: &[u8], seed: Option<TransferSeed>) -> Result<()> {
    if let Some(seed) = seed {
        write_json(
            &safe_path(root, TRANSFER)?,
            &Transfer {
                journal: digest(journal),
                seed,
            },
        )?;
    }
    Ok(())
}
/// Recovery can follow a committed epoch whose pending-marker unlink failed.
/// Capture that exact trusted terminal receipt before rollback replaces it.
pub fn recovery_transfer(root: &Path, journal: &[u8]) -> Result<()> {
    if let Some(mut transfer) = read_bounded::<Transfer>(&safe_path(root, TRANSFER)?, 65536)? {
        ensure!(
            transfer.journal == digest(journal) && transfer.seed.history == history(root)?,
            "Stand epoch recovery transfer changed"
        );
        let receipt = digest(fs::read(safe_path(root, RECEIPT)?)?);
        if receipt != transfer.seed.receipt {
            ensure!(
                trusted(root)?,
                "Stand epoch terminal receipt is not trusted"
            );
            transfer.seed.receipt = receipt;
            write_json(&safe_path(root, TRANSFER)?, &transfer)?;
        }
    }
    Ok(())
}
pub fn finish_transfer(root: &Path, journal: &[u8]) -> Result<()> {
    if let Some(mut transfer) = read_bounded::<Transfer>(&safe_path(root, TRANSFER)?, 65536)? {
        ensure!(
            transfer.journal == digest(journal)
                && transfer.seed.receipt == digest(fs::read(safe_path(root, RECEIPT)?)?)
                && transfer.seed.history == history(root)?,
            "Stand epoch transaction transfer changed"
        );
        anchor(root)?;
        transfer.seed.receipt = digest(fs::read(safe_path(root, RECEIPT)?)?);
        // Leave a recoverable transfer until the pending marker is durable gone.
        write_json(&safe_path(root, TRANSFER)?, &transfer)?;
    }
    Ok(())
}
pub fn clear_transfer(root: &Path) -> Result<()> {
    let path = safe_path(root, TRANSFER)?;
    if exists(&path)? {
        remove_file(&path)?;
    }
    Ok(())
}
