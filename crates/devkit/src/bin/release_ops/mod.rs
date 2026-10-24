//! A bounded release state machine, with provider operations at one seam.
mod provider;
mod readiness;
#[cfg(test)]
mod tests;

use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use devkit::{
    Result, archive, assembler,
    distribution::{NpmArtifact, ReleaseIndex},
    files::{self, SHA256_RE},
    processes::ProcessRunner,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const REPO: &str = "docouno/devkit";
const CONTRACT_OPEN: &str = "<!-- devkit-release-contract-v1\n";
const CONTRACT_CLOSE: &str = "\nend-devkit-release-contract -->";
const PROOF_OPEN: &str = "<!-- devkit-public-proof-v1\n";
const PROOF_CLOSE: &str = "\nend-devkit-public-proof -->";
const ENVIRONMENT: &str = "@docouno/env";
const CLI: &str = "@docouno/devkit";

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= maximum,
        "Not a bounded regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == metadata.len() && bytes.len() as u64 <= maximum,
        "File changed while reading: {}",
        path.display()
    );
    Ok(bytes)
}
fn source_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn stable(value: &str) -> Result<[u64; 3]> {
    files::version(value)?;
    let parts: Vec<_> = value
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        parts.len() == 3,
        "Publication requires an exact stable version"
    );
    Ok([parts[0], parts[1], parts[2]])
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selectors {
    environment: Option<String>,
    cli: Option<String>,
    github: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Contract {
    schema_version: u32,
    repository: String,
    version: String,
    source_commit: String,
    source_tree: String,
    build_run: String,
    index_sha256: String,
    previous: Selectors,
}
impl Contract {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.repository == REPO,
            "Unknown release contract"
        );
        stable(&self.version)?;
        ensure!(
            source_hash(&self.source_commit)
                && source_hash(&self.source_tree)
                && SHA256_RE.is_match(&self.index_sha256),
            "Invalid contract source/index identity"
        );
        self.build_run_id()?;
        for value in [&self.previous.environment, &self.previous.cli]
            .into_iter()
            .flatten()
        {
            files::version(value)?;
        }
        if let Some(value) = &self.previous.github {
            stable(
                value
                    .strip_prefix('v')
                    .context("Previous GitHub selector must be a version tag")?,
            )?;
        }
        Ok(())
    }
    fn build_run_id(&self) -> Result<u64> {
        let suffix = self
            .build_run
            .strip_prefix("https://github.com/docouno/devkit/actions/runs/")
            .context("Build run must identify the original GitHub Actions attempt")?;
        let (run, attempt) = suffix
            .split_once("/attempts/")
            .context("Build run must include its attempt")?;
        ensure!(
            run.bytes().all(|b| b.is_ascii_digit()) && attempt.bytes().all(|b| b.is_ascii_digit()),
            "Invalid build run"
        );
        ensure!(attempt.parse::<u64>()? > 0, "Invalid build attempt");
        let run = run.parse()?;
        ensure!(run > 0, "Invalid build run id");
        Ok(run)
    }
    fn tag(&self) -> String {
        format!("v{}", self.version)
    }
    fn body(&self) -> Result<String> {
        Ok(format!(
            "Devkit {} for Linux x86_64 and local Docker Compose.\n\nUpgrade 0.2 projects using the new npm CLI or this release's install.sh; older versioned bootstrap scripts retain their original behavior.\n\n{CONTRACT_OPEN}{}{CONTRACT_CLOSE}\n",
            self.version,
            serde_json::to_string(self)?
        ))
    }
    fn from_body(body: &str) -> Result<Self> {
        let result: Self = serde_json::from_str(block(body, CONTRACT_OPEN, CONTRACT_CLOSE)?)?;
        result.validate()?;
        Ok(result)
    }
    fn check_index(&self, set: &FrozenSet) -> Result<()> {
        let i = &set.index;
        ensure!(
            self.version == i.version
                && self.source_commit == i.source_commit
                && self.source_tree == i.source_tree
                && self.build_run == i.build_run
                && self.index_sha256 == set.index_sha256,
            "Frozen build set differs from durable release contract"
        );
        Ok(())
    }
}
fn block<'a>(body: &'a str, open: &str, close: &str) -> Result<&'a str> {
    ensure!(
        body.matches(open).count() == 1 && body.matches(close).count() == 1,
        "Missing or duplicate release metadata block"
    );
    body.split_once(open)
        .and_then(|(_, tail)| tail.split_once(close).map(|(value, _)| value))
        .context("Malformed release metadata block")
}

