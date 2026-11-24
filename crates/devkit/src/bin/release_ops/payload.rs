use super::*;
use devkit::{
    delivery::{
        render_bootstrap_pins, render_npm_launcher, render_project_loader_pins, verify_executable,
    },
    distribution::{Target, npm_platforms},
    release::{AdmittedRelease, admit_release},
    runtime::{PnpmPin, ToolPins},
};

impl FrozenSet {
    /// Reopen both distribution archives even when an earlier invocation left a hydrated copy.
    pub(super) fn hydrate(&self) -> Result<()> {
        let root = files::project_root(&self.root)?;
        let payload = root.join("payload");
        let cached = files::exists(&payload)?;
        // A new payload must be staged on its own volume. Validating an existing set
        // needs no write access there and keeps executable checks on the build-set volume.
        let stage = if cached {
            tempfile::tempdir()?
        } else {
            tempfile::Builder::new()
                .prefix(".devkit-hydrate-")
                .tempdir_in(&root)?
        };
        let extracted = stage.path().join("tar");
        let tar = self.operation_copy(&self.index.archive, stage.path())?;
        let tar_root = format!("devkit-{}", self.index.version);
        archive::extract_archive(&tar, &extracted, &tar_root)?;
        let release = admit_release(&extracted)?;
        ensure!(
            release.identity == self.index.release_identity
                && release.manifest.version == self.index.version,
            "Frozen tar payload differs from release identity or version"
        );
        let npm = self.operation_copy(&self.index.packages["cli"].archive, stage.path())?;
        let package = stage.path().join("npm");
        archive::extract_archive(&npm, &package, "package")
            .context("Invalid frozen CLI npm archive")?;
        // The archive reader admits only canonical file modes. Admission checks every file and
        // the identical release manifest binds the complete set of paths, bytes and modes.
        ensure!(
            admit_release(&package)?.identity == release.identity,
            "Frozen CLI npm payload differs from the release tar"
        );
        ensure!(
            release.manifest.environment.integrity == self.index.packages["environment"].integrity,
            "Bundled environment package differs from its frozen npm archive"
        );

        if !cached {
            self.verify_payload_matrix(&release)?;
            match archive::publish_new_directory(&extracted, &payload) {
                Ok(()) => return Ok(()),
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) => {}
                Err(e) => return Err(e),
            }
        }
        let existing = admit_release(&payload)?;
        ensure!(
            existing.identity == release.identity,
            "Existing hydrated payload differs from the frozen release"
        );
        self.verify_payload_matrix(&existing)
    }

    pub(super) fn verify_payload_matrix(&self, release: &AdmittedRelease) -> Result<()> {
        let targets = REQUIRED_PUBLISHABLE_TARGETS;
        let expected_files: BTreeSet<_> = targets.iter().map(|t| t.payload_name()).collect();
        let actual_files: BTreeSet<_> = release
            .manifest
            .files
            .keys()
            .map(String::as_str)
            .filter(|name| {
                !name.contains('/') && (*name == "devkit" || name.starts_with("devkit-"))
            })
            .collect();
        ensure!(
            actual_files == expected_files,
            "Frozen payload engine target matrix differs"
        );
        let mut hashes = BTreeMap::new();
        for target in targets {
            let asset = self.index.engine_asset(*target)?;
            let hash = &self.index.files[asset].sha256;
            verify_executable(
                &release.root.join(target.payload_name()),
                hash,
                *target,
                true,
            )
            .with_context(|| format!("Hydrated engine differs from its frozen asset: {asset}"))?;
            hashes.insert(target.triple().to_owned(), hash.clone());
        }
        let pins = ToolPins::parse_release(&release.bytes("runtime/toolchains.env")?)?;
        ensure!(
            pins.version == self.index.version && pins.engines.hashes() == &hashes,
            "Frozen toolchain engine pins differ from the release index"
        );
        for target in Target::LINUX {
            PnpmPin::from_release_for_target(release, target)
                .with_context(|| format!("Invalid frozen pnpm pin for {}", target.triple()))?;
        }
        let package: Value = serde_json::from_slice(&release.bytes("package.json")?)?;
        let (systems, cpus) = npm_platforms(targets);
        ensure!(
            package["os"] == json!(systems)
                && package["cpu"] == json!(cpus)
                && package["bin"] == json!({"devkit": "bin/devkit"}),
            "Frozen npm platforms or bin entry differ from the release matrix"
        );
        let version = &self.index.version;
        for (file, rendered) in [
            ("bin/devkit", render_npm_launcher(&hashes)?),
            (
                "runtime/devkit.sh",
                render_project_loader_pins(version, &hashes)?,
            ),
        ] {
            ensure!(
                release
                    .manifest
                    .files
                    .get(file)
                    .is_some_and(|f| f.mode == 0o755 && f.sha256 == digest(rendered.as_bytes())),
                "Frozen {file} differs from its target matrix"
            );
        }
        let bootstrap_hash = digest(render_bootstrap_pins(version, &hashes)?.as_bytes());
        ensure!(
            ["install.sh", "bootstrap.sh"]
                .iter()
                .all(|file| self.index.files[*file].sha256 == bootstrap_hash),
            "Frozen bootstrap scripts differ from the release matrix"
        );
        Ok(())
    }
}
