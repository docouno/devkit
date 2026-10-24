use super::*;

struct Fixture {
    directory: tempfile::TempDir,
    contract: Contract,
}
impl Fixture {
    fn new() -> Self {
        Self::with_version("0.3.0")
    }
    fn native_update() -> Self {
        let mut fixture = Self::with_version("0.3.1");
        fixture.contract.previous = Selectors {
            environment: Some("0.3.0".into()),
            cli: Some("0.3.0".into()),
            github: Some("v0.3.0".into()),
        };
        fixture
    }
    fn with_version(version: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        for name in [
            "devkit-x86_64-unknown-linux-musl".to_owned(),
            format!("devkit-{version}.tar.gz"),
            "release.json".into(),
            "install.sh".into(),
            "bootstrap.sh".into(),
            "checksums.txt".into(),
            format!("docouno-env-{version}.tgz"),
            format!("docouno-devkit-{version}.tgz"),
        ] {
            let bytes = format!("frozen {name}");
            fs::write(directory.path().join(&name), &bytes).unwrap();
            files.insert(
                name,
                devkit::distribution::AssetIdentity {
                    sha256: digest(bytes.as_bytes()),
                    size: bytes.len() as u64,
                },
            );
        }
        let mut packages = BTreeMap::new();
        for (kind, name, archive) in [
            (
                "environment",
                ENVIRONMENT,
                format!("docouno-env-{version}.tgz"),
            ),
            ("cli", CLI, format!("docouno-devkit-{version}.tgz")),
        ] {
            let bytes = fs::read(directory.path().join(&archive)).unwrap();
            packages.insert(
                kind.into(),
                NpmArtifact {
                    name: name.into(),
                    version: version.into(),
                    archive,
                    integrity: format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes))),
                    shasum: format!("{:x}", Sha1::digest(&bytes)),
                },
            );
        }
        let index = ReleaseIndex {
            schema_version: 1,
            version: version.into(),
            target: devkit::distribution::TARGET.into(),
            source_commit: "a".repeat(40),
            source_tree: "b".repeat(40),
            build_run: "https://github.com/docouno/devkit/actions/runs/123/attempts/1".into(),
            publication_eligible: true,
            release_identity: files["release.json"].sha256.clone(),
            engine: "devkit-x86_64-unknown-linux-musl".into(),
            archive: format!("devkit-{version}.tar.gz"),
            packages,
            files,
        };
        files::write_json(&directory.path().join("release-index.json"), &index).unwrap();
        let contract = FrozenSet::load(directory.path())
            .unwrap()
            .contract(Selectors {
                environment: Some("0.2.1".into()),
                cli: Some("0.2.1".into()),
                github: Some("v0.2.1".into()),
            })
            .unwrap();
        Self {
            directory,
            contract,
        }
    }
    fn set(&self) -> FrozenSet {
        FrozenSet::load(self.directory.path()).unwrap()
    }
    fn remote(&self) -> Fake {
        Fake {
            release: None,
            assets: BTreeMap::new(),
            npm: BTreeMap::new(),
            selectors: self.contract.previous.clone(),
            mutations: Vec::new(),
            fault: None,
            tag: None,
            visibility_lag: 0,
            pending_reads: BTreeMap::new(),
            readiness_pauses: 0,
            corrupt_publish_bytes: false,
            selector_reads: std::collections::VecDeque::new(),
            latest_read_elapsed: BTreeMap::new(),
        }
    }
}

