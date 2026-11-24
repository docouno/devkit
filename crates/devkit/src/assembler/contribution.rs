//! Native producers bind executable bytes to source attribution and target-specific notices.
//! The assembler verifies the sealed inputs without executing their binaries.
use super::*;
use crate::delivery::verify_executable;
use crate::distribution::{BuildInfo, REQUIRED_PUBLISHABLE_TARGETS, Target};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Producer {
    schema_version: u32,
    target: String,
    binary: String,
    binary_sha256: String,
    binary_size: u64,
    build_info: BuildInfo,
    #[serde(default)]
    verified: bool,
    notice_set_sha256: String,
    build_run: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    host_env: BTreeMap<String, String>,
}

pub struct ProduceOptions {
    pub root: PathBuf,
    pub engine: PathBuf,
    pub notices: PathBuf,
    pub notice_set_sha256: String,
    pub build_run: String,
    pub output: PathBuf,
    pub host_env: Option<PathBuf>,
    pub verified: bool,
}

pub struct TargetInput {
    pub target: Target,
    pub directory: PathBuf,
    pub sha256: String,
}

impl std::str::FromStr for TargetInput {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let (target, rest) = value
            .split_once(':')
            .context("Expected --target-input <triple>:<directory>:<sha256>")?;
        let (directory, sha256) = rest
            .rsplit_once(':')
            .context("Missing target input digest")?;
        ensure!(
            !directory.is_empty() && SHA256_RE.is_match(sha256),
            "Invalid target input directory or SHA256"
        );
        Ok(Self {
            target: Target::parse(target)?,
            directory: project_root(Path::new(directory))?,
            sha256: sha256.to_owned(),
        })
    }
}

fn actions_attempt(run: &str) -> Result<Option<(&str, u64)>> {
    let Some(path) = run.strip_prefix("https://github.com/") else {
        return Ok(None);
    };
    let Some((repo, suffix)) = path.split_once("/actions/runs/") else {
        return Ok(None);
    };
    let (id, attempt) = suffix
        .split_once("/attempts/")
        .context("Missing Actions attempt")?;
    let prefix = run.strip_suffix(attempt).expect("Parsed attempt suffix");
    let (owner, name) = repo.split_once('/').context("Missing Actions repository")?;
    ensure!(
        [owner, name].iter().all(|s| !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
            && id.bytes().all(|b| b.is_ascii_digit())
            && id.parse::<u64>()? > 0
            && attempt.bytes().all(|b| b.is_ascii_digit()),
        "Invalid Actions run"
    );
    let n = attempt.parse::<u64>()?;
    ensure!(n > 0, "Invalid Actions attempt");
    Ok(Some((prefix, n)))
}

fn compatible_build_run(producer: &str, assembler: &str) -> Result<bool> {
    Ok(
        match (actions_attempt(producer)?, actions_attempt(assembler)?) {
            (Some((a, first)), Some((b, now))) => a == b && first <= now,
            (None, None) => !producer.is_empty() && producer == assembler,
            _ => false,
        },
    )
}

pub fn contribute(options: &ProduceOptions, runner: &ProcessRunner) -> Result<Value> {
    let root = project_root(&options.root)?;
    let engine = project_root(&options.engine)?;
    let notices = project_root(&options.notices)?;
    let output = project_root(&options.output)?;
    ensure!(
        !exists(&output)? && !output.starts_with(&notices) && !notices.starts_with(&output),
        "Contribution requires a new output outside its notice bundle"
    );
    let stage = tempfile::Builder::new()
        .prefix(".devkit-target-")
        .tempdir_in(output.parent().context("Contribution output parent")?)?;
    let target = Target::host()?;
    let binary = target.asset();
    let directory = stage.path().join("contribution");
    let (bytes, id) = read_regular_file(&engine, 128 * 1024 * 1024, "Producer executable")?
        .context("Missing producer executable")?;
    let binary_size = bytes.len() as u64;
    write_atomic(&directory.join(binary), &bytes, 0o755)?;
    drop(bytes);
    verify_executable(&directory.join(binary), &id.sha256, target, true)?;
    let info = engine_build_info(&directory.join(binary), runner)?;
    ensure!(
        info.target == target.triple(),
        "Native producer target differs"
    );
    if options.verified {
        ensure!(
            info.source_verified,
            "Verified contribution requires a frozen source build"
        );
    }
    copy_tree(&notices, &directory.join("notices"), &|_| false, false)?;
    let settings = pins(&root)?;
    let receipt = verify_notice_set(
        &directory.join("notices"),
        &root,
        target.triple(),
        &settings["DEVKIT_RUST_VERSION"],
        &options.notice_set_sha256,
    )?;
    ensure!(
        receipt.generator == info,
        "Notices were collected by a different native build"
    );
    ensure!(
        !options.build_run.is_empty(),
        "Missing producer build-run identity"
    );
    let host_env = match &options.host_env {
        Some(path) => {
            let (bytes, _) =
                read_regular_file(&project_root(path)?, 8192, "Producer host environment")?
                    .context("Missing host environment")?;
            serde_json::from_slice(&bytes)?
        }
        None => BTreeMap::new(),
    };
    let receipt = Producer {
        schema_version: 1,
        target: target.triple().into(),
        binary: binary.into(),
        binary_sha256: id.sha256,
        binary_size,
        build_info: info,
        verified: options.verified,
        notice_set_sha256: options.notice_set_sha256.clone(),
        build_run: options.build_run.clone(),
        host_env,
    };
    let bytes = json_bytes(&receipt)?;
    ensure!(bytes.len() <= 64 * 1024, "Producer receipt too large");
    write_atomic(&directory.join("producer.json"), &bytes, 0o644)?;
    if options.verified {
        verify_source_checkout(
            &root,
            receipt
                .build_info
                .source_commit
                .as_deref()
                .context("Missing producer commit")?,
            receipt
                .build_info
                .source_tree
                .as_deref()
                .context("Missing producer tree")?,
            runner,
        )?;
    }
    runner.check_cancelled()?;
    publish_new_directory(&directory, &output)?;
    Ok(json!({"target":target.triple(),"contributionSha256":digest(bytes)}))
}

pub(super) struct Contribution<'a> {
    input: &'a TargetInput,
    producer: Producer,
}