struct FrozenSet {
    root: PathBuf,
    index: ReleaseIndex,
    index_sha256: String,
}
impl FrozenSet {
    fn load(root: &Path) -> Result<Self> {
        ensure!(
            fs::symlink_metadata(root)?.is_dir(),
            "Build set must be a real directory"
        );
        let bytes = read_bounded(&root.join("release-index.json"), 4 * 1024 * 1024)?;
        let index: ReleaseIndex = serde_json::from_slice(&bytes)?;
        index.validate()?;
        ensure!(
            index.publication_eligible
                && source_hash(&index.source_commit)
                && source_hash(&index.source_tree),
            "Build set is not publication eligible"
        );
        stable(&index.version)?;
        let set = Self {
            root: root.to_path_buf(),
            index,
            index_sha256: digest(&bytes),
        };
        for (name, identity) in &set.index.files {
            let bytes = read_bounded(&root.join(name), 128 * 1024 * 1024)?;
            ensure!(
                bytes.len() as u64 == identity.size && digest(&bytes) == identity.sha256,
                "Frozen asset differs: {name}"
            );
        }
        let allowed: BTreeSet<_> = set
            .index
            .files
            .keys()
            .map(String::as_str)
            .chain(["release-index.json", "payload"])
            .collect();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            ensure!(
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| allowed.contains(n)),
                "Unexpected build-set member"
            );
        }
        for package in set.index.packages.values() {
            let bytes = read_bounded(&root.join(&package.archive), 128 * 1024 * 1024)?;
            verify_npm_bytes(package, &package.integrity, &package.shasum, &bytes)?;
        }
        Ok(set)
    }
    fn contract(&self, previous: Selectors) -> Result<Contract> {
        let result = Contract {
            schema_version: 1,
            repository: REPO.into(),
            version: self.index.version.clone(),
            source_commit: self.index.source_commit.clone(),
            source_tree: self.index.source_tree.clone(),
            build_run: self.index.build_run.clone(),
            index_sha256: self.index_sha256.clone(),
            previous,
        };
        result.validate()?;
        Ok(result)
    }
    fn hydrate(&self) -> Result<()> {
        let payload = self.root.join("payload");
        if !payload.exists() {
            archive::extract_archive(
                &self.root.join(&self.index.archive),
                &payload,
                &format!("devkit-{}", self.index.version),
            )?;
        }
        let admitted = devkit::release::admit_release(&payload)?;
        ensure!(
            admitted.identity == self.index.release_identity,
            "Hydrated payload identity differs"
        );
        Ok(())
    }
    fn asset_names(&self) -> BTreeSet<String> {
        self.index
            .files
            .keys()
            .cloned()
            .chain(["release-index.json".into()])
            .collect()
    }
    fn asset_bytes(&self, name: &str) -> Result<Vec<u8>> {
        let bytes = read_bounded(&self.root.join(name), 128 * 1024 * 1024)?;
        if name == "release-index.json" {
            ensure!(
                digest(&bytes) == self.index_sha256,
                "Frozen index changed after validation"
            );
        } else {
            let expected = self
                .index
                .files
                .get(name)
                .context("Asset is not in the frozen index")?;
            ensure!(
                bytes.len() as u64 == expected.size && digest(&bytes) == expected.sha256,
                "Frozen asset changed after validation: {name}"
            );
        }
        Ok(bytes)
    }
    fn operation_copy(&self, name: &str, directory: &Path) -> Result<PathBuf> {
        let path = directory.join(name);
        files::write_atomic(&path, &self.asset_bytes(name)?, 0o444)?;
        Ok(path)
    }
}
fn verify_npm_bytes(
    expected: &NpmArtifact,
    integrity: &str,
    shasum: &str,
    bytes: &[u8],
) -> Result<()> {
    ensure!(
        integrity == expected.integrity
            && shasum == expected.shasum
            && format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes))) == integrity
            && format!("{:x}", Sha1::digest(bytes)) == shasum,
        "Published npm bytes/SRI differ for {}; use a new patch version",
        expected.name
    );
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
struct RemoteAsset {
    id: u64,
    name: String,
    size: u64,
    digest: Option<String>,
    state: String,
}
#[derive(Clone, Debug, Deserialize)]
struct RemoteRelease {
    id: u64,
    tag_name: String,
    target_commitish: String,
    draft: bool,
    prerelease: bool,
    immutable: bool,
    body: Option<String>,
    assets: Vec<RemoteAsset>,
}
impl RemoteRelease {
    fn contract(&self) -> Result<Contract> {
        let contract = Contract::from_body(self.body.as_deref().unwrap_or_default())?;
        ensure!(
            self.tag_name == contract.tag(),
            "GitHub release tag differs from contract"
        );
        if self.draft {
            ensure!(
                self.target_commitish == contract.source_commit,
                "Draft target is not the exact source commit"
            );
        } else {
            ensure!(
                self.immutable,
                "Public release must be immutable before continuing"
            );
        }
        Ok(contract)
    }
}
struct PublishedNpm {
    integrity: String,
    shasum: String,
    bytes: Vec<u8>,
}
trait Remote {
    fn release(&mut self, tag: &str) -> Result<Option<RemoteRelease>>;
    fn create_draft(&mut self, contract: &Contract) -> Result<RemoteRelease>;
    fn download_asset(
        &mut self,
        release: &RemoteRelease,
        asset: &RemoteAsset,
        path: &Path,
    ) -> Result<()>;
    fn upload_asset(&mut self, release: &RemoteRelease, path: &Path) -> Result<()>;
    fn npm_version(&mut self, name: &str, version: &str) -> Result<Option<PublishedNpm>>;
    fn npm_after_publish(&mut self, name: &str, version: &str) -> Result<PublishedNpm>;
    fn publish_npm(&mut self, path: &Path, tag: &str) -> Result<()>;
    fn selectors(&mut self) -> Result<Selectors>;
    fn selectors_after_latest(
        &mut self,
        contract: &Contract,
        acknowledged: &BTreeMap<String, String>,
        pending: Option<&str>,
    ) -> Result<Selectors>;
    fn npm_latest(&mut self, name: &str, version: &str) -> Result<()>;
    fn edit_release(
        &mut self,
        release: &RemoteRelease,
        body: &str,
        channel: Option<bool>,
    ) -> Result<()>;
    fn tag_source(&mut self, tag: &str) -> Result<Option<(String, String)>>;
    fn main_contains(&mut self, commit: &str, tree: &str) -> Result<()>;
}

