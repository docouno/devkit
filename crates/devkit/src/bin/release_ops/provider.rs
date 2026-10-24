//! Official gh/npm commands own authentication. Tokens are neither interpreted
//! nor persisted here. Public registry reads use a clean, private npm context.
use super::*;
use devkit::processes::{OutputMode, ProcessOutput};
use std::{
    process::Command,
    time::{Duration, Instant},
};

#[cfg(test)]
#[path = "provider_lookup_tests.rs"]
mod lookup_tests;

fn github_response(output: ProcessOutput, method: &str, path: &str) -> Result<Option<Value>> {
    let text = output.text()?;
    let line = text
        .lines()
        .next()
        .context("GitHub API returned no HTTP status")?;
    ensure!(
        line.starts_with("HTTP/"),
        "GitHub API returned no HTTP status"
    );
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .context("GitHub HTTP status missing")?
        .parse()?;
    if status == 404 && method == "GET" {
        return Ok(None);
    }
    ensure!(
        (200..300).contains(&status) && output.code == 0,
        "GitHub API {method} {path} failed with HTTP {status}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .map(|(_, body)| body)
        .context("GitHub API body missing")?;
    Ok(Some(if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body)?
    }))
}
fn npm_response(output: ProcessOutput) -> Result<Option<Value>> {
    let value: Value = serde_json::from_slice(&output.stdout)
        .context("Registry read returned no JSON; refusing to infer absence")?;
    if output.code != 0 {
        ensure!(
            value["error"]["code"] == "E404",
            "Registry read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(None);
    }
    ensure!(
        value.get("error").is_none(),
        "Registry returned an error with a successful status"
    );
    Ok(Some(value))
}

pub(super) fn require_event_source(source: &str, event_sha: Option<&str>) -> Result<()> {
    ensure!(
        source_hash(source),
        "Publication requires an exact source SHA"
    );
    ensure!(
        event_sha == Some(source),
        "Frozen source must equal the authentic workflow GITHUB_SHA for npm provenance. To retry a historical release after main moves, rerun its original run: gh run rerun ORIGINAL_RUN_ID --failed; do not dispatch it from a newer commit"
    );
    Ok(())
}

pub(super) fn require_actions(source: &str) -> Result<()> {
    require_event_source(source, std::env::var("GITHUB_SHA").ok().as_deref())?;
    for (key, expected) in [
        ("GITHUB_ACTIONS", "true"),
        ("GITHUB_REPOSITORY", REPO),
        ("GITHUB_REF", "refs/heads/main"),
        ("GITHUB_EVENT_NAME", "workflow_dispatch"),
        (
            "GITHUB_WORKFLOW_REF",
            "docouno/devkit/.github/workflows/release.yml@refs/heads/main",
        ),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
    ] {
        ensure!(
            std::env::var(key).ok().as_deref() == Some(expected),
            "Publication requires the reviewed main release.yml on a GitHub-hosted runner: {key}"
        );
    }
    let runner = ProcessRunner::new()?;
    for (program, expected) in [("npm", "11.21.1"), ("node", "v24.22.1")] {
        let output = runner.checked_with_timeout(
            Command::new(program).arg("--version"),
            OutputMode::Capture(4096),
            Duration::from_secs(15),
        )?;
        ensure!(
            output.text()?.trim() == expected,
            "Publisher requires {program} {expected}"
        );
    }
    Ok(())
}

pub(super) struct GhNpm {
    runner: ProcessRunner,
    scratch: tempfile::TempDir,
    gh_executable: PathBuf,
    release_ids: BTreeMap<String, u64>,
    latest_read_started: BTreeMap<String, Instant>,
}
impl GhNpm {
    pub(super) fn new(runner: ProcessRunner) -> Result<Self> {
        let scratch = tempfile::tempdir()?;
        fs::create_dir(scratch.path().join("npm"))?;
        Ok(Self {
            runner,
            scratch,
            gh_executable: PathBuf::from("gh"),
            release_ids: BTreeMap::new(),
            latest_read_started: BTreeMap::new(),
        })
    }
    fn execute(&self, command: &mut Command, limit: usize) -> Result<ProcessOutput> {
        self.runner.checked_with_timeout(
            command,
            OutputMode::Capture(limit),
            Duration::from_secs(180),
        )
    }
    fn gh(&self) -> Command {
        let mut command = Command::new(&self.gh_executable);
        command
            .current_dir(self.scratch.path())
            .env("GH_HOST", "github.com")
            .env("GH_PROMPT_DISABLED", "1");
        command
    }
    fn pending_release(&self, tag: &str) -> Result<Option<(u64, bool)>> {
        // This is the same pending-tag lookup used by official gh FetchRelease.
        // REST /releases/tags only discovers published releases. Source/tag/
        // assets are read from the resulting REST record, not inferred here.
        let request = json!({
            "query": "query($owner:String!, $name:String!, $tagName:String!) { repository(owner:$owner,name:$name) { release(tagName:$tagName) { databaseId isDraft } } }",
            "variables": {"owner":"docouno", "name":"devkit", "tagName":tag},
        });
        let input = tempfile::NamedTempFile::new_in(self.scratch.path())?;
        fs::write(input.path(), serde_json::to_vec(&request)?)?;
        let mut command = self.gh();
        command
            .args(["api", "graphql", "--hostname", "github.com", "--input"])
            .arg(input.path());
        let output = self.execute(&mut command, 4 * 1024 * 1024)?;
        let response: Value = serde_json::from_slice(&output.stdout)
            .context("GraphQL release lookup returned malformed JSON")?;
        ensure!(
            response.get("errors").is_none(),
            "GraphQL release lookup returned errors; absence is not established"
        );
        let repository = response
            .get("data")
            .and_then(|v| v.get("repository"))
            .and_then(Value::as_object)
            .context("GraphQL repository is missing or inaccessible; absence is not established")?;
        let release = repository
            .get("release")
            .context("GraphQL release field is missing; absence is not established")?;
        if release.is_null() {
            return Ok(None);
        }
        let id = release
            .get("databaseId")
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .context("GraphQL release databaseId must be a positive integer")?;
        let draft = release
            .get("isDraft")
            .and_then(Value::as_bool)
            .context("GraphQL release isDraft must be a boolean")?;
        Ok(Some((id, draft)))
    }
    fn remember_release(&mut self, tag: &str, release: RemoteRelease) -> Result<RemoteRelease> {
        ensure!(
            release.id > 0 && release.tag_name == tag,
            "GitHub returned another release ID/tag"
        );
        if let Some(id) = self.release_ids.get(tag) {
            ensure!(
                *id == release.id,
                "GitHub release ID changed during this operation; refusing to replace the original release"
            );
        }
        self.release_ids.insert(tag.into(), release.id);
        Ok(release)
    }
    fn api(&self, method: &str, path: &str, body: Option<Value>) -> Result<Option<Value>> {
        self.api_before(method, path, body, None)
    }
    fn api_before(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        deadline: Option<Instant>,
    ) -> Result<Option<Value>> {
        ensure!(
            path.starts_with("repos/docouno/devkit/"),
            "Unexpected GitHub API scope"
        );
        let mut command = self.gh();
        command.args([
            "api",
            "--method",
            method,
            "--include",
            "--hostname",
            "github.com",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            path,
        ]);
        let input = if let Some(body) = body {
            let file = tempfile::NamedTempFile::new_in(self.scratch.path())?;
            fs::write(file.path(), serde_json::to_vec(&body)?)?;
            command.arg("--input").arg(file.path());
            Some(file)
        } else {
            None
        };
        // Keep status information on failures; only an actual HTTP 404 is
        // absence. Network/auth/rate-limit errors always stop the controller.
        let output = self.runner.run_with_timeout(
            &mut command,
            OutputMode::Capture(4 * 1024 * 1024),
            request_timeout(deadline, Instant::now())?,
        )?;
        drop(input);
        github_response(output, method, path)
    }
    fn npm(&self) -> Command {
        let mut command = Command::new("npm");
        // cwd has no package.json or .npmrc. Explicit registry and config paths
        // remove ambient token fallback while retaining npm's GitHub OIDC env.
        command.current_dir(self.scratch.path().join("npm"));
        for (key, _) in std::env::vars_os() {
            let lower = key.to_string_lossy().to_ascii_lowercase();
            if lower.starts_with("npm_config_")
                || [
                    "node_auth_token",
                    "npm_token",
                    "npm_id_token",
                    "node_options",
                ]
                .contains(&lower.as_str())
            {
                command.env_remove(key);
            }
        }
        command
            .env(
                "npm_config_userconfig",
                self.scratch.path().join("user.npmrc"),
            )
            .env(
                "npm_config_globalconfig",
                self.scratch.path().join("global.npmrc"),
            )
            .env("npm_config_cache", self.scratch.path().join("cache"))
            .env("npm_config_update_notifier", "false")
            .args([
                "--registry=https://registry.npmjs.org/",
                "--fetch-retries=0",
                "--fetch-timeout=90000",
            ]);
        command
    }
    fn npm_json_before(
        &self,
        spec: &str,
        field: Option<&str>,
        deadline: Option<Instant>,
    ) -> Result<Option<Value>> {
        let mut command = self.npm();
        command.args(["view", spec]);
        if let Some(field) = field {
            command.arg(field);
        }
        command.arg("--json");
        let output = self.runner.run_with_timeout(
            &mut command,
            OutputMode::Capture(4 * 1024 * 1024),
            request_timeout(deadline, Instant::now())?,
        )?;
        npm_response(output)
    }
    fn fetch_before(
        &self,
        url: &str,
        maximum: u64,
        output: &Path,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "internal-fetch",
                "--url",
                url,
                "--max-bytes",
                &maximum.to_string(),
                "--output",
            ])
            .arg(output);
        self.runner.checked_with_timeout(
            &mut command,
            OutputMode::Capture(8192),
            request_timeout(deadline, Instant::now())?,
        )?;
        Ok(())
    }
    fn npm_version_before(
        &self,
        name: &str,
        version: &str,
        deadline: Option<Instant>,
    ) -> Result<Option<PublishedNpm>> {
        let Some(value) = self.npm_json_before(&format!("{name}@{version}"), None, deadline)?
        else {
            return Ok(None);
        };
        ensure!(
            value["name"] == name && value["version"] == version,
            "Registry returned another package/version"
        );
        let dist = &value["dist"];
        let url = dist["tarball"]
            .as_str()
            .context("Npm tarball URL missing")?;
        let uri: ureq::http::Uri = url.parse()?;
        ensure!(
            uri.scheme_str() == Some("https")
                && uri.authority().map(|a| a.as_str()) == Some("registry.npmjs.org"),
            "Npm tarball must originate at the public npm registry"
        );
        let directory = tempfile::tempdir_in(self.scratch.path())?;
        let path = directory.path().join("package.tgz");
        self.fetch_before(url, 128 * 1024 * 1024, &path, deadline)?;
        Ok(Some(PublishedNpm {
            integrity: dist["integrity"]
                .as_str()
                .context("Npm SRI missing")?
                .into(),
            shasum: dist["shasum"].as_str().context("Npm SHA1 missing")?.into(),
            bytes: read_bounded(&path, 128 * 1024 * 1024)?,
        }))
    }
    fn selectors_before(&self, deadline: Option<Instant>) -> Result<Selectors> {
        let latest = |name| -> Result<Option<String>> {
            self.npm_json_before(name, Some("dist-tags"), deadline)?
                .map(|value| {
                    let tags = value
                        .as_object()
                        .context("Npm dist-tags must be an object")?;
                    tags.get("latest")
                        .map(|v| v.as_str().map(str::to_owned).context("Invalid npm latest"))
                        .transpose()
                })
                .transpose()
                .map(Option::flatten)
        };
        let environment = latest(ENVIRONMENT)?;
        let cli = latest(CLI)?;
        let github = self
            .api_before(
                "GET",
                &format!("repos/{REPO}/releases/latest"),
                None,
                deadline,
            )?
            .map(|value| {
                value["tag_name"]
                    .as_str()
                    .map(str::to_owned)
                    .context("GitHub latest tag missing")
            })
            .transpose()?;
        Ok(Selectors {
            environment,
            cli,
            github,
        })
    }
}