impl<'a> Contribution<'a> {
    fn read(
        input: &'a TargetInput,
        options: &AssemblyOptions,
        root: &Path,
        rust_version: &str,
    ) -> Result<Self> {
        ensure!(
            SHA256_RE.is_match(&input.sha256),
            "Invalid producer contribution digest"
        );
        let (bytes, id) = read_regular_file(
            &safe_path(&input.directory, "producer.json")?,
            64 * 1024,
            "Producer contribution",
        )?
        .context("Missing producer contribution")?;
        ensure!(
            id.sha256 == input.sha256,
            "Contribution differs from the producer digest"
        );
        let p: Producer = serde_json::from_slice(&bytes)?;
        let t = input.target;
        ensure!(
            p.schema_version == 1
                && p.target == t.triple()
                && p.binary == t.asset()
                && p.build_info.schema_version == 1
                && p.build_info.target == p.target
                && p.binary_size > 0
                && p.binary_size <= 128 * 1024 * 1024,
            "Invalid producer executable identity for {}",
            t.triple()
        );
        if options.mode != AssemblyMode::Local {
            ensure!(
                p.verified && p.build_info.source_verified,
                "Unverified contribution for {}",
                t.triple()
            );
            ensure!(
                p.build_info.source_commit.as_deref() == Some(&options.source_commit)
                    && p.build_info.source_tree.as_deref() == Some(&options.source_tree)
                    && compatible_build_run(&p.build_run, &options.build_run)?,
                "Producer source or build run differs for {}",
                t.triple()
            );
        }
        let binary = safe_path(&input.directory, t.asset())?;
        ensure!(
            fs::symlink_metadata(&binary)?.len() == p.binary_size,
            "Producer executable size differs"
        );
        verify_executable(&binary, &p.binary_sha256, t, true)?;
        let notices = verify_notice_set(
            &safe_path(&input.directory, "notices")?,
            root,
            t.triple(),
            rust_version,
            &p.notice_set_sha256,
        )?;
        ensure!(
            notices.generator == p.build_info,
            "Notice collector source/build identity differs from the contributed engine"
        );
        let members: BTreeSet<_> = sorted_entries(&input.directory)?
            .into_iter()
            .map(|e| e.file_name())
            .collect();
        ensure!(
            members
                == [t.asset(), "notices", "producer.json"]
                    .into_iter()
                    .map(std::ffi::OsString::from)
                    .collect(),
            "Unexpected contribution contents"
        );
        Ok(Self { input, producer: p })
    }

    pub(super) fn import(&self, payload: &Path, root: &Path, rust_version: &str) -> Result<()> {
        let t = self.input.target;
        let (bytes, id) = read_regular_file(
            &safe_path(&self.input.directory, t.asset())?,
            self.producer.binary_size,
            "Contributed executable",
        )?
        .context("Missing executable")?;
        ensure!(
            id.sha256 == self.sha256(),
            "Contributed executable changed during import"
        );
        write_atomic(&payload.join(t.payload_name()), &bytes, 0o755)?;
        let notices = safe_path(&payload.join("notices"), t.triple())?;
        copy_tree(
            &self.input.directory.join("notices"),
            &notices,
            &|_| false,
            false,
        )?;
        verify_notice_set(
            &notices,
            root,
            t.triple(),
            rust_version,
            &self.producer.notice_set_sha256,
        )?;
        Ok(())
    }