fn check_source(remote: &mut impl Remote, contract: &Contract) -> Result<()> {
    remote.main_contains(&contract.source_commit, &contract.source_tree)?;
    if let Some((commit, tree)) = remote.tag_source(&contract.tag())? {
        ensure!(
            commit == contract.source_commit && tree == contract.source_tree,
            "Existing Git tag differs; tags must never move"
        );
    }
    Ok(())
}
fn check_remote_assets(
    remote: &mut impl Remote,
    release: &RemoteRelease,
    set: &FrozenSet,
    complete: bool,
) -> Result<()> {
    let expected = set.asset_names();
    let mut seen = BTreeSet::new();
    for asset in &release.assets {
        ensure!(
            seen.insert(asset.name.clone())
                && expected.contains(&asset.name)
                && asset.state == "uploaded",
            "Unexpected, duplicate or incomplete GitHub asset: {}",
            asset.name
        );
        let bytes = set.asset_bytes(&asset.name)?;
        ensure!(
            asset.size == bytes.len() as u64
                && asset.digest.as_deref() == Some(&format!("sha256:{}", digest(&bytes))),
            "GitHub asset metadata differs: {}",
            asset.name
        );
        let directory = tempfile::tempdir()?;
        let downloaded = directory.path().join(&asset.name);
        remote.download_asset(release, asset, &downloaded)?;
        ensure!(
            read_bounded(&downloaded, 128 * 1024 * 1024)? == bytes,
            "GitHub asset bytes differ: {}",
            asset.name
        );
    }
    if complete {
        ensure!(
            seen == expected,
            "GitHub draft is incomplete; recover the original build artifact, never rebuild this version"
        );
    }
    Ok(())
}
fn check_npm(
    remote: &mut impl Remote,
    set: &FrozenSet,
    required: bool,
) -> Result<BTreeSet<String>> {
    let mut existing = BTreeSet::new();
    for package in set.index.packages.values() {
        if let Some(published) = remote.npm_version(&package.name, &package.version)? {
            verify_npm_bytes(
                package,
                &published.integrity,
                &published.shasum,
                &published.bytes,
            )?;
            ensure!(
                published.bytes == set.asset_bytes(&package.archive)?,
                "Npm and frozen archive bytes differ"
            );
            existing.insert(package.name.clone());
        } else {
            ensure!(!required, "Public package is missing: {}", package.name);
        }
    }
    Ok(existing)
}
fn guard_npm_predecessors(previous: &Selectors, version: &str) -> Result<()> {
    let target = stable(version)?;
    for (label, previous) in [(ENVIRONMENT, &previous.environment), (CLI, &previous.cli)] {
        let message = format!(
            "Normal releases require an existing stable predecessor for {label}; bootstrap a new package separately, not through a stage placeholder"
        );
        let previous = previous.as_deref().context(message.clone())?;
        let predecessor = stable(previous).context(message)?;
        ensure!(
            predecessor < target,
            "Previous {label} version must be older than the target release"
        );
    }
    Ok(())
}
fn guard_selectors(current: &Selectors, contract: &Contract) -> Result<()> {
    guard_current_selectors(current, &contract.previous, &contract.version)
}
fn guard_current_selectors(current: &Selectors, previous: &Selectors, version: &str) -> Result<()> {
    let target = stable(version)?;
    for (label, current, previous, github) in [
        (
            "environment",
            &current.environment,
            &previous.environment,
            false,
        ),
        ("CLI", &current.cli, &previous.cli, false),
        ("GitHub", &current.github, &previous.github, true),
    ] {
        let expected_target = if github {
            format!("v{version}")
        } else {
            version.to_owned()
        };
        ensure!(
            current.as_deref() == Some(&expected_target) || current == previous,
            "Unexpected {label} latest pointer; refusing to overwrite it"
        );
        if let Some(value) = current {
            let value = if github {
                value
                    .strip_prefix('v')
                    .context("GitHub latest is not a version tag")?
            } else {
                value
            };
            let base = value.split_once('-').map_or(value, |(base, _)| base);
            ensure!(
                stable(base)? <= target,
                "Newer {label} latest pointer; refusing downgrade"
            );
        }
    }
    Ok(())
}
fn stage(remote: &mut impl Remote, set: &FrozenSet, contract: &Contract) -> Result<()> {
    contract.check_index(set)?;
    guard_npm_predecessors(&contract.previous, &contract.version)?;
    check_source(remote, contract)?;
    guard_selectors(&remote.selectors()?, contract)?;
    let existing = check_npm(remote, set, false)?;
    let release = if let Some(release) = remote.release(&contract.tag())? {
        ensure!(
            release.contract()? == *contract,
            "Durable release contract differs from requested release"
        );
        release
    } else {
        ensure!(
            existing.is_empty(),
            "Registry version already exists without an original GitHub frozen set; use a new patch"
        );
        remote.create_draft(contract)?
    };
    ensure!(
        release.contract()? == *contract,
        "Created draft contract differs"
    );
    check_remote_assets(
        remote,
        &release,
        set,
        !release.draft || !existing.is_empty(),
    )?;
    for name in set.asset_names() {
        if !release.assets.iter().any(|asset| asset.name == name) {
            ensure!(
                release.draft && existing.is_empty(),
                "Cannot append assets to a public or partially published version"
            );
            let directory = tempfile::tempdir()?;
            let frozen = set.operation_copy(&name, directory.path())?;
            remote.upload_asset(&release, &frozen)?;
        }
    }
    let release = remote
        .release(&contract.tag())?
        .context("Draft disappeared after upload")?;
    ensure!(
        release.contract()? == *contract,
        "Release contract changed during staging"
    );
    check_remote_assets(remote, &release, set, true)
}
fn publish(remote: &mut impl Remote, set: &FrozenSet, contract: &Contract) -> Result<()> {
    contract.check_index(set)?;
    guard_npm_predecessors(&contract.previous, &contract.version)?;
    check_source(remote, contract)?;
    let release = remote
        .release(&contract.tag())?
        .context("Stage every frozen asset before publishing npm")?;
    ensure!(
        release.contract()? == *contract,
        "Durable release contract differs"
    );
    check_remote_assets(remote, &release, set, true)?;
    guard_selectors(&remote.selectors()?, contract)?;
    let existing = check_npm(remote, set, false)?;
    for kind in ["environment", "cli"] {
        let package = &set.index.packages[kind];
        if !existing.contains(&package.name) {
            let directory = tempfile::tempdir()?;
            let frozen = set.operation_copy(&package.archive, directory.path())?;
            remote.publish_npm(&frozen, &format!("candidate-{}", contract.version))?;
            // A successful request is not proof that the registry stores our bytes.
            let value = remote
                .npm_after_publish(&package.name, &package.version)
                .with_context(|| {
                    format!(
                        "Could not verify published {}@{}; retry against the original frozen set",
                        package.name, package.version
                    )
                })?;
            verify_npm_bytes(package, &value.integrity, &value.shasum, &value.bytes)?;
            eprintln!(
                "Verified frozen npm archive {}@{}",
                package.name, package.version
            );
        }
    }
    check_npm(remote, set, true)?;
    if release.draft {
        remote.edit_release(
            &release,
            release.body.as_deref().context("Draft body missing")?,
            Some(false),
        )?;
    }
    let public = remote
        .release(&contract.tag())?
        .context("Published release missing")?;
    ensure!(
        !public.draft && public.immutable && public.contract()? == *contract,
        "Release did not become immutable with the same contract"
    );
    ensure!(
        remote.tag_source(&contract.tag())?
            == Some((contract.source_commit.clone(), contract.source_tree.clone())),
        "Published Git tag/source binding differs"
    );
    check_remote_assets(remote, &public, set, true)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Proof {
    schema_version: u32,
    version: String,
    source_commit: String,
    source_tree: String,
    index_sha256: String,
    publication_run: String,
    gates: BTreeMap<String, Gate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_promotion_npx: Option<Gate>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Gate {
    sha256: String,
    bytes: u64,
    summary: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeUpgrade {
    from_version: String,
    to_version: String,
    from_release_identity: String,
    to_release_identity: String,
    accepted_old_after_fresh_cli_add: bool,
    old_engine_version_verified: bool,
    new_engine_version_verified: bool,
    product_inputs_preserved: bool,
    preview_readonly: bool,
    checks_passed: bool,
    same_version_noop: bool,
    host_sdk_attempts: u64,
}
fn native_upgrade_from(c: &Contract) -> Option<&str> {
    c.previous
        .environment
        .as_deref()
        .filter(|previous| stable(previous).is_ok_and(|version| version >= [0, 3, 0]))
}
fn validate_native_upgrade(
    readme: &Value,
    c: &Contract,
    target_identity: Option<&str>,
) -> Result<()> {
    let Some(previous) = native_upgrade_from(c) else {
        return Ok(());
    };
    let upgrade: NativeUpgrade = serde_json::from_value(readme["nativeUpgrade"].clone())
        .context("Actual native-to-native upgrade proof is missing or malformed")?;
    ensure!(
        upgrade.from_version == previous
            && upgrade.to_version == c.version
            && stable(&upgrade.from_version)? < stable(&upgrade.to_version)?
            && SHA256_RE.is_match(&upgrade.from_release_identity)
            && SHA256_RE.is_match(&upgrade.to_release_identity)
            && upgrade.from_release_identity != upgrade.to_release_identity,
        "Native upgrade version/release identities differ"
    );
    ensure!(
        upgrade.accepted_old_after_fresh_cli_add
            && upgrade.old_engine_version_verified
            && upgrade.new_engine_version_verified
            && upgrade.product_inputs_preserved
            && upgrade.preview_readonly
            && upgrade.checks_passed
            && upgrade.same_version_noop
            && upgrade.host_sdk_attempts == 0,
        "Actual native-to-native upgrade checks are incomplete"
    );
    if let Some(target) = target_identity {
        ensure!(
            upgrade.to_release_identity == target,
            "Native upgrade target differs from the frozen release identity"
        );
    }
    Ok(())
}
impl Proof {
    fn validate(&self, contract: &Contract) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.version == contract.version
                && self.source_commit == contract.source_commit
                && self.source_tree == contract.source_tree
                && self.index_sha256 == contract.index_sha256,
            "Public proof identifies a different frozen set"
        );
        ensure!(
            self.publication_run
                .starts_with("https://github.com/docouno/devkit/actions/runs/"),
            "Proof must identify its Actions run"
        );
        ensure!(
            self.gates
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
                == BTreeSet::from([
                    "public-0.2.0",
                    "public-0.2.1",
                    "npx-pinned",
                    "performance",
                    "transport-cold",
                    "readme"
                ]),
            "Public proof is missing required gates"
        );
        for (name, gate) in &self.gates {
            ensure!(
                SHA256_RE.is_match(&gate.sha256)
                    && gate.bytes > 0
                    && gate.bytes <= 16 * 1024 * 1024,
                "Invalid receipt identity: {name}"
            );
            validate_gate(name, &gate.summary, contract)?;
        }
        if let Some(gate) = &self.post_promotion_npx {
            ensure!(
                SHA256_RE.is_match(&gate.sha256)
                    && gate.bytes > 0
                    && gate.bytes <= 16 * 1024 * 1024,
                "Invalid post-promotion receipt identity"
            );
            validate_gate("npx-latest", &gate.summary, contract)?;
        }
        Ok(())
    }
}
fn validate_gate(name: &str, value: &Value, c: &Contract) -> Result<()> {
    ensure!(value["success"] == true, "Gate failed: {name}");
    if let Some(old) = name.strip_prefix("public-") {
        ensure!(
            value["schemaVersion"] == 3
                && value["version"] == old
                && value["nextVersion"] == c.version
                && value["expectedIndexSha256"] == c.index_sha256
                && value["authenticationUsedForDownloads"] == false
                && value["defaultReleaseBaseTested"] == true,
            "Public matrix receipt differs: {name}"
        );
        let identities = value["identities"]
            .as_array()
            .context("Public identity proof missing")?;
        ensure!(
            identities.len() == 1
                && identities[0]["sourceCommit"] == c.source_commit
                && identities[0]["sourceTree"] == c.source_tree
                && identities[0]["indexSha256"] == c.index_sha256,
            "Public source receipt differs"
        );
        ensure!(
            value["consumerSuccess"] == true,
            "Packaged consumer success must be captured in proof summary"
        );
    } else {
        ensure!(
            value["schemaVersion"] == 1
                && value["version"] == c.version
                && value["sourceCommit"] == c.source_commit
                && value["sourceTree"] == c.source_tree
                && value["indexSha256"] == c.index_sha256,
            "Receipt source identity differs: {name}"
        );
        if name.starts_with("npx-") {
            ensure!(
                value["selectorMode"]
                    == if name == "npx-pinned" {
                        "pinned"
                    } else {
                        "latest"
                    }
                    && value["ignoreScripts"] == true
                    && value["authenticationUsed"] == false
                    && value["umasks"] == json!(["022", "077"]),
                "Npx public gate is incomplete"
            );
        }
        if name == "performance" {
            ensure!(
                value["measurements"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
                "Performance measurements missing"
            );
            validate_performance_trace(value)?;
        }
        if name == "transport-cold" {
            ensure!(
                value["authenticationUsed"] == false,
                "Cold transport must be anonymous"
            );
            let measurements = value["measurements"]
                .as_array()
                .context("Actual cold transport measurements missing")?;
            for transport in ["npx", "curl"] {
                ensure!(
                    measurements.iter().any(|m| m["transport"] == transport
                        && m["cacheState"] == "empty"
                        && m["seconds"]
                            .as_f64()
                            .is_some_and(|v| v.is_finite() && v > 0.0)),
                    "Actual cold {transport} proof missing"
                );
            }
            ensure!(
                measurements
                    .iter()
                    .filter(|m| m["transport"] == "curl")
                    .all(|m| m["hostLanguageSDKAttempted"] == false),
                "Cold curl attempted a host SDK"
            );
        }
        if name == "readme" {
            ensure!(
                value["authenticationUsed"] == false
                    && value["publicNpx"] == true
                    && value["barePtyCancelled"] == true
                    && value["standaloneWebStatePreservingHmr"] == true
                    && value["hostLanguageSDKAttemptedForNativeCommands"] == false
                    && value["readmeSha256"]
                        .as_str()
                        .is_some_and(|sha| SHA256_RE.is_match(sha)),
                "Actual public README flow is incomplete"
            );
            validate_native_upgrade(value, c, None)?;
        }
    }
    Ok(())
}
fn selected(value: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| value.get(*key).map(|v| ((*key).into(), v.clone())))
            .collect(),
    )
}
const PERFORMANCE_TRACE_COLUMNS: &[&str] = &[
    "dockerCliCalls",
    "oneShotContainers",
    "controlContainers",
    "toolContainers",
    "otherContainers",
    "execSyscallAttempts",
    "execSyscallErrors",
    "successfulExecs",
    "instrumentedTotalSeconds",
    "instrumentedToolContainerSeconds",
    "instrumentedOutsideToolSeconds",
];
fn validate_performance_trace(value: &Value) -> Result<()> {
    let measurements = value["measurements"].as_array().context("Performance measurements missing")?;
    let columns = value.get("traceColumns");
    if columns.is_none() && measurements.iter().all(|m| !m["trace"].is_array()) {
        // Historical schema1 proofs carry trace objects without a column list.
        return Ok(());
    }
    ensure!(columns == Some(&json!(PERFORMANCE_TRACE_COLUMNS)), "Invalid performance trace columns");
    for measurement in measurements {
        let row = measurement["trace"].as_array().context("Performance trace row missing")?;
        ensure!(row.len() == PERFORMANCE_TRACE_COLUMNS.len(), "Invalid performance trace row width");
        for (index, value) in row.iter().enumerate() {
            ensure!(
                if index < 8 { value.as_u64().is_some() } else { value.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0) },
                "Invalid performance trace value: {}", PERFORMANCE_TRACE_COLUMNS[index]
            );
        }
    }
    Ok(())
}
fn compact_gate(name: &str, value: Value) -> Result<Value> {
    if name == "readme" {
        let mut summary = selected(
            &value,
            &[
                "schemaVersion",
                "success",
                "version",
                "sourceCommit",
                "sourceTree",
                "indexSha256",
                "authenticationUsed",
                "publicNpx",
                "barePtyCancelled",
                "standaloneWebStatePreservingHmr",
                "hostLanguageSDKAttemptedForNativeCommands",
                "readmeSha256",
                "nativeUpgrade",
                "seconds",
                "limits",
            ],
        );
        summary["exampleResults"] = Value::Array(
            value["results"]
                .as_array()
                .context("README command receipts missing")?
                .iter()
                .map(|v| selected(v, &["label", "passed", "durationSeconds", "exitCode"]))
                .collect(),
        );
        return Ok(summary);
    }
    if name == "performance" {
        let mut summary = selected(
            &value,
            &[
                "schemaVersion",
                "success",
                "version",
                "sourceCommit",
                "sourceTree",
                "indexSha256",
                "buildRun",
                "releaseIdentity",
                "baseline",
                "durationSeconds",
                "comparisons",
                "definitions",
                "limits",
                "harness",
                "admissionChecks",
            ],
        );
        let measurements = value["measurements"]
            .as_array()
            .context("Performance measurements missing")?;
        // Keep every trace value exactly, with the repeated column names once
        // for the complete measurement set. Raw receipt identity remains bound
        // by Gate.sha256; no timing statistic is rounded or discarded.
        summary["traceColumns"] = json!(PERFORMANCE_TRACE_COLUMNS);
        summary["measurements"] = Value::Array(
            measurements
                .iter()
                .map(|measurement| {
                    let mut item = selected(
                        measurement,
                        &[
                            "name",
                            "shape",
                            "side",
                            "operation",
                            "componentCount",
                            "sampleCount",
                            "medianSeconds",
                            "p95Seconds",
                            "minSeconds",
                            "maxSeconds",
                            "inputsBytesAndModesUnchanged",
                        ],
                    );
                    item["trace"] = Value::Array(
                        PERFORMANCE_TRACE_COLUMNS
                            .iter()
                            .map(|key| measurement["trace"][*key].clone())
                            .collect(),
                    );
                    item
                })
                .collect(),
        );
        if let Some(fixtures) = value["fixtures"].as_array() {
            summary["fixtures"] = Value::Array(
                fixtures
                    .iter()
                    .map(|fixture| {
                        selected(
                            fixture,
                            &[
                                "shape",
                                "side",
                                "componentCount",
                                "localTransitiveCrates",
                                "version",
                                "releaseIdentity",
                                "inputSnapshotSha256",
                                "inputFiles",
                                "inputBytes",
                                "rustImageId",
                                "productInputSha256",
                                "sourceUnchanged",
                            ],
                        )
                    })
                    .collect(),
            );
        }
        summary["host"] = selected(
            &value["host"],
            &["system", "machine", "kernel", "cpuCount", "cpuModel"],
        );
        summary["tools"] = selected(
            &value["tools"],
            &[
                "traceImageId",
                "traceRecipeSha256",
                "straceFiles",
                "straceVersion",
            ],
        );
        return Ok(summary);
    }
    let mut summary = value;
    for key in [
        "downloads",
        "harnessSourceSha256",
        "scratch",
        "consumerReceipt",
        "commands",
        "diagnostics",
    ] {
        if let Some(o) = summary.as_object_mut() {
            o.remove(key);
        }
    }
    Ok(summary)
}
fn gate_from_receipt(name: &str, bytes: &[u8], value: Value, c: &Contract) -> Result<Gate> {
    validate_gate(name, &value, c)?;
    let summary = compact_gate(name, value)?;
    validate_gate(name, &summary, c)?;
    Ok(Gate {
        sha256: digest(bytes),
        bytes: bytes.len() as u64,
        summary,
    })
}
fn proof_from_files(c: &Contract, run: &str, paths: &BTreeMap<String, PathBuf>) -> Result<Proof> {
    let mut gates = BTreeMap::new();
    for (name, path) in paths {
        let bytes = read_bounded(path, 16 * 1024 * 1024)?;
        let mut value: Value = serde_json::from_slice(&bytes)?;
        if name.starts_with("public-") {
            let receipt_path = value["consumerReceipt"]
                .as_str()
                .context("Public consumer receipt missing")?
                .to_owned();
            let consumer_bytes = read_bounded(Path::new(&receipt_path), 16 * 1024 * 1024)?;
            let consumer: Value = serde_json::from_slice(&consumer_bytes)?;
            ensure!(
                consumer["passed"] == true
                    && consumer["hostLanguageSDKAttempted"] == false
                    && consumer["baselineUsesGenuineReleasedFactory"] == true
                    && consumer["nextVersionTested"] == true
                    && consumer["packageSource"] == "registry"
                    && consumer["nativeManagerVersion"] == c.version,
                "Actual packaged consumer matrix did not pass its public contracts"
            );
            value["consumerSuccess"] = json!(true);
            value["consumerReceiptSha256"] = json!(digest(&consumer_bytes));
        }
        // Keep durable content compact, while original receipt digest binds the
        // full diagnostic evidence retained by Actions and the operator.
        gates.insert(name.clone(), gate_from_receipt(name, &bytes, value, c)?);
    }
    let proof = Proof {
        schema_version: 1,
        version: c.version.clone(),
        source_commit: c.source_commit.clone(),
        source_tree: c.source_tree.clone(),
        index_sha256: c.index_sha256.clone(),
        publication_run: run.into(),
        gates,
        post_promotion_npx: None,
    };
    proof.validate(c)?;
    Ok(proof)
}
fn proof_body(old: &str, proof: &Proof) -> Result<String> {
    let raw = serde_json::to_string(proof)?;
    ensure!(raw.len() <= 40000, "Compact public proof exceeds release-body budget");
    let replacement = format!(
        "Public verification: passed. Receipt SHA256 `{}`.\n{PROOF_OPEN}{raw}{PROOF_CLOSE}",
        digest(raw.as_bytes())
    );
    if old.contains(PROOF_OPEN) || old.contains(PROOF_CLOSE) {
        let previous = block(old, PROOF_OPEN, PROOF_CLOSE)?;
        let (prefix, rest) = old.split_once(PROOF_OPEN).unwrap();
        let (_, suffix) = rest.split_once(PROOF_CLOSE).unwrap();
        let owned = format!(
            "Public verification: passed. Receipt SHA256 `{}`.\n",
            digest(previous.as_bytes())
        );
        let prefix = prefix.strip_suffix(&owned)
            .filter(|base| base.is_empty() || base.ends_with('\n'))
            .unwrap_or(prefix);
        Ok(format!("{prefix}{replacement}{suffix}"))
    } else {
        let separator = if old.ends_with('\n') { "" } else { "\n" };
        Ok(format!("{old}{separator}{replacement}\n"))
    }
}
fn record_proof(
    remote: &mut impl Remote,
    set: &FrozenSet,
    c: &Contract,
    proof: &Proof,
) -> Result<()> {
    c.check_index(set)?;
    proof.validate(c)?;
    validate_native_upgrade(
        &proof.gates["readme"].summary,
        c,
        Some(&set.index.release_identity),
    )?;
    let release = remote
        .release(&c.tag())?
        .context("Public release missing")?;
    ensure!(
        !release.draft && release.immutable && release.contract()? == *c,
        "Proof requires the original immutable public release"
    );
    check_remote_assets(remote, &release, set, true)?;
    check_npm(remote, set, true)?;
    let old = release.body.as_deref().context("Release body missing")?;
    let body = proof_body(old, proof)?;
    remote.edit_release(&release, &body, None)?;
    let saved = remote
        .release(&c.tag())?
        .context("Release disappeared after saving proof")?;
    ensure!(
        saved.contract()? == *c && saved.body.as_deref() == Some(body.as_str()),
        "Durable proof write did not persist"
    );
    Ok(())
}
fn record_latest(remote: &mut impl Remote, set: &FrozenSet, c: &Contract, bytes: &[u8]) -> Result<()> {
    let release = remote.release(&c.tag())?.context("Public release missing")?;
    ensure!(release.contract()? == *c, "Durable release contract differs");
    let selectors = remote.selectors()?;
    ensure!(
        selectors.environment.as_deref() == Some(&c.version)
            && selectors.cli.as_deref() == Some(&c.version)
            && selectors.github.as_deref() == Some(&c.tag()),
        "Unqualified npx proof requires completed promotion"
    );
    let mut proof: Proof = serde_json::from_str(block(
        release.body.as_deref().unwrap_or_default(), PROOF_OPEN, PROOF_CLOSE,
    )?)?;
    proof.post_promotion_npx = Some(gate_from_receipt(
        "npx-latest", bytes, serde_json::from_slice(bytes)?, c,
    )?);
    record_proof(remote, set, c, &proof)
}
fn promote(remote: &mut impl Remote, set: &FrozenSet, c: &Contract) -> Result<()> {
    c.check_index(set)?;
    guard_npm_predecessors(&c.previous, &c.version)?;
    check_source(remote, c)?;
    let release = remote
        .release(&c.tag())?
        .context("Public release missing")?;
    ensure!(
        !release.draft && release.immutable && release.contract()? == *c,
        "Promotion requires the original immutable public release"
    );
    let proof: Proof = serde_json::from_str(block(
        release.body.as_deref().unwrap_or_default(),
        PROOF_OPEN,
        PROOF_CLOSE,
    )?)?;
    proof.validate(c)?;
    validate_native_upgrade(
        &proof.gates["readme"].summary,
        c,
        Some(&set.index.release_identity),
    )?;
    check_remote_assets(remote, &release, set, true)?;
    check_npm(remote, set, true)?;
    let mut current = remote.selectors()?;
    guard_selectors(&current, c)?;
    let mut acknowledged = BTreeMap::new();
    for (name, environment) in [(ENVIRONMENT, true), (CLI, false)] {
        let pointer = if environment {
            &current.environment
        } else {
            &current.cli
        };
        if pointer.as_deref() != Some(&c.version) {
            let predecessor = pointer.clone().context("Npm latest predecessor missing")?;
            remote.npm_latest(name, &c.version)?;
            acknowledged.insert(name.into(), predecessor);
            current = remote.selectors_after_latest(c, &acknowledged, Some(name))?;
        }
    }
    guard_selectors(&current, c)?;
    if release.prerelease || current.github.as_deref() != Some(&c.tag()) {
        remote.edit_release(
            &release,
            release.body.as_deref().unwrap_or_default(),
            Some(true),
        )?;
    }
    // GitHub's final read also includes npm selectors. Once npm acknowledged a
    // write, a stale predecessor at this boundary uses the same visibility
    // budget rather than turning a successful promotion into a false failure.
    let after = if acknowledged.is_empty() {
        remote.selectors()?
    } else {
        remote.selectors_after_latest(c, &acknowledged, None)?
    };
    guard_selectors(&after, c)?;
    ensure!(
        after.environment.as_deref() == Some(&c.version)
            && after.cli.as_deref() == Some(&c.version)
            && after.github.as_deref() == Some(&c.tag()),
        "Selectors did not converge to the same frozen target"
    );
    Ok(())
}