fn request_timeout(deadline: Option<Instant>, now: Instant) -> Result<Duration> {
    let maximum = Duration::from_secs(180);
    match deadline {
        None => Ok(maximum),
        Some(deadline) => deadline
            .checked_duration_since(now)
            .filter(|value| !value.is_zero())
            .map(|remaining| remaining.min(maximum))
            .context("Release read deadline expired; retry against the original frozen set"),
    }
}

fn selector_read_deadline(pending_started: Option<Instant>, now: Instant) -> Instant {
    let cap = now + Duration::from_secs(180);
    pending_started.map_or(cap, |started| (started + readiness::BUDGET).min(cap))
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn callback_delay_cannot_restart_the_acknowledged_channel_deadline() {
        let acknowledged = Instant::now();
        // The waiter calculated its allowance before a scheduler/callback
        // delay. The adapter must use the original Instant, not now+allowance.
        let delayed = acknowledged + Duration::from_secs(355);
        let deadline = selector_read_deadline(Some(acknowledged), delayed);
        assert_eq!(deadline, acknowledged + readiness::BUDGET);
        assert_eq!(
            request_timeout(Some(deadline), delayed).unwrap(),
            Duration::from_secs(5)
        );
        let expired = acknowledged + Duration::from_secs(361);
        assert!(
            request_timeout(
                Some(selector_read_deadline(Some(acknowledged), expired)),
                expired
            )
            .is_err()
        );
        assert_eq!(
            selector_read_deadline(None, expired),
            expired + Duration::from_secs(180)
        );
    }

    #[test]
    fn every_request_uses_fresh_remaining_time_with_the_existing_cap() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(360);
        assert_eq!(
            request_timeout(None, start).unwrap(),
            Duration::from_secs(180)
        );
        assert_eq!(
            request_timeout(Some(deadline), start).unwrap(),
            Duration::from_secs(180)
        );
        assert_eq!(
            request_timeout(Some(deadline), start + Duration::from_secs(353)).unwrap(),
            Duration::from_secs(7)
        );
        assert!(request_timeout(Some(deadline), deadline).is_err());
        assert!(request_timeout(Some(deadline), deadline + Duration::from_secs(1)).is_err());
    }
}
impl Remote for GhNpm {
    fn release(&mut self, tag: &str) -> Result<Option<RemoteRelease>> {
        let published = self.api("GET", &format!("repos/{REPO}/releases/tags/{tag}"), None)?;
        if let Some(value) = published {
            return self
                .remember_release(tag, serde_json::from_value(value)?)
                .map(Some);
        }
        let Some((id, draft)) = self.pending_release(tag)? else {
            ensure!(
                !self.release_ids.contains_key(tag),
                "Previously observed GitHub release is missing; refusing to replace it"
            );
            return Ok(None);
        };
        let value = self
            .api("GET", &format!("repos/{REPO}/releases/{id}"), None)?
            .context(
                "GraphQL release ID is not readable through REST; retry without replacing it",
            )?;
        let release: RemoteRelease = serde_json::from_value(value)?;
        ensure!(
            release.id == id && release.draft == draft,
            "GraphQL and REST release ID/state differ; retry without replacing it"
        );
        self.remember_release(tag, release).map(Some)
    }
    fn create_draft(&mut self, contract: &Contract) -> Result<RemoteRelease> {
        let release = serde_json::from_value(self.api("POST", &format!("repos/{REPO}/releases"), Some(json!({"tag_name":contract.tag(),"target_commitish":contract.source_commit,"name":contract.tag(),"body":contract.body()?,"draft":true,"prerelease":true,"make_latest":"false"})))?.context("Create draft returned no release")?)?;
        self.remember_release(&contract.tag(), release)
    }
    fn download_asset(
        &mut self,
        _release: &RemoteRelease,
        asset: &RemoteAsset,
        path: &Path,
    ) -> Result<()> {
        ensure!(
            asset.size <= 128 * 1024 * 1024,
            "Remote asset exceeds its byte budget"
        );
        let mut command = self.gh();
        command.args([
            "api",
            "--hostname",
            "github.com",
            "--header",
            "Accept: application/octet-stream",
            &format!("repos/{REPO}/releases/assets/{}", asset.id),
        ]);
        let output = self.execute(&mut command, 128 * 1024 * 1024)?;
        ensure!(
            output.stdout.len() as u64 == asset.size,
            "GitHub asset download was truncated"
        );
        ensure!(!path.exists(), "Download destination must be new");
        files::write_atomic(path, &output.stdout, 0o644)
    }
    fn upload_asset(&mut self, release: &RemoteRelease, path: &Path) -> Result<()> {
        ensure!(
            release.draft && !path.as_os_str().as_encoded_bytes().contains(&b'#'),
            "Upload requires a draft and a path without gh's label separator"
        );
        let mut command = self.gh();
        command
            .args(["release", "upload", &release.tag_name, "--repo", REPO])
            .arg(path);
        self.execute(&mut command, 64 * 1024)?;
        Ok(())
    }
    fn npm_version(&mut self, name: &str, version: &str) -> Result<Option<PublishedNpm>> {
        self.npm_version_before(name, version, None)
    }
    fn npm_after_publish(&mut self, name: &str, version: &str) -> Result<PublishedNpm> {
        let started = Instant::now();
        let deadline = started + readiness::BUDGET;
        let runner = self.runner.clone();
        readiness::wait(
            name,
            version,
            || started.elapsed(),
            |_remaining| self.npm_version_before(name, version, Some(deadline)),
            |duration| readiness::pause(&runner, duration),
        )
    }
    fn publish_npm(&mut self, path: &Path, tag: &str) -> Result<()> {
        let mut command = self.npm();
        command.arg("publish").arg(path).args([
            "--ignore-scripts",
            "--access=public",
            "--provenance",
            "--tag",
            tag,
        ]);
        self.execute(&mut command, 128 * 1024)?;
        Ok(())
    }
    fn selectors(&mut self) -> Result<Selectors> {
        self.selectors_before(None)
    }
    fn selectors_after_latest(
        &mut self,
        contract: &Contract,
        acknowledged: &BTreeMap<String, String>,
        pending: Option<&str>,
    ) -> Result<Selectors> {
        ensure!(
            acknowledged
                .keys()
                .all(|name| self.latest_read_started.contains_key(name)),
            "Latest visibility wait requires a successful npm write"
        );
        let runner = self.runner.clone();
        readiness::wait_latest(
            contract,
            acknowledged,
            pending,
            |name| self.latest_read_started[name].elapsed(),
            |_remaining, pending| {
                self.selectors_before(Some(selector_read_deadline(
                    pending.map(|name| self.latest_read_started[name]),
                    Instant::now(),
                )))
            },
            |duration| readiness::pause(&runner, duration),
        )
    }
    fn npm_latest(&mut self, name: &str, version: &str) -> Result<()> {
        let mut command = self.npm();
        command.args(["dist-tag", "add", &format!("{name}@{version}"), "latest"]);
        self.execute(&mut command, 64 * 1024)?;
        self.latest_read_started
            .entry(name.into())
            .or_insert_with(Instant::now);
        Ok(())
    }
    fn edit_release(
        &mut self,
        release: &RemoteRelease,
        body: &str,
        channel: Option<bool>,
    ) -> Result<()> {
        let mut value = json!({"body":body});
        if let Some(stable) = channel {
            value["draft"] = json!(false);
            value["prerelease"] = json!(!stable);
            value["make_latest"] = json!(if stable { "true" } else { "false" });
        }
        self.api(
            "PATCH",
            &format!("repos/{REPO}/releases/{}", release.id),
            Some(value),
        )?
        .context("Update release returned no result")?;
        Ok(())
    }
    fn tag_source(&mut self, tag: &str) -> Result<Option<(String, String)>> {
        let Some(reference) = self.api("GET", &format!("repos/{REPO}/git/ref/tags/{tag}"), None)?
        else {
            return Ok(None);
        };
        ensure!(
            reference["ref"] == format!("refs/tags/{tag}"),
            "GitHub returned a different tag reference"
        );
        let mut object = reference["object"].clone();
        let mut seen = BTreeSet::new();
        for _ in 0..8 {
            let sha = object["sha"].as_str().context("Git object SHA missing")?;
            ensure!(
                source_hash(sha) && seen.insert(sha.to_owned()),
                "Invalid or cyclic Git tag chain"
            );
            match object["type"].as_str() {
                Some("tag") => {
                    let tag = self
                        .api("GET", &format!("repos/{REPO}/git/tags/{sha}"), None)?
                        .context("Annotated Git tag missing")?;
                    ensure!(tag["sha"] == sha, "Annotated tag object differs");
                    object = tag["object"].clone();
                }
                Some("commit") => {
                    let commit = self
                        .api("GET", &format!("repos/{REPO}/git/commits/{sha}"), None)?
                        .context("Tagged commit missing")?;
                    let tree = commit["tree"]["sha"]
                        .as_str()
                        .context("Tagged tree missing")?;
                    ensure!(
                        commit["sha"] == sha && source_hash(tree),
                        "Tagged commit identity differs"
                    );
                    return Ok(Some((sha.into(), tree.into())));
                }
                _ => bail!("Release tag does not resolve to a Git commit"),
            }
        }
        bail!("Annotated release tag chain exceeds its bound")
    }
    fn main_contains(&mut self, sha: &str, tree: &str) -> Result<()> {
        ensure!(
            source_hash(sha) && source_hash(tree),
            "Invalid source identity"
        );
        let commit = self
            .api("GET", &format!("repos/{REPO}/git/commits/{sha}"), None)?
            .context("Reviewed source commit is not in the repository")?;
        ensure!(
            commit["sha"] == sha && commit["tree"]["sha"] == tree,
            "GitHub source tree differs"
        );
        let comparison = self
            .api("GET", &format!("repos/{REPO}/compare/{sha}...main"), None)?
            .context("Cannot establish source/main ancestry")?;
        ensure!(
            matches!(comparison["status"].as_str(), Some("identical" | "ahead")),
            "Release source must be reviewed main or its ancestor"
        );
        Ok(())
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    fn output(code: u8, stdout: &str) -> ProcessOutput {
        ProcessOutput {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"provider diagnostic".to_vec(),
        }
    }
    #[test]
    fn controller_publication_boundary_requires_authentic_event_source() {
        let source = "a".repeat(40);
        assert!(require_event_source(&source, None).is_err());
        let different = "b".repeat(40);
        let error = require_event_source(&source, Some(&different)).unwrap_err();
        assert!(error.to_string().contains("rerun its original run"));
        assert!(require_event_source(&source, Some(&source)).is_ok());
    }
    #[test]
    fn actual_absence_is_distinct_from_auth_transport_rate_limit_and_bad_json() {
        assert!(
            github_response(output(1, "HTTP/2.0 404 Not Found\r\n\r\n{}"), "GET", "test")
                .unwrap()
                .is_none()
        );
        assert!(
            github_response(
                output(1, "HTTP/2.0 404 Not Found\r\n\r\n{}"),
                "POST",
                "test"
            )
            .is_err()
        );
        for response in [
            "",
            "connection failed",
            "HTTP/2.0 403 Forbidden\n\n{}",
            "HTTP/2.0 429 Too Many Requests\n\n{}",
            "HTTP/2.0 200 OK\n\ntruncated",
        ] {
            assert!(github_response(output(1, response), "GET", "test").is_err());
        }
        assert_eq!(
            github_response(output(0, "HTTP/2.0 200 OK\n\n{\"id\":7}"), "GET", "test")
                .unwrap()
                .unwrap()["id"],
            7
        );
        assert!(
            npm_response(output(1, r#"{"error":{"code":"E404"}}"#))
                .unwrap()
                .is_none()
        );
        for code in ["E403", "E401", "ENOTFOUND", "ETIMEDOUT", "E429"] {
            assert!(
                npm_response(output(1, &format!(r#"{{"error":{{"code":"{code}"}}}}"#))).is_err()
            );
        }
        assert!(npm_response(output(0, r#"{"error":{"code":"E404"}}"#)).is_err());
        assert!(npm_response(output(1, "")).is_err());
    }
    #[test]
    fn npm_adapter_uses_ephemeral_config_and_exact_public_registry() {
        let provider = GhNpm::new(ProcessRunner::new().unwrap()).unwrap();
        let command = provider.npm();
        let args: Vec<_> = command
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--registry=https://registry.npmjs.org/".to_owned()));
        let configs: Vec<_> = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        for name in [
            "npm_config_userconfig",
            "npm_config_globalconfig",
            "npm_config_cache",
        ] {
            assert!(configs.iter().any(|(key, value)| key == name
                && value.starts_with(provider.scratch.path().to_str().unwrap())));
        }
    }
}