    pub(super) fn sha256(&self) -> &str {
        &self.producer.binary_sha256
    }
    pub(super) fn version(&self) -> &str {
        &self.producer.build_info.version
    }
    pub(super) fn target(&self) -> Target {
        self.input.target
    }
}

pub(super) fn read_inputs<'a>(
    options: &'a AssemblyOptions,
    root: &Path,
    rust_version: &str,
) -> Result<Vec<Contribution<'a>>> {
    let mut contributions = Vec::new();
    let mut seen = BTreeSet::new();
    for input in &options.target_inputs {
        ensure!(
            seen.insert(input.target.triple()),
            "Duplicate contribution for {}",
            input.target.triple()
        );
        contributions.push(Contribution::read(input, options, root, rust_version)?);
    }
    ensure!(
        seen.contains(TARGET),
        "Missing canonical x86_64 contribution"
    );
    if options.mode != AssemblyMode::Local {
        let required = if options.mode == AssemblyMode::Published {
            REQUIRED_PUBLISHABLE_TARGETS
        } else {
            &Target::ALL
        };
        ensure!(
            seen == required.iter().map(|t| t.triple()).collect(),
            "Incomplete or unexpected verified target matrix"
        );
    }
    contributions.sort_by_key(Contribution::target);
    Ok(contributions)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreign_contributions_must_bind_to_the_requested_target_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let target = Target::LinuxArm64;
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        let receipt = json!({
            "schemaVersion":1, "target":target.triple(), "binary":target.asset(),
            "binarySha256":"1".repeat(64), "binarySize":120,
            "verified":true,"buildInfo":{"sourceVerified":true,"schemaVersion":1,"target":target.triple(),"version":"0.6.0","sourceCommit":commit,"sourceTree":tree},
            "noticeSetSha256":"2".repeat(64), "buildRun":"workflow"
        });
        let mut assembler = AssemblyOptions {
            root: tmp.path().into(),
            output: tmp.path().join("set"),
            target_inputs: Vec::new(),
            npm_inputs: tmp.path().join("npm"),
            source_commit: commit,
            source_tree: tree,
            build_run: "workflow".into(),
            mode: AssemblyMode::Published,
        };
        for (path, wrong, diagnostic) in [
            ("/verified", Value::Bool(false), "Unverified"),
            (
                "/buildInfo/sourceVerified",
                Value::Bool(false),
                "Unverified",
            ),
            (
                "/target",
                json!("aarch64-apple-darwin"),
                "executable identity",
            ),
            ("/binary", json!("devkit"), "executable identity"),
            ("/buildInfo/target", json!(TARGET), "executable identity"),
            (
                "/buildInfo/sourceCommit",
                json!("another commit"),
                "Producer source",
            ),
            (
                "/buildInfo/sourceTree",
                json!("another tree"),
                "Producer source",
            ),
            ("/buildRun", json!("another workflow"), "build run differs"),
        ] {
            let mut altered = receipt.clone();
            *altered.pointer_mut(path).unwrap() = wrong;
            let bytes = json_bytes(&altered).unwrap();
            let input = TargetInput {
                directory: tmp.path().into(),
                target,
                sha256: digest(&bytes),
            };
            fs::write(tmp.path().join("producer.json"), bytes).unwrap();
            for mode in [AssemblyMode::Published, AssemblyMode::VerifiedPreview] {
                assembler.mode = mode;
                let err = Contribution::read(&input, &assembler, tmp.path(), "1.99.1")
                    .err()
                    .expect("mismatched receipt");
                assert!(
                    err.to_string().contains(diagnostic),
                    "{mode:?} {path}: {err}"
                );
            }
        }
    }

    #[test]
    fn contributions_from_the_same_run_survive_failed_only_reruns() {
        let original = "https://github.com/acme/devkit/actions/runs/7/attempts/1";
        let retry = "https://github.com/acme/devkit/actions/runs/7/attempts/2";
        assert!(compatible_build_run(original, retry).unwrap());
        assert!(!compatible_build_run(retry, original).unwrap());
        assert!(!compatible_build_run(original, &retry.replace("acme", "other")).unwrap());
        assert!(!compatible_build_run(original, &retry.replace("runs/7", "runs/8")).unwrap());
        assert!(compatible_build_run("local-author", "local-author").unwrap());
        assert!(!compatible_build_run("local-author", original).unwrap());
        assert!(
            compatible_build_run(original, &retry.replace("attempts/2", "attempts/0")).is_err()
        );
    }
}