#[test]
fn fork_run_hydrates_exact_bytes_without_acquiring_publication_authority() {
    let fixture = Fixture::new();
    let source = tempfile::tempdir().unwrap();
    let git = |arguments: &[&str]| {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(source.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    for name in [
        "VERSION",
        "LICENSE",
        "README.md",
        "toolchains.env",
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "runtime/file",
        "templates/file",
        "packages/env/file",
    ] {
        files::write_atomic(&source.path().join(name), b"reviewed source\n", 0o644).unwrap();
    }
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
    let commit = git(&["rev-parse", "HEAD"]);
    let tree = git(&["rev-parse", "HEAD^{tree}"]);
    let payload_source = tempfile::tempdir().unwrap();
    let environment = b"fixture environment archive";
    files::write_atomic(
        &payload_source.path().join("npm/env.tgz"),
        environment,
        0o644,
    )
    .unwrap();
    files::write_json(
        &payload_source.path().join("release.json"),
        &devkit::model::Release {
            schema_version: 1,
            version: "0.3.0".into(),
            rust_version: "1.99.1".into(),
            environment: devkit::model::Environment {
                name: ENVIRONMENT.into(),
                version: "0.3.0".into(),
                archive: "npm/env.tgz".into(),
                integrity: format!("sha512-{}", STANDARD.encode(Sha512::digest(environment))),
            },
            files: files::file_map(payload_source.path()).unwrap(),
        },
    )
    .unwrap();
    let set_root = fixture.directory.path();
    fs::copy(
        payload_source.path().join("release.json"),
        set_root.join("release.json"),
    )
    .unwrap();
    fs::remove_file(set_root.join("devkit-0.3.0.tar.gz")).unwrap();
    archive::create_archive(
        payload_source.path(),
        "devkit-0.3.0",
        &set_root.join("devkit-0.3.0.tar.gz"),
    )
    .unwrap();
    let mut index: ReleaseIndex = files::read_json(&set_root.join("release-index.json")).unwrap();
    index.source_commit = commit.clone();
    index.source_tree = tree.clone();
    index.build_run = "https://github.com/example/fork/actions/runs/456/attempts/2".into();
    for name in ["release.json", "devkit-0.3.0.tar.gz"] {
        let bytes = fs::read(set_root.join(name)).unwrap();
        index.files.insert(
            name.into(),
            devkit::distribution::AssetIdentity {
                sha256: digest(&bytes),
                size: bytes.len() as u64,
            },
        );
    }
    index.release_identity = index.files["release.json"].sha256.clone();
    files::write_json(&set_root.join("release-index.json"), &index).unwrap();
    let mut args = Args {
        command: "hydrate".into(),
        values: BTreeMap::from([
            ("--root".into(), source.path().to_str().unwrap().into()),
            ("--version".into(), "0.3.0".into()),
            ("--source-commit".into(), commit),
            ("--source-tree".into(), tree),
            ("--build-set".into(), set_root.to_str().unwrap().into()),
        ]),
    };
    run_args(Args {
        command: args.command.clone(),
        values: args.values.clone(),
    })
    .unwrap();
    assert_eq!(
        fs::read(set_root.join("payload/npm/env.tgz")).unwrap(),
        environment
    );
    assert_eq!(
        devkit::release::admit_release(&set_root.join("payload"))
            .unwrap()
            .identity,
        index.release_identity
    );
    for (field, wrong) in [
        ("--version", "0.4.0"),
        ("--source-commit", &"c".repeat(40)),
        ("--source-tree", &"d".repeat(40)),
    ] {
        let mut wrong_args = Args {
            command: args.command.clone(),
            values: args.values.clone(),
        };
        wrong_args.values.insert(field.into(), wrong.into());
        assert!(run_args(wrong_args).is_err(), "Accepted wrong {field}");
    }
    for field in ["commit", "tree"] {
        let mut wrong_index = index.clone();
        if field == "commit" {
            wrong_index.source_commit = "c".repeat(40);
        } else {
            wrong_index.source_tree = "d".repeat(40);
        }
        files::write_json(&set_root.join("release-index.json"), &wrong_index).unwrap();
        assert!(
            run_args(Args {
                command: args.command.clone(),
                values: args.values.clone()
            })
            .unwrap_err()
            .to_string()
            .contains("Frozen build set differs from requested source/version")
        );
    }
    files::write_json(&set_root.join("release-index.json"), &index).unwrap();
    args.command = "stage".into();
    for key in ["--previous-env", "--previous-cli", "--previous-github"] {
        args.values.insert(key.into(), "none".into());
    }
    assert!(
        run_args(args)
            .unwrap_err()
            .to_string()
            .contains("original GitHub Actions attempt")
    );
    fs::write(set_root.join("install.sh"), "changed frozen bytes").unwrap();
    assert!(FrozenSet::load(set_root).is_err());
}

struct Fake {
    release: Option<RemoteRelease>,
    assets: BTreeMap<String, Vec<u8>>,
    npm: BTreeMap<String, PublishedNpm>,
    selectors: Selectors,
    mutations: Vec<String>,
    fault: Option<String>,
    tag: Option<(String, String)>,
    visibility_lag: usize,
    pending_reads: BTreeMap<String, usize>,
    readiness_pauses: usize,
    corrupt_publish_bytes: bool,
    selector_reads: std::collections::VecDeque<Result<Selectors>>,
    latest_read_elapsed: BTreeMap<String, std::time::Duration>,
}
impl Fake {
    fn hit(&mut self, label: &str) -> Result<()> {
        self.mutations.push(label.into());
        if self.fault.as_deref() == Some(label) {
            self.fault = None;
            bail!("Response lost after successful {label}");
        }
        Ok(())
    }
    fn proof(&mut self, c: &Contract) {
        let mut gates = BTreeMap::new();
        for name in [
            "public-0.2.0",
            "public-0.2.1",
            "npx-pinned",
            "performance",
            "transport-cold",
            "readme",
        ] {
            let mut summary = json!({"schemaVersion":1,"success":true,"version":c.version,"sourceCommit":c.source_commit,"sourceTree":c.source_tree,"indexSha256":c.index_sha256});
            if let Some(old) = name.strip_prefix("public-") {
                summary = json!({"schemaVersion":3,"success":true,"version":old,"nextVersion":c.version,"expectedIndexSha256":c.index_sha256,"authenticationUsedForDownloads":false,"defaultReleaseBaseTested":true,"consumerSuccess":true,"identities":[{"sourceCommit":c.source_commit,"sourceTree":c.source_tree,"indexSha256":c.index_sha256}]});
            }
            if name == "npx-pinned" {
                summary["selectorMode"] = json!("pinned");
                summary["ignoreScripts"] = json!(true);
                summary["authenticationUsed"] = json!(false);
                summary["umasks"] = json!(["022", "077"]);
            }
            if name == "performance" {
                summary["measurements"] = json!([{"case":"warm","samples":30}]);
            }
            if name == "transport-cold" {
                summary["authenticationUsed"] = json!(false);
                summary["measurements"] = json!([{"transport":"npx","cacheState":"empty","seconds":2.0},{"transport":"curl","cacheState":"empty","seconds":2.0,"hostLanguageSDKAttempted":false}]);
            }
            if name == "readme" {
                for key in [
                    "publicNpx",
                    "barePtyCancelled",
                    "standaloneWebStatePreservingHmr",
                ] {
                    summary[key] = json!(true);
                }
                summary["authenticationUsed"] = json!(false);
                summary["hostLanguageSDKAttemptedForNativeCommands"] = json!(false);
                summary["readmeSha256"] = json!("d".repeat(64));
                if native_upgrade_from(c).is_some() {
                    summary["nativeUpgrade"] =
                        native_upgrade(c, &digest(&self.assets["release.json"]));
                }
            }
            gates.insert(
                name.into(),
                Gate {
                    sha256: "c".repeat(64),
                    bytes: 100,
                    summary,
                },
            );
        }
        let proof = Proof {
            schema_version: 1,
            version: c.version.clone(),
            source_commit: c.source_commit.clone(),
            source_tree: c.source_tree.clone(),
            index_sha256: c.index_sha256.clone(),
            publication_run: c.build_run.clone(),
            gates,
            post_promotion_npx: None,
        };
        proof.validate(c).unwrap();
        let release = self.release.as_mut().unwrap();
        release.body = Some(format!(
            "{}{PROOF_OPEN}{}{PROOF_CLOSE}",
            c.body().unwrap(),
            serde_json::to_string(&proof).unwrap()
        ));
    }
}
impl Remote for Fake {
    fn release(&mut self, _tag: &str) -> Result<Option<RemoteRelease>> {
        let mut release = self.release.clone();
        if let Some(release) = &mut release {
            release.assets = self
                .assets
                .iter()
                .enumerate()
                .map(|(i, (name, bytes))| RemoteAsset {
                    id: i as u64 + 1,
                    name: name.clone(),
                    size: bytes.len() as u64,
                    digest: Some(format!("sha256:{}", digest(bytes))),
                    state: "uploaded".into(),
                })
                .collect();
        }
        Ok(release)
    }
    fn create_draft(&mut self, c: &Contract) -> Result<RemoteRelease> {
        let release = RemoteRelease {
            id: 1,
            tag_name: c.tag(),
            target_commitish: c.source_commit.clone(),
            draft: true,
            prerelease: true,
            immutable: false,
            body: Some(c.body()?),
            assets: Vec::new(),
        };
        self.release = Some(release.clone());
        self.hit("draft")?;
        Ok(release)
    }
    fn download_asset(
        &mut self,
        _release: &RemoteRelease,
        asset: &RemoteAsset,
        path: &Path,
    ) -> Result<()> {
        fs::write(path, &self.assets[&asset.name])?;
        Ok(())
    }
    fn upload_asset(&mut self, _release: &RemoteRelease, path: &Path) -> Result<()> {
        let name = path.file_name().unwrap().to_str().unwrap();
        ensure!(!self.assets.contains_key(name), "Would overwrite an asset");
        self.assets.insert(name.into(), fs::read(path)?);
        self.hit(&format!("upload:{name}"))
    }
    fn npm_version(&mut self, name: &str, _version: &str) -> Result<Option<PublishedNpm>> {
        if let Some(remaining) = self.pending_reads.get_mut(name)
            && *remaining > 0
        {
            *remaining -= 1;
            return Ok(None);
        }
        Ok(self.npm.get(name).map(|n| PublishedNpm {
            integrity: n.integrity.clone(),
            shasum: n.shasum.clone(),
            bytes: n.bytes.clone(),
        }))
    }
    fn npm_after_publish(&mut self, name: &str, version: &str) -> Result<PublishedNpm> {
        let elapsed = std::cell::Cell::new(std::time::Duration::ZERO);
        let pauses = std::cell::Cell::new(0);
        let result = readiness::wait(
            name,
            version,
            || elapsed.get(),
            |_remaining| self.npm_version(name, version),
            |duration| {
                pauses.set(pauses.get() + 1);
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        );
        self.readiness_pauses += pauses.get();
        result
    }
    fn publish_npm(&mut self, path: &Path, tag: &str) -> Result<()> {
        ensure!(
            self.assets.len() == 9 && self.assets.contains_key("release-index.json"),
            "Registry write before complete durable GitHub staging"
        );
        ensure!(
            tag == format!(
                "candidate-{}",
                self.release.as_ref().unwrap().contract()?.version
            ),
            "Would expose latest before public proof"
        );
        let name = if path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("docouno-env")
        {
            ENVIRONMENT
        } else {
            CLI
        };
        ensure!(!self.npm.contains_key(name), "Would republish a version");
        let bytes = fs::read(path)?;
        self.npm.insert(
            name.into(),
            PublishedNpm {
                integrity: format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes))),
                shasum: format!("{:x}", Sha1::digest(&bytes)),
                bytes,
            },
        );
        if self.corrupt_publish_bytes {
            self.npm.get_mut(name).unwrap().bytes.push(0);
        }
        self.pending_reads.insert(name.into(), self.visibility_lag);
        self.hit(&format!("npm:{name}"))
    }
    fn selectors(&mut self) -> Result<Selectors> {
        self.selector_reads
            .pop_front()
            .unwrap_or_else(|| Ok(self.selectors.clone()))
    }
    fn selectors_after_latest(
        &mut self,
        contract: &Contract,
        acknowledged: &BTreeMap<String, String>,
        pending: Option<&str>,
    ) -> Result<Selectors> {
        let started = self.latest_read_elapsed.clone();
        let elapsed = std::cell::Cell::new(std::time::Duration::ZERO);
        let pauses = std::cell::Cell::new(0);
        let result = readiness::wait_latest(
            contract,
            acknowledged,
            pending,
            |name| started[name] + elapsed.get(),
            |_, _| self.selectors(),
            |duration| {
                pauses.set(pauses.get() + 1);
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        );
        for duration in self.latest_read_elapsed.values_mut() {
            *duration += elapsed.get();
        }
        self.readiness_pauses += pauses.get();
        result
    }
    fn npm_latest(&mut self, name: &str, version: &str) -> Result<()> {
        if name == ENVIRONMENT {
            self.selectors.environment = Some(version.into());
        } else {
            self.selectors.cli = Some(version.into());
        }
        self.hit(&format!("latest:{name}"))?;
        self.latest_read_elapsed
            .entry(name.into())
            .or_insert(std::time::Duration::ZERO);
        Ok(())
    }
    fn edit_release(
        &mut self,
        _release: &RemoteRelease,
        body: &str,
        channel: Option<bool>,
    ) -> Result<()> {
        let release = self.release.as_mut().unwrap();
        release.body = Some(body.into());
        let label = if let Some(stable) = channel {
            release.draft = false;
            release.immutable = true;
            release.prerelease = !stable;
            let c = Contract::from_body(body)?;
            self.tag = Some((c.source_commit, c.source_tree));
            if stable {
                self.selectors.github = Some(release.tag_name.clone());
                "latest:github"
            } else {
                "public"
            }
        } else {
            "proof"
        };
        self.hit(label)
    }
    fn tag_source(&mut self, _tag: &str) -> Result<Option<(String, String)>> {
        Ok(self.tag.clone())
    }
    fn main_contains(&mut self, commit: &str, tree: &str) -> Result<()> {
        ensure!(
            commit == "a".repeat(40) && tree == "b".repeat(40),
            "Source not on reviewed main"
        );
        Ok(())
    }
}

#[test]
fn lost_upload_response_resumes_without_overwriting_any_original_bytes() {
    let f = Fixture::new();
    let mut remote = f.remote();
    remote.fault = Some("upload:checksums.txt".into());
    assert!(stage(&mut remote, &f.set(), &f.contract).is_err());
    let first = remote.assets["checksums.txt"].clone();
    stage(&mut remote, &f.set(), &f.contract).unwrap();
    assert_eq!(remote.assets["checksums.txt"], first);
    assert_eq!(
        remote
            .mutations
            .iter()
            .filter(|s| *s == "upload:checksums.txt")
            .count(),
        1
    );
    assert_eq!(remote.assets.len(), 9);
}

#[test]
fn existing_incomplete_draft_checks_selectors_before_any_asset_mutation() {
    for unexpected in ["0.4.0", "0.2.0"] {
        let fixture = Fixture::new();
        let set = fixture.set();
        let mut remote = fixture.remote();
        stage(&mut remote, &set, &fixture.contract).unwrap();
        remote.assets.remove("checksums.txt");
        remote.selectors.environment = Some(unexpected.into());
        let before = remote.mutations.len();
        let assets_before = remote.assets.clone();
        assert!(stage(&mut remote, &set, &fixture.contract).is_err());
        assert_eq!(remote.mutations.len(), before);
        assert_eq!(remote.assets, assets_before);
    }
}

#[test]
fn publication_waits_for_exact_version_without_repeating_the_write() {
    let fixture = Fixture::new();
    let set = fixture.set();
    let mut remote = fixture.remote();
    stage(&mut remote, &set, &fixture.contract).unwrap();
    remote.visibility_lag = 2;
    publish(&mut remote, &set, &fixture.contract).unwrap();
    for name in [ENVIRONMENT, CLI] {
        assert_eq!(
            remote
                .mutations
                .iter()
                .filter(|op| *op == &format!("npm:{name}"))
                .count(),
            1
        );
    }
    assert!(remote.release.as_ref().unwrap().immutable);
    assert_eq!(remote.selectors, fixture.contract.previous);
    assert_eq!(remote.readiness_pauses, 4);
}

#[test]
fn publication_timeout_never_writes_another_package_or_public_release() {
    let fixture = Fixture::new();
    let set = fixture.set();
    let mut remote = fixture.remote();
    stage(&mut remote, &set, &fixture.contract).unwrap();
    let before = remote.mutations.len();
    remote.visibility_lag = usize::MAX;
    let error = publish(&mut remote, &set, &fixture.contract).unwrap_err();
    assert!(format!("{error:#}").contains("within 360s"));
    assert_eq!(remote.mutations[before..], ["npm:@docouno/env"]);
    assert!(remote.release.as_ref().unwrap().draft);
    assert_eq!(remote.selectors, fixture.contract.previous);
    assert_eq!(remote.readiness_pauses, 36);
}

#[test]
fn first_visible_mismatching_bytes_stop_before_any_more_mutations() {
    let fixture = Fixture::new();
    let set = fixture.set();
    let mut remote = fixture.remote();
    stage(&mut remote, &set, &fixture.contract).unwrap();
    let before = remote.mutations.len();
    remote.corrupt_publish_bytes = true;
    let error = publish(&mut remote, &set, &fixture.contract).unwrap_err();
    assert!(error.to_string().contains("bytes/SRI differ"));
    assert_eq!(remote.mutations[before..], ["npm:@docouno/env"]);
    assert_eq!(remote.readiness_pauses, 0);
    assert!(remote.release.as_ref().unwrap().draft);
}

#[test]
fn new_package_and_placeholder_baselines_refuse_before_any_write() {
    for previous in [None, Some("0.0.0-stage"), Some("0.3.0"), Some("0.4.0")] {
        for environment in [false, true] {
            let mut fixture = Fixture::new();
            if environment {
                fixture.contract.previous.environment = previous.map(str::to_owned);
            } else {
                fixture.contract.previous.cli = previous.map(str::to_owned);
            }
            let set = fixture.set();
            let mut remote = fixture.remote();
            assert!(
                stage(&mut remote, &set, &fixture.contract).is_err(),
                "Accepted {previous:?} as a new release baseline"
            );
            assert!(remote.mutations.is_empty());
        }
    }
}

#[test]
fn publication_checks_predecessors_even_with_a_complete_legacy_draft() {
    for previous in [None, Some("0.0.0-stage"), Some("0.3.0")] {
        for environment in [false, true] {
            let fixture = Fixture::new();
            let set = fixture.set();
            let mut remote = fixture.remote();
            stage(&mut remote, &set, &fixture.contract).unwrap();
            let mut historical = fixture.contract.clone();
            if environment {
                historical.previous.environment = previous.map(str::to_owned);
            } else {
                historical.previous.cli = previous.map(str::to_owned);
            }
            remote.release.as_mut().unwrap().body = Some(historical.body().unwrap());
            remote.selectors = historical.previous.clone();
            let before = remote.mutations.len();
            assert!(publish(&mut remote, &set, &historical).is_err());
            assert_eq!(remote.mutations.len(), before);
            assert!(remote.npm.is_empty());
        }
    }
}

#[test]
fn historical_placeholder_contract_is_still_readable() {
    let fixture = Fixture::new();
    let mut historical = fixture.contract.clone();
    historical.previous.cli = Some("0.0.0-stage".into());
    assert_eq!(
        Contract::from_body(&historical.body().unwrap()).unwrap(),
        historical
    );
    assert!(guard_npm_predecessors(&historical.previous, &historical.version).is_err());
    assert!(guard_npm_predecessors(&fixture.contract.previous, &fixture.contract.version).is_ok());
}

fn native_upgrade(c: &Contract, identity: &str) -> Value {
    json!({
        "fromVersion": c.previous.environment,
        "toVersion": c.version,
        "fromReleaseIdentity": "e".repeat(64),
        "toReleaseIdentity": identity,
        "acceptedOldAfterFreshCliAdd": true,
        "oldEngineVersionVerified": true,
        "newEngineVersionVerified": true,
        "productInputsPreserved": true,
        "previewReadonly": true,
        "checksPassed": true,
        "sameVersionNoop": true,
        "hostSdkAttempts": 0
    })
}

#[test]
fn native_readme_requires_every_bound_upgrade_assertion() {
    let fixture = Fixture::native_update();
    let set = fixture.set();
    let c = &fixture.contract;
    let mut remote = fixture.remote();
    stage(&mut remote, &set, c).unwrap();
    publish(&mut remote, &set, c).unwrap();
    remote.proof(c);
    let proof: Proof = serde_json::from_str(
        block(
            remote.release.as_ref().unwrap().body.as_deref().unwrap(),
            PROOF_OPEN,
            PROOF_CLOSE,
        )
        .unwrap(),
    )
    .unwrap();
    let original = proof.gates["readme"].summary.clone();
    validate_gate("readme", &original, c).unwrap();
    let mut variants = vec![Value::Null, json!({})];
    for (key, value) in [
        ("fromVersion", json!("0.2.1")),
        ("toVersion", json!("0.4.0")),
        ("fromReleaseIdentity", json!("bad")),
        ("toReleaseIdentity", json!("bad")),
        (
            "fromReleaseIdentity",
            original["nativeUpgrade"]["toReleaseIdentity"].clone(),
        ),
        ("hostSdkAttempts", json!(1)),
        ("hostSdkAttempts", json!(false)),
    ] {
        let mut value_copy = original["nativeUpgrade"].clone();
        value_copy[key] = value;
        variants.push(value_copy);
    }
    for key in [
        "acceptedOldAfterFreshCliAdd",
        "oldEngineVersionVerified",
        "newEngineVersionVerified",
        "productInputsPreserved",
        "previewReadonly",
        "checksPassed",
        "sameVersionNoop",
    ] {
        let mut value = original["nativeUpgrade"].clone();
        value[key] = json!(false);
        variants.push(value);
    }
    for native in variants {
        let mut changed = original.clone();
        changed["nativeUpgrade"] = native;
        assert!(validate_gate("readme", &changed, c).is_err());
    }
    let mut raw = original.clone();
    raw["results"] = json!([]);
    let compact = compact_gate("readme", raw).unwrap();
    assert_eq!(compact["nativeUpgrade"], original["nativeUpgrade"]);
    validate_gate("readme", &compact, c).unwrap();
}

#[test]
fn native_upgrade_target_binding_blocks_proof_and_promotion_before_writes() {
    for invalid in [
        Value::Null,
        native_upgrade(&Fixture::native_update().contract, &"f".repeat(64)),
    ] {
        let fixture = Fixture::native_update();
        let set = fixture.set();
        let c = &fixture.contract;
        let mut remote = fixture.remote();
        stage(&mut remote, &set, c).unwrap();
        publish(&mut remote, &set, c).unwrap();
        remote.proof(c);
        let mut proof: Proof = serde_json::from_str(
            block(
                remote.release.as_ref().unwrap().body.as_deref().unwrap(),
                PROOF_OPEN,
                PROOF_CLOSE,
            )
            .unwrap(),
        )
        .unwrap();
        proof.gates.get_mut("readme").unwrap().summary["nativeUpgrade"] = invalid;
        let before = remote.mutations.len();
        assert!(record_proof(&mut remote, &set, c, &proof).is_err());
        assert_eq!(remote.mutations.len(), before);
        remote.release.as_mut().unwrap().body = Some(format!(
            "{}{PROOF_OPEN}{}{PROOF_CLOSE}",
            c.body().unwrap(),
            serde_json::to_string(&proof).unwrap()
        ));
        assert!(promote(&mut remote, &set, c).is_err());
        assert_eq!(remote.mutations.len(), before);
        assert_eq!(remote.selectors, c.previous);
    }
    let fixture = Fixture::native_update();
    let set = fixture.set();
    let mut remote = fixture.remote();
    stage(&mut remote, &set, &fixture.contract).unwrap();
    publish(&mut remote, &set, &fixture.contract).unwrap();
    remote.proof(&fixture.contract);
    let proof: Proof = serde_json::from_str(
        block(
            remote.release.as_ref().unwrap().body.as_deref().unwrap(),
            PROOF_OPEN,
            PROOF_CLOSE,
        )
        .unwrap(),
    )
    .unwrap();
    record_proof(&mut remote, &set, &fixture.contract, &proof).unwrap();
    promote(&mut remote, &set, &fixture.contract).unwrap();
    assert_eq!(remote.selectors.environment.as_deref(), Some("0.3.1"));
    assert_eq!(remote.selectors.cli.as_deref(), Some("0.3.1"));
}

#[test]
fn lost_publish_response_at_every_boundary_resumes_the_same_set() {
    for fault in ["npm:@docouno/env", "npm:@docouno/devkit", "public"] {
        let f = Fixture::new();
        let mut remote = f.remote();
        let set = f.set();
        stage(&mut remote, &set, &f.contract).unwrap();
        let original = remote.assets.clone();
        remote.fault = Some(fault.into());
        assert!(publish(&mut remote, &set, &f.contract).is_err());
        stage(&mut remote, &set, &f.contract).unwrap();
        publish(&mut remote, &set, &f.contract).unwrap();
        assert_eq!(remote.assets, original);
        assert_eq!(remote.selectors, f.contract.previous);
        assert_eq!(
            remote
                .mutations
                .iter()
                .filter(|s| *s == "npm:@docouno/env")
                .count(),
            1
        );
        assert_eq!(
            remote
                .mutations
                .iter()
                .filter(|s| *s == "npm:@docouno/devkit")
                .count(),
            1
        );
        let env = remote
            .mutations
            .iter()
            .position(|s| s == "npm:@docouno/env")
            .unwrap();
        let cli = remote
            .mutations
            .iter()
            .position(|s| s == "npm:@docouno/devkit")
            .unwrap();
        assert!(env < cli);
        assert!(remote.release.as_ref().unwrap().immutable);
    }
}

#[test]
fn matching_metadata_with_different_actual_npm_bytes_is_a_conflict() {
    let f = Fixture::new();
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    publish(&mut remote, &set, &f.contract).unwrap();
    remote.npm.get_mut(ENVIRONMENT).unwrap().bytes.push(0);
    let before = remote.mutations.len();
    assert!(publish(&mut remote, &set, &f.contract).is_err());
    assert_eq!(remote.mutations.len(), before);
}

#[test]
fn existing_registry_without_durable_frozen_set_never_creates_a_new_release() {
    let f = Fixture::new();
    let set = f.set();
    let mut remote = f.remote();
    let p = &set.index.packages["environment"];
    remote.npm.insert(
        ENVIRONMENT.into(),
        PublishedNpm {
            integrity: p.integrity.clone(),
            shasum: p.shasum.clone(),
            bytes: fs::read(set.root.join(&p.archive)).unwrap(),
        },
    );
    assert!(stage(&mut remote, &set, &f.contract).is_err());
    assert!(remote.release.is_none());
}

#[test]
fn changed_contract_tag_or_asset_refuses_without_mutation() {
    let f = Fixture::new();
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    let before = remote.mutations.len();
    remote.assets.get_mut("checksums.txt").unwrap().push(0);
    assert!(stage(&mut remote, &set, &f.contract).is_err());
    assert_eq!(remote.mutations.len(), before);
    let mut remote = f.remote();
    remote.tag = Some(("c".repeat(40), "b".repeat(40)));
    assert!(stage(&mut remote, &set, &f.contract).is_err());
    assert!(remote.mutations.is_empty());
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    let mut changed = f.contract.clone();
    changed.previous.cli = None;
    let before = remote.mutations.len();
    assert!(stage(&mut remote, &set, &changed).is_err());
    assert_eq!(remote.mutations.len(), before);
}

#[test]
fn promotion_requires_all_public_gates_and_resumes_every_partial_channel_write() {
    for fault in [
        "latest:@docouno/env",
        "latest:@docouno/devkit",
        "latest:github",
    ] {
        let f = Fixture::new();
        let set = f.set();
        let mut remote = f.remote();
        stage(&mut remote, &set, &f.contract).unwrap();
        publish(&mut remote, &set, &f.contract).unwrap();
        let before = remote.mutations.len();
        assert!(promote(&mut remote, &set, &f.contract).is_err());
        assert_eq!(remote.mutations.len(), before);
        remote.proof(&f.contract);
        remote.fault = Some(fault.into());
        assert!(promote(&mut remote, &set, &f.contract).is_err());
        promote(&mut remote, &set, &f.contract).unwrap();
        assert_eq!(
            remote.selectors,
            Selectors {
                environment: Some("0.3.0".into()),
                cli: Some("0.3.0".into()),
                github: Some("v0.3.0".into())
            }
        );
        let before = remote.mutations.len();
        promote(&mut remote, &set, &f.contract).unwrap();
        assert_eq!(remote.mutations.len(), before);
        for label in [
            "latest:@docouno/env",
            "latest:@docouno/devkit",
            "latest:github",
        ] {
            assert_eq!(
                remote
                    .mutations
                    .iter()
                    .filter(|value| value.as_str() == label)
                    .count(),
                1
            );
        }
    }
}

#[test]
fn acknowledged_npm_writes_wait_through_stale_reads_including_the_final_snapshot() {
    let f = Fixture::native_update();
    let set = f.set();
    let c = &f.contract;
    let mut remote = f.remote();
    stage(&mut remote, &set, c).unwrap();
    publish(&mut remote, &set, c).unwrap();
    remote.proof(c);
    let previous = c.previous.clone();
    let environment = Selectors {
        environment: Some(c.version.clone()),
        ..previous.clone()
    };
    let npm = Selectors {
        cli: Some(c.version.clone()),
        ..environment.clone()
    };
    let complete = Selectors {
        github: Some(c.tag()),
        ..npm.clone()
    };
    remote.selector_reads.extend([
        Ok(previous.clone()), // Pre-write snapshot.
        Ok(previous.clone()), // Successful env ack is not visible yet.
        Ok(previous.clone()), // A subsequent independent registry read is still stale.
        Ok(environment),
        Ok(previous.clone()), // An older env read can recur during CLI readiness.
        Ok(npm),
        Ok(Selectors {
            github: Some(c.tag()),
            ..previous
        }), // Final read after GH write.
        Ok(complete.clone()),
    ]);
    let before = remote.mutations.len();
    promote(&mut remote, &set, c).unwrap();
    assert_eq!(remote.selectors, complete);
    assert_eq!(remote.readiness_pauses, 4);
    assert!(remote.selector_reads.is_empty());
    assert_eq!(
        &remote.mutations[before..],
        [
            "latest:@docouno/env",
            "latest:@docouno/devkit",
            "latest:github"
        ]
    );
    promote(&mut remote, &set, c).unwrap();
    assert_eq!(remote.mutations.len(), before + 3);
}

#[test]
fn selector_read_fault_after_successful_write_stops_and_resume_never_repeats_that_write() {
    for kind in ["authorization", "transport", "malformed JSON"] {
        let f = Fixture::native_update();
        let set = f.set();
        let c = &f.contract;
        let mut remote = f.remote();
        stage(&mut remote, &set, c).unwrap();
        publish(&mut remote, &set, c).unwrap();
        remote.proof(c);
        remote
            .selector_reads
            .extend([Ok(c.previous.clone()), Err(anyhow::anyhow!(kind))]);
        let before = remote.mutations.len();
        assert_eq!(promote(&mut remote, &set, c).unwrap_err().to_string(), kind);
        assert_eq!(&remote.mutations[before..], ["latest:@docouno/env"]);
        assert_eq!(remote.readiness_pauses, 0);
        promote(&mut remote, &set, c).unwrap();
        assert_eq!(
            &remote.mutations[before..],
            [
                "latest:@docouno/env",
                "latest:@docouno/devkit",
                "latest:github"
            ]
        );
    }
}

#[test]
fn selector_wait_exhaustion_stops_before_other_mutations_and_resumes_visible_original_bytes() {
    let f = Fixture::native_update();
    let set = f.set();
    let c = &f.contract;
    let mut remote = f.remote();
    stage(&mut remote, &set, c).unwrap();
    publish(&mut remote, &set, c).unwrap();
    remote.proof(c);
    remote
        .selector_reads
        .extend((0..40).map(|_| Ok(c.previous.clone())));
    let before = remote.mutations.len();
    let original = remote.assets.clone();
    let error = promote(&mut remote, &set, c).unwrap_err();
    assert!(error.to_string().contains("within 360s"));
    assert_eq!(remote.readiness_pauses, 36);
    assert_eq!(remote.selector_reads.len(), 3); // Initial read + 36 bounded attempts.
    assert_eq!(&remote.mutations[before..], ["latest:@docouno/env"]);
    remote.selector_reads.clear(); // Registry has now exposed the acknowledged write.
    promote(&mut remote, &set, c).unwrap();
    assert_eq!(remote.assets, original);
    assert_eq!(
        &remote.mutations[before..],
        [
            "latest:@docouno/env",
            "latest:@docouno/devkit",
            "latest:github"
        ]
    );
}

#[test]
fn unexpected_selector_after_successful_write_never_polls_or_promotes_other_channels() {
    for pointer in [None, Some("malformed"), Some("0.3.2"), Some("0.2.9")] {
        let f = Fixture::native_update();
        let set = f.set();
        let c = &f.contract;
        let mut remote = f.remote();
        stage(&mut remote, &set, c).unwrap();
        publish(&mut remote, &set, c).unwrap();
        remote.proof(c);
        let mut unexpected = c.previous.clone();
        unexpected.environment = pointer.map(str::to_owned);
        remote
            .selector_reads
            .extend([Ok(c.previous.clone()), Ok(unexpected)]);
        let before = remote.mutations.len();
        assert!(promote(&mut remote, &set, c).is_err());
        assert_eq!(remote.readiness_pauses, 0);
        assert_eq!(&remote.mutations[before..], ["latest:@docouno/env"]);
    }
}

#[test]
fn github_only_staleness_does_not_gain_a_retry_policy() {
    let f = Fixture::native_update();
    let set = f.set();
    let c = &f.contract;
    let mut remote = f.remote();
    stage(&mut remote, &set, c).unwrap();
    publish(&mut remote, &set, c).unwrap();
    remote.proof(c);
    let npm = Selectors {
        environment: Some(c.version.clone()),
        cli: Some(c.version.clone()),
        ..c.previous.clone()
    };
    remote.selector_reads.extend([
        Ok(c.previous.clone()),
        Ok(Selectors {
            environment: Some(c.version.clone()),
            ..c.previous.clone()
        }),
        Ok(npm.clone()),
        Ok(npm),
    ]);
    assert!(
        promote(&mut remote, &set, c)
            .unwrap_err()
            .to_string()
            .contains("Selectors did not converge")
    );
    assert_eq!(remote.readiness_pauses, 0);
}

#[test]
fn newer_or_unexpected_pointer_blocks_all_promotion_before_first_write() {
    for selector in ["0.4.0", "0.2.9"] {
        let f = Fixture::new();
        let set = f.set();
        let mut remote = f.remote();
        stage(&mut remote, &set, &f.contract).unwrap();
        publish(&mut remote, &set, &f.contract).unwrap();
        remote.proof(&f.contract);
        remote.selectors.cli = Some(selector.into());
        let before = remote.mutations.len();
        assert!(promote(&mut remote, &set, &f.contract).is_err());
        assert_eq!(remote.mutations.len(), before);
    }
}

#[test]
fn incomplete_stage_cannot_be_extended_after_any_registry_publication() {
    let f = Fixture::new();
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    publish(&mut remote, &set, &f.contract).unwrap();
    remote.release.as_mut().unwrap().draft = true;
    remote.assets.remove("checksums.txt");
    let before = remote.mutations.len();
    assert!(stage(&mut remote, &set, &f.contract).is_err());
    assert_eq!(remote.mutations.len(), before);
}

#[test]
fn durable_proof_update_preserves_channels_and_contract() {
    let f = Fixture::new();
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    publish(&mut remote, &set, &f.contract).unwrap();
    remote.proof(&f.contract);
    let proof: Proof = serde_json::from_str(
        block(
            remote.release.as_ref().unwrap().body.as_deref().unwrap(),
            PROOF_OPEN,
            PROOF_CLOSE,
        )
        .unwrap(),
    )
    .unwrap();
    record_proof(&mut remote, &set, &f.contract, &proof).unwrap();
    assert_eq!(remote.selectors, f.contract.previous);
    assert_eq!(
        remote.release.as_ref().unwrap().contract().unwrap(),
        f.contract
    );
    let mut incomplete = proof;
    incomplete.gates.remove("performance");
    assert!(record_proof(&mut remote, &set, &f.contract, &incomplete).is_err());
}

#[test]
fn local_bytes_and_duplicate_metadata_are_not_self_authorizing() {
    let f = Fixture::new();
    fs::write(f.directory.path().join("docouno-env-0.3.0.tgz"), b"changed").unwrap();
    assert!(FrozenSet::load(f.directory.path()).is_err());
    assert!(
        Contract::from_body(&format!(
            "{}{}",
            f.contract.body().unwrap(),
            f.contract.body().unwrap()
        ))
        .is_err()
    );
    assert!(Contract::from_body("ordinary notes without contract").is_err());
}

#[test]
fn timed_provider_reads_preserve_nonzero_output_and_enforce_deadlines() {
    use devkit::processes::OutputMode;
    use std::{process::Command, time::Duration};
    let runner = ProcessRunner::new().unwrap();
    let output = runner
        .run_with_timeout(
            Command::new("sh").args([
                "-c",
                "printf '{\"error\":\"E404\"}'; printf diagnostic >&2; exit 42",
            ]),
            OutputMode::Capture(4096),
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(output.code, 42);
    assert_eq!(output.stdout, b"{\"error\":\"E404\"}");
    assert_eq!(output.stderr, b"diagnostic");
    let started = std::time::Instant::now();
    assert!(
        runner
            .run_with_timeout(
                Command::new("sh").args(["-c", "sleep 20"]),
                OutputMode::Capture(4096),
                Duration::from_millis(40)
            )
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn generated_performance(c: &Contract) -> Value {
    let mut raw = json!({"schemaVersion":1,"success":true,"version":c.version,"sourceCommit":c.source_commit,"sourceTree":c.source_tree,"indexSha256":c.index_sha256,"measurements":[],"fixtures":[],"commands":[],"definitions":{"warm":"two warmups, alternating old/new, plain samples","cold":"actual public transport required separately"}});
    for index in 0..24 {
        raw["measurements"].as_array_mut().unwrap().push(json!({
            "name":format!("large-closure-doctor-native-{index}"),"shape":"large-closure","side":"native","operation":"doctor","componentCount":20,
            "sampleCount":30,"medianSeconds":0.123,"p95Seconds":0.456,"minSeconds":0.1,"maxSeconds":0.5,
            "samplesSeconds":vec![0.123456789;30],"warmupSeconds":[0.1,0.1],"sampleOrder":(0..32).collect::<Vec<_>>(),
            "root":"/private/long/diagnostic/path","argv":["make","doctor"],"inputsBytesAndModesUnchanged":true,
            "trace":{"dockerLog":"/private/log","hostExecSummary":"/private/strace","dockerCliCalls":2,"oneShotContainers":0,"controlContainers":0,"toolContainers":0,"otherContainers":0,"execSyscallAttempts":7,"execSyscallErrors":0,"successfulExecs":7,"instrumentedTotalSeconds":0.2,"instrumentedToolContainerSeconds":0.0,"instrumentedOutsideToolSeconds":0.2}
        }));
    }
    for _ in 0..8 {
        raw["fixtures"].as_array_mut().unwrap().push(json!({"shape":"large-closure","side":"native","componentCount":20,"localTransitiveCrates":80,"version":c.version,"releaseIdentity":"a".repeat(64),"inputSnapshotSha256":"b".repeat(64),"inputFiles":800,"inputBytes":100000,"rustImageId":"c".repeat(64),"productInputSha256":"d".repeat(64),"sourceUnchanged":true,"root":"/private/product"}));
    }
    raw["commands"] = json!([{"stdout":"z".repeat(200000)}]);
    raw
}

#[test]
fn durable_performance_summary_bounds_the_actual_twenty_four_case_shape() {
    let f = Fixture::new();
    let raw = generated_performance(&f.contract);
    let full = serde_json::to_vec(&raw).unwrap();
    let compact = compact_gate("performance", raw).unwrap();
    validate_gate("performance", &compact, &f.contract).unwrap();
    let text = serde_json::to_string(&compact).unwrap();
    println!(
        "Representative performance proof: full={} bytes, durable={} bytes, cases=24, fixtures=8",
        full.len(),
        text.len()
    );
    assert!(full.len() > 200000);
    assert!(
        text.len() < 26000,
        "Compact performance summary is {} bytes",
        text.len()
    );
    assert!(
        !text.contains("samplesSeconds")
            && !text.contains("/private/")
            && !text.contains("sampleOrder")
    );
    assert_eq!(compact["measurements"].as_array().unwrap().len(), 24);
}

fn read_proof(remote: &Fake) -> Proof {
    serde_json::from_str(
        block(
            remote.release.as_ref().unwrap().body.as_deref().unwrap(),
            PROOF_OPEN,
            PROOF_CLOSE,
        )
        .unwrap(),
    )
    .unwrap()
}

fn npx_receipt(c: &Contract, mode: &str) -> Value {
    let registry_spec = if mode == "latest" {
        CLI.to_owned()
    } else {
        format!("{CLI}@{}", c.version)
    };
    let cases = ["022", "077"].map(|mask| {
        json!({
            "umask": mask,
            "registrySpec": registry_spec,
            "files": 310,
            "engineSha256": "e".repeat(64),
            "sameVersionRequests": 0,
            "canonicalReleaseIdentity": "f".repeat(64),
            "bareCiDiagnostic": true,
            "nativeImportNetwork": "none"
        })
    });
    json!({
        "schemaVersion": 1,
        "success": true,
        "error": null,
        "version": c.version,
        "sourceCommit": c.source_commit,
        "sourceTree": c.source_tree,
        "indexSha256": c.index_sha256,
        "selectorMode": mode,
        "ignoreScripts": true,
        "authenticationUsed": false,
        "umasks": ["022", "077"],
        "durationSeconds": 4.944376594999994,
        "harnessSha256": "d".repeat(64),
        "diagnostics": "/home/runner/work/project/project/.artifacts/release-proof/npx-public-diagnostics",
        "cases": cases
    })
}

fn assert_trace_facts(original: &Value, compact: &Value) {
    assert_eq!(compact["traceColumns"], json!(PERFORMANCE_TRACE_COLUMNS));
    let rows = compact["measurements"].as_array().unwrap();
    assert_eq!(rows.len(), 24);
    for (index, row) in rows.iter().enumerate() {
        for (column, name) in PERFORMANCE_TRACE_COLUMNS.iter().enumerate() {
            assert_eq!(
                row["trace"][column],
                original["measurements"][index]["trace"][*name]
            );
        }
        for key in [
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
        ] {
            assert_eq!(row[key], original["measurements"][index][key]);
        }
    }
}

#[test]
fn performance_projection_preserves_trace_values_and_rejects_ambiguous_columns() {
    let f = Fixture::new();
    let original = varied_performance(&f.contract);
    let compact = compact_gate("performance", original.clone()).unwrap();
    assert_trace_facts(&original, &compact);
    for key in [
        "comparisons",
        "definitions",
        "limits",
        "harness",
        "admissionChecks",
        "host",
        "tools",
        "baseline",
        "releaseIdentity",
        "buildRun",
    ] {
        assert_eq!(compact[key], original[key]);
    }
    assert_eq!(compact["comparisons"].as_array().unwrap().len(), 14);
    assert_eq!(compact["fixtures"].as_array().unwrap().len(), 8);
    for (index, fixture) in original["fixtures"].as_array().unwrap().iter().enumerate() {
        let mut expected = fixture.clone();
        expected.as_object_mut().unwrap().remove("root");
        assert_eq!(compact["fixtures"][index], expected);
    }
    validate_gate("performance", &original, &f.contract).unwrap(); // Historical object traces.
    validate_gate("performance", &compact, &f.contract).unwrap();
    let corruptions = [
        "missing-header",
        "missing-column",
        "extra-column",
        "duplicate-column",
        "reordered-column",
        "missing-value",
        "extra-value",
        "bad-count",
        "bad-timing",
    ];
    for corruption in corruptions {
        let mut invalid = compact.clone();
        match corruption {
            "missing-header" => {
                invalid.as_object_mut().unwrap().remove("traceColumns");
            }
            "missing-column" => {
                invalid["traceColumns"].as_array_mut().unwrap().pop();
            }
            "extra-column" => invalid["traceColumns"]
                .as_array_mut()
                .unwrap()
                .push(json!("unknown")),
            "duplicate-column" => invalid["traceColumns"][1] = invalid["traceColumns"][0].clone(),
            "reordered-column" => invalid["traceColumns"].as_array_mut().unwrap().swap(0, 1),
            "missing-value" => {
                invalid["measurements"][0]["trace"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
            "extra-value" => invalid["measurements"][0]["trace"]
                .as_array_mut()
                .unwrap()
                .push(json!(0)),
            "bad-count" => invalid["measurements"][0]["trace"][0] = json!(0.5),
            "bad-timing" => invalid["measurements"][0]["trace"][8] = json!(-1),
            _ => unreachable!(),
        }
        assert!(
            validate_gate("performance", &invalid, &f.contract).is_err(),
            "{corruption}"
        );
    }
}

#[test]
fn proof_updates_replace_only_the_owned_line_and_preserve_all_surrounding_notes() {
    let f = Fixture::native_update();
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    publish(&mut remote, &set, &f.contract).unwrap();
    remote.proof(&f.contract);
    let proof = read_proof(&remote);
    let raw = serde_json::to_string(&proof).unwrap();
    let user_line = format!(
        "User quotation: Public verification: passed. Receipt SHA256 `{}`.\n",
        digest(raw.as_bytes())
    );
    let prefix = format!(
        "{}\nManual notes and whitespace stay here.  \n{user_line}",
        f.contract.body().unwrap()
    );
    let suffix = "\n\n## Operator notes\nPreserve this after the proof.\n";
    remote.release.as_mut().unwrap().body =
        Some(format!("{prefix}{PROOF_OPEN}{raw}{PROOF_CLOSE}{suffix}"));
    record_proof(&mut remote, &set, &f.contract, &proof).unwrap();
    let first = remote.release.as_ref().unwrap().body.clone().unwrap();
    assert!(first.starts_with(&prefix) && first.ends_with(suffix));
    for _ in 0..3 {
        record_proof(&mut remote, &set, &f.contract, &proof).unwrap();
        assert_eq!(
            remote.release.as_ref().unwrap().body.as_deref(),
            Some(first.as_str())
        );
    }
    assert_eq!(
        first
            .lines()
            .filter(|line| line.starts_with("Public verification: passed."))
            .count(),
        1
    );
    assert_eq!(remote.selectors, f.contract.previous);
    assert_eq!(
        remote.release.as_ref().unwrap().contract().unwrap(),
        f.contract
    );
    // A line with another digest is not ours to remove.
    let unowned = format!(
        "Public verification: passed. Receipt SHA256 `{}`.\n",
        "0".repeat(64)
    );
    let old = format!("{prefix}{unowned}{PROOF_OPEN}{raw}{PROOF_CLOSE}{suffix}");
    assert!(
        proof_body(&old, &proof)
            .unwrap()
            .starts_with(&format!("{prefix}{unowned}"))
    );
    for malformed in [
        format!("{prefix}{PROOF_CLOSE}"),
        format!("{first}{PROOF_OPEN}"),
        format!("{first}{PROOF_CLOSE}"),
    ] {
        remote.release.as_mut().unwrap().body = Some(malformed);
        let before = remote.mutations.len();
        assert!(record_proof(&mut remote, &set, &f.contract, &proof).is_err());
        assert_eq!(remote.mutations.len(), before);
    }
}

#[test]
fn proof_updates_without_a_prefix_newline_preserve_text_and_remain_idempotent() {
    let f = Fixture::native_update();
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, &f.contract).unwrap();
    publish(&mut remote, &set, &f.contract).unwrap();
    remote.proof(&f.contract);
    let proof = read_proof(&remote);
    let raw = serde_json::to_string(&proof).unwrap();
    let suffix = "\n\nOperator notes after the proof stay byte-for-byte.  \n";
    for note in [
        "Operator note without newline",
        "Operator note with trailing spaces  ",
        "Operator note ending in carriage return\r",
        "",
    ] {
        let prefix = format!("{}{note}", f.contract.body().unwrap());
        remote.release.as_mut().unwrap().body =
            Some(format!("{prefix}{PROOF_OPEN}{raw}{PROOF_CLOSE}{suffix}"));
        record_proof(&mut remote, &set, &f.contract, &proof).unwrap();
        let first = remote.release.as_ref().unwrap().body.clone().unwrap();
        assert!(first.starts_with(&prefix) && first.ends_with(suffix));
        let status = first.find("Public verification: passed.").unwrap();
        assert_eq!(&first[status - 1..status], "\n");
        assert_eq!(
            first
                .lines()
                .filter(|line| line.starts_with("Public verification: passed."))
                .count(),
            1
        );
        for _ in 0..3 {
            record_proof(&mut remote, &set, &f.contract, &proof).unwrap();
            assert_eq!(
                remote.release.as_ref().unwrap().body.as_deref(),
                Some(first.as_str())
            );
        }
        assert_eq!(remote.selectors, f.contract.previous);
        assert_eq!(
            remote.release.as_ref().unwrap().contract().unwrap(),
            f.contract
        );
    }
}

fn varied_performance(c: &Contract) -> Value {
    let mut raw = generated_performance(c);
    for measurement in raw["measurements"].as_array_mut().unwrap() {
        for (key, value) in [
            ("medianSeconds", 0.123_456_789_012_345_66),
            ("p95Seconds", 0.4567890123456789),
            ("minSeconds", 0.1111111111111111),
            ("maxSeconds", 0.49999999999999994),
        ] {
            measurement[key] = json!(value);
        }
        for key in &PERFORMANCE_TRACE_COLUMNS[8..] {
            measurement["trace"][*key] = json!(0.123_456_789_012_345_66);
        }
    }
    raw["comparisons"] = json!((0..14).map(|i| {
        let mut comparison = json!({
            "shape": format!("component-shape-{i}"),
            "operation": "describe-vs-doctor",
            "nativeToBaselineMedianRatio": 0.123_456_789_012_345_66,
            "nativeFaster": true
        });
        if i % 4 == 0 {
            comparison["scopeDifference"] = json!("Native describe admits the full graph without SDK readiness; baseline doctor includes Docker and image readiness.");
        }
        comparison
    }).collect::<Vec<_>>());
    raw["definitions"] = json!(["warm","latency","hostProcesses","containers","tooling","cold","inputs","fixture"].map(|key| (key.to_owned(), format!("{key}: Plain warm measurements alternate old and new release factories with two warmups. Instrumented tracing is a separate diagnostic run; source, mode and product identities are unchanged. Raw samples and complete diagnostics remain in the original receipt."))).into_iter().collect::<BTreeMap<_,_>>());
    raw["limits"] = json!({
        "minimumSamples": 30,
        "warmups": 2,
        "p95": "nearest-rank ceil(0.95*n)",
        "requireNativeControlContainers": 0,
        "requireNativeDescribeDockerCalls": 0,
        "requireBuildToolContainersPerComponent": 1,
        "support": "Linux x86_64 with local Docker Compose; host language SDK excluded",
        "setupAndLockResolution": "Prepared outside measurement; required ecosystem checks remain enabled"
    });
    raw["host"] = json!({
        "system": "Linux",
        "machine": "x86_64",
        "kernel": "6.17.0-generic",
        "cpuCount": 4,
        "cpuModel": "Synthetic server processor with extended descriptive model metadata ".repeat(4)
    });
    let strace_files = [
        "strace",
        "libc.so.6",
        "liblzma.so.5",
        "libunwind.so.8",
        "libunwind-ptrace.so.0",
        "libunwind-x86_64.so.8",
        "ld-linux-x86-64.so.2",
    ]
    .map(|key| (key.to_owned(), "c".repeat(64)))
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    raw["tools"] = json!({
        "traceImageId": format!("sha256:{}", "a".repeat(64)),
        "traceRecipeSha256": "b".repeat(64),
        "straceVersion": "strace -- version 6.1",
        "straceFiles": strace_files
    });
    raw["harness"] = json!(
        ["verify.py", "test_harness.py", "measure.py"]
            .map(|key| (key.to_owned(), "d".repeat(64)))
            .into_iter()
            .collect::<BTreeMap<_, _>>()
    );
    raw["admissionChecks"] = json!([
        {"case": "refuse-source-edits", "exitCode": 1, "sourceUnchanged": true},
        {"case": "refuse-missing-cache", "exitCode": 1, "sourceUnchanged": true}
    ]);
    raw["buildRun"] = json!(c.build_run);
    raw["releaseIdentity"] = json!("e".repeat(64));
    raw["baseline"] = json!({
        "version": "0.2.1",
        "source": "/home/runner/work/project/project/.artifacts/legacy/devkit-0.2.1",
        "publicArchiveUrl": "https://github.com/docouno/devkit/releases/download/v0.2.1/devkit-0.2.1.tar.gz",
        "archiveSha256": "f".repeat(64),
        "releaseIdentity": "a".repeat(64)
    });
    raw
}

#[test]
fn complete_generated_proof_compacts_latest_and_retains_all_required_facts_with_headroom() {
    let f = Fixture::native_update();
    let c = &f.contract;
    let set = f.set();
    let mut remote = f.remote();
    stage(&mut remote, &set, c).unwrap();
    publish(&mut remote, &set, c).unwrap();
    remote.proof(c);
    let mut proof = read_proof(&remote);
    let raw_performance = varied_performance(c);
    let performance_bytes = serde_json::to_vec_pretty(&raw_performance).unwrap();
    proof.gates.insert(
        "performance".into(),
        gate_from_receipt(
            "performance",
            &performance_bytes,
            raw_performance.clone(),
            c,
        )
        .unwrap(),
    );
    proof.gates.get_mut("readme").unwrap().summary["exampleResults"] = json!((0..56).map(|index| json!({"label":format!("README example {index}: accepted source state"),"passed":true,"durationSeconds":0.123_456_789_012_345_66,"exitCode":0})).collect::<Vec<_>>());
    proof.gates.get_mut("npx-pinned").unwrap().summary =
        compact_gate("npx-pinned", npx_receipt(c, "pinned")).unwrap();
    for old in ["0.2.0", "0.2.1"] {
        let summary = &mut proof
            .gates
            .get_mut(&format!("public-{old}"))
            .unwrap()
            .summary;
        let identity = &mut summary["identities"][0];
        identity["version"] = json!(c.version);
        identity["releaseIdentity"] = json!(set.index.release_identity);
        identity["buildRun"] = json!(c.build_run);
        identity["gitSource"] = json!({"commit":c.source_commit,"tree":c.source_tree});
        identity["tarAndNpmCanonicalPayloadEqual"] = json!(true);
        identity["packages"] = json!([ENVIRONMENT,CLI].map(|name| json!({"name":name,"version":c.version,"integrity":format!("sha512-{}", "a".repeat(88)),"shasum":"b".repeat(40)})));
        summary["limits"] = json!([
            "Actual public legacy factory; source and complete package identities verified",
            "No host language SDK and no authenticated downloads"
        ]);
        summary["consumerReceiptSha256"] = json!("c".repeat(64));
    }
    proof.validate(c).unwrap();
    assert_trace_facts(&raw_performance, &proof.gates["performance"].summary);
    let latest = npx_receipt(c, "latest");
    let bytes = serde_json::to_vec_pretty(&latest).unwrap();
    let mut old_path = proof.clone();
    old_path
        .gates
        .get_mut("performance")
        .unwrap()
        .summary
        .as_object_mut()
        .unwrap()
        .remove("traceColumns");
    for (index, measurement) in
        old_path.gates.get_mut("performance").unwrap().summary["measurements"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
    {
        measurement["trace"] = selected(
            &raw_performance["measurements"][index]["trace"],
            PERFORMANCE_TRACE_COLUMNS,
        );
    }
    old_path.post_promotion_npx = Some(Gate {
        sha256: digest(&bytes),
        bytes: bytes.len() as u64,
        summary: latest.clone(),
    });
    let before = serde_json::to_vec(&old_path).unwrap().len();
    println!("Generated original final proof: {before} bytes");
    assert!(before > 40000);
    assert!(
        proof_body(&c.body().unwrap(), &old_path)
            .unwrap_err()
            .to_string()
            .contains("exceeds release-body budget")
    );
    record_proof(&mut remote, &set, c, &proof).unwrap();
    promote(&mut remote, &set, c).unwrap();
    record_latest(&mut remote, &set, c, &bytes).unwrap();
    let saved = read_proof(&remote);
    saved.validate(c).unwrap();
    let gate = saved.post_promotion_npx.as_ref().unwrap();
    assert_eq!(gate.sha256, digest(&bytes));
    assert_eq!(gate.bytes, bytes.len() as u64);
    assert_ne!(
        gate.sha256,
        digest(&serde_json::to_vec(&gate.summary).unwrap())
    );
    for key in [
        "schemaVersion",
        "success",
        "version",
        "sourceCommit",
        "sourceTree",
        "indexSha256",
        "selectorMode",
        "ignoreScripts",
        "authenticationUsed",
        "umasks",
        "cases",
        "durationSeconds",
        "harnessSha256",
    ] {
        assert_eq!(gate.summary[key], latest[key]);
    }
    assert_eq!(gate.summary["cases"].as_array().unwrap().len(), 2);
    assert!(gate.summary.get("diagnostics").is_none());
    assert_eq!(gate.summary, compact_gate("npx-latest", latest).unwrap());
    assert_eq!(
        saved.gates["readme"].summary["nativeUpgrade"],
        proof.gates["readme"].summary["nativeUpgrade"]
    );
    for (name, original) in &proof.gates {
        assert_eq!(
            serde_json::to_value(&saved.gates[name]).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }
    let after = serde_json::to_vec(&saved).unwrap().len();
    println!("Generated compact final proof: {after} bytes");
    assert!(
        after < 37000,
        "Expected several KB of proof headroom, got {after}"
    );
    let body = remote.release.as_ref().unwrap().body.clone().unwrap();
    record_latest(&mut remote, &set, c, &bytes).unwrap();
    assert_eq!(
        remote.release.as_ref().unwrap().body.as_deref(),
        Some(body.as_str())
    );
    assert_eq!(
        body.lines()
            .filter(|line| line.starts_with("Public verification: passed."))
            .count(),
        1
    );
    let mut too_large = saved;
    too_large.gates.get_mut("readme").unwrap().summary["unknownFutureFact"] =
        json!("x".repeat(40000));
    let writes = remote.mutations.len();
    assert!(record_proof(&mut remote, &set, c, &too_large).is_err());
    assert_eq!(remote.mutations.len(), writes);
}

#[test]
#[ignore = "explicit operator-supplied retained public receipts; local read-only"]
fn actual_retained_proof_compaction_readonly() {
    let root = PathBuf::from(
        std::env::var("DEVKIT_PROOF_PROBE_DIRECTORY").expect("explicit receipt directory required"),
    );
    let mut proof: Proof = files::read_json(&root.join("compact.json")).unwrap();
    let previous = proof.gates["readme"].summary["nativeUpgrade"]["fromVersion"]
        .as_str()
        .unwrap()
        .to_owned();
    let c = Contract {
        schema_version: 1,
        repository: REPO.into(),
        version: proof.version.clone(),
        source_commit: proof.source_commit.clone(),
        source_tree: proof.source_tree.clone(),
        index_sha256: proof.index_sha256.clone(),
        build_run: proof.publication_run.clone(),
        previous: Selectors {
            environment: Some(previous.clone()),
            cli: Some(previous.clone()),
            github: Some(format!("v{previous}")),
        },
    };
    proof.validate(&c).unwrap();
    let bytes = read_bounded(&root.join("npx-latest.json"), 16 * 1024 * 1024).unwrap();
    let latest: Value = serde_json::from_slice(&bytes).unwrap();
    let before_pre = serde_json::to_vec(&proof).unwrap().len();
    proof.post_promotion_npx = Some(Gate {
        sha256: digest(&bytes),
        bytes: bytes.len() as u64,
        summary: latest.clone(),
    });
    let before_final = serde_json::to_vec(&proof).unwrap().len();
    assert!(proof_body(&c.body().unwrap(), &proof).is_err());
    proof.post_promotion_npx = Some(gate_from_receipt("npx-latest", &bytes, latest, &c).unwrap());
    let latest_only = serde_json::to_vec(&proof).unwrap().len();
    let old_performance = proof.gates["performance"].summary.clone();
    let compact = compact_gate("performance", old_performance.clone()).unwrap();
    assert_trace_facts(&old_performance, &compact);
    for key in [
        "comparisons",
        "fixtures",
        "definitions",
        "limits",
        "harness",
        "admissionChecks",
        "host",
        "tools",
        "baseline",
        "releaseIdentity",
        "buildRun",
    ] {
        assert_eq!(compact[key], old_performance[key]);
    }
    proof.gates.get_mut("performance").unwrap().summary = compact;
    proof.validate(&c).unwrap();
    let after_final = serde_json::to_vec(&proof).unwrap().len();
    let body = proof_body(&c.body().unwrap(), &proof).unwrap();
    assert!(after_final < 36000);
    assert_eq!(proof_body(&body, &proof).unwrap(), body);
    assert_eq!(
        proof.post_promotion_npx.as_ref().unwrap().sha256,
        digest(&bytes)
    );
    let mut stress = proof.clone();
    let performance = &mut stress.gates.get_mut("performance").unwrap().summary;
    performance["host"]["cpuModel"] =
        json!("Synthetic server processor with extended model metadata ".repeat(5));
    for measurement in performance["measurements"].as_array_mut().unwrap() {
        for (key, value) in [
            ("medianSeconds", 0.123_456_789_012_345_66),
            ("p95Seconds", 0.4567890123456789),
            ("minSeconds", 0.1111111111111111),
            ("maxSeconds", 0.49999999999999994),
        ] {
            measurement[key] = json!(value);
        }
        for index in 8..11 {
            measurement["trace"][index] = json!(0.123_456_789_012_345_66);
        }
    }
    stress.validate(&c).unwrap();
    let stress_bytes = serde_json::to_vec(&stress).unwrap().len();
    let stress_body = proof_body(&c.body().unwrap(), &stress).unwrap();
    assert!(stress_bytes < 37000);
    println!(
        "{}",
        json!({"preProofBytes":before_pre,"oldFinalProofBytes":before_final,"latestOnlyCompactBytes":latest_only,"finalProofBytes":after_final,"finalProofHeadroom":40000-after_final,"renderedNotesBytes":body.len(),"stressProofBytes":stress_bytes,"stressRenderedNotesBytes":stress_body.len(),"latestRawReceiptSha256":digest(&bytes),"latestRawReceiptBytes":bytes.len(),"traceValuesPreserved":24*11,"comparisons":proof.gates["performance"].summary["comparisons"].as_array().unwrap().len(),"fixtures":proof.gates["performance"].summary["fixtures"].as_array().unwrap().len(),"nativeUpgradePreserved":true})
    );
}