struct Args {
    command: String,
    values: BTreeMap<String, String>,
}
impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let command = args.next().context("Usage: devkit-release inspect|stage|publish|record-proof|promote|hydrate --root PATH --version X.Y.Z --source-commit SHA --source-tree SHA --previous-env STABLE_VERSION --previous-cli STABLE_VERSION --previous-github vVERSION|none [--build-set PATH] [--output PATH]")?;
        let mut values = BTreeMap::new();
        let allowed = [
            "--root",
            "--version",
            "--source-commit",
            "--source-tree",
            "--previous-env",
            "--previous-cli",
            "--previous-github",
            "--build-set",
            "--output",
            "--publication-run",
            "--public-020",
            "--public-021",
            "--npx",
            "--performance",
            "--transport",
            "--readme",
            "--url",
            "--max-bytes",
        ];
        while let Some(key) = args.next() {
            ensure!(
                allowed.contains(&key.as_str()) && !values.contains_key(&key),
                "Unknown or repeated option: {key}"
            );
            let value = args.next().context("Missing option value")?;
            ensure!(!value.starts_with("--"), "Missing option value: {key}");
            values.insert(key, value);
        }
        Ok(Self { command, values })
    }
    fn get(&self, key: &str) -> Result<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .with_context(|| format!("Missing {key}"))
    }
    fn previous(&self) -> Result<Selectors> {
        let value = |key| -> Result<Option<String>> {
            Ok(match self.get(key)? {
                "none" => None,
                value => Some(value.into()),
            })
        };
        Ok(Selectors {
            environment: value("--previous-env")?,
            cli: value("--previous-cli")?,
            github: value("--previous-github")?,
        })
    }
    fn check_contract(&self, c: &Contract) -> Result<()> {
        ensure!(
            c.version == self.get("--version")?
                && c.source_commit == self.get("--source-commit")?
                && c.source_tree == self.get("--source-tree")?
                && c.previous == self.previous()?,
            "Release inputs differ from durable contract"
        );
        Ok(())
    }
}
pub(super) fn run() -> Result<()> {
    run_args(Args::parse()?)
}
fn run_args(args: Args) -> Result<()> {
    let runner = ProcessRunner::new()?;
    if args.command == "internal-fetch" {
        return devkit::download::fetch(
            args.get("--url")?,
            Path::new(args.get("--output")?),
            args.get("--max-bytes")?.parse()?,
            &runner,
        );
    }
    let root = Path::new(args.get("--root")?).canonicalize()?;
    stable(args.get("--version")?)?;
    assembler::verify_source_checkout(
        &root,
        args.get("--source-commit")?,
        args.get("--source-tree")?,
        &runner,
    )?;
    let mut remote = provider::GhNpm::new(runner)?;
    if args.command == "inspect" {
        remote.main_contains(args.get("--source-commit")?, args.get("--source-tree")?)?;
        if let Some(release) = remote.release(&format!("v{}", args.get("--version")?))? {
            let c = release.contract()?;
            args.check_contract(&c)?;
            check_source(&mut remote, &c)?;
            let output = Path::new(args.get("--output")?);
            ensure!(!output.exists(), "Restore output must be new");
            fs::create_dir(output)?;
            for asset in &release.assets {
                ensure!(
                    asset.name == "release-index.json"
                        || (asset.name != "."
                            && asset.name != ".."
                            && asset
                                .name
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
                    "Unsafe remote asset name"
                );
                remote.download_asset(&release, asset, &output.join(&asset.name))?;
            }
            let complete = if output.join("release-index.json").exists() {
                ensure!(
                    digest(&read_bounded(
                        &output.join("release-index.json"),
                        4 * 1024 * 1024
                    )?) == c.index_sha256,
                    "Remote index differs from original contract"
                );
                let index: ReleaseIndex = files::read_json(&output.join("release-index.json"))?;
                index.validate()?;
                index.files.keys().all(|name| output.join(name).exists())
            } else {
                false
            };
            if complete {
                let set = FrozenSet::load(output)?;
                c.check_index(&set)?;
                check_remote_assets(&mut remote, &release, &set, true)?;
                check_npm(&mut remote, &set, !release.draft)?;
                set.hydrate()?;
            } else {
                ensure!(
                    release.draft,
                    "Immutable public release is missing frozen bytes"
                );
            }
            println!(
                "{}",
                json!({"state":if complete {"restored"} else {"incomplete"},"contract":c,"originalRunId":c.build_run_id()?,"originalAttempt":c.build_run.rsplit_once('/').context("Original build attempt missing")?.1.parse::<u64>()?})
            );
        } else {
            let previous = args.previous()?;
            guard_npm_predecessors(&previous, args.get("--version")?)?;
            guard_current_selectors(&remote.selectors()?, &previous, args.get("--version")?)?;
            for name in [ENVIRONMENT, CLI] {
                ensure!(
                    remote.npm_version(name, args.get("--version")?)?.is_none(),
                    "Registry version exists without original GitHub bytes; use a new patch"
                );
            }
            println!("{}", json!({"state":"fresh"}));
        }
        return Ok(());
    }
    let set = FrozenSet::load(Path::new(args.get("--build-set")?))?;
    if args.command == "hydrate" {
        ensure!(
            set.index.version == args.get("--version")?
                && set.index.source_commit == args.get("--source-commit")?
                && set.index.source_tree == args.get("--source-tree")?,
            "Frozen build set differs from requested source/version"
        );
        set.hydrate()?;
    } else {
        let c = set.contract(args.previous()?)?;
        args.check_contract(&c)?;
        provider::require_actions(&c.source_commit)?;
        match args.command.as_str() {
            "stage" => stage(&mut remote, &set, &c)?,
            "publish" => publish(&mut remote, &set, &c)?,
            "record-proof" => {
                let paths = BTreeMap::from([
                    (
                        "public-0.2.0".into(),
                        PathBuf::from(args.get("--public-020")?),
                    ),
                    (
                        "public-0.2.1".into(),
                        PathBuf::from(args.get("--public-021")?),
                    ),
                    ("npx-pinned".into(), PathBuf::from(args.get("--npx")?)),
                    (
                        "performance".into(),
                        PathBuf::from(args.get("--performance")?),
                    ),
                    (
                        "transport-cold".into(),
                        PathBuf::from(args.get("--transport")?),
                    ),
                    ("readme".into(), PathBuf::from(args.get("--readme")?)),
                ]);
                let readme: Value =
                    serde_json::from_slice(&read_bounded(&paths["readme"], 16 * 1024 * 1024)?)?;
                let manifest: Value = serde_json::from_slice(&set.asset_bytes("release.json")?)?;
                ensure!(
                    readme["readmeSha256"] == manifest["files"]["README.md"]["sha256"],
                    "README gate did not execute the frozen public README"
                );
                let proof = proof_from_files(&c, args.get("--publication-run")?, &paths)?;
                if let Some(output) = args.values.get("--output") {
                    files::write_json(Path::new(output), &proof)?;
                }
                record_proof(&mut remote, &set, &c, &proof)?;
            }
            "promote" => promote(&mut remote, &set, &c)?,
            "record-latest" => {
                let bytes = read_bounded(Path::new(args.get("--npx")?), 16 * 1024 * 1024)?;
                record_latest(&mut remote, &set, &c, &bytes)?;
            }
            _ => bail!("Unknown release command: {}", args.command),
        }
    }
    println!(
        "{}",
        json!({"ok":true,"command":args.command,"indexSha256":set.index_sha256,"version":set.index.version})
    );
    Ok(())
}
