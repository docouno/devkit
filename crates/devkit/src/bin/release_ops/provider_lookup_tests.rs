//! Exercise the actual gh command adapter, including exit/status/JSON handling.
use super::*;

struct Fixture {
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let script = r#"#!/bin/sh
set -eu
root=${0%/*}
printf '%s\n' "$*" >> "$root/calls"
input=''
previous=''
for arg in "$@"; do
  if test "$previous" = --input; then input=$arg; fi
  previous=$arg
done
case "$*" in
  'api graphql '*) cp "$input" "$root/graphql-request.json"; response=graphql ;;
  *'repos/docouno/devkit/releases/tags/'*) response=rest-tag ;;
  *'--method POST'*) response=create ;;
  *'repos/docouno/devkit/releases/'*) response=rest-id ;;
  *) printf 'unexpected adapter request\n' >&2; exit 99 ;;
esac
cat "$root/$response"
exit "$(cat "$root/$response-code")"
"#;
        files::write_atomic(&root.join("gh"), script.as_bytes(), 0o755).unwrap();
        let fixture = Self { directory };
        fixture.rest("rest-tag", 404, json!({"message":"Not Found"}));
        fixture.graphql(
            json!({"data":{"repository":{"release":{"databaseId":42,"isDraft":true}}}}),
            0,
        );
        fixture.rest("rest-id", 200, release(42, true));
        fixture.rest("create", 201, release(42, true));
        fixture
    }
    fn root(&self) -> &Path {
        self.directory.path()
    }
    fn provider(&self) -> GhNpm {
        let mut provider = GhNpm::new(ProcessRunner::new().unwrap()).unwrap();
        provider.gh_executable = self.root().join("gh");
        provider
    }
    fn rest(&self, name: &str, status: u16, value: Value) {
        fs::write(
            self.root().join(name),
            format!(
                "HTTP/2.0 {status} fixture\n\n{}",
                serde_json::to_string(&value).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            self.root().join(format!("{name}-code")),
            if status < 300 { "0" } else { "1" },
        )
        .unwrap();
    }
    fn graphql(&self, value: Value, code: u8) {
        fs::write(
            self.root().join("graphql"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        fs::write(self.root().join("graphql-code"), code.to_string()).unwrap();
    }
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.root().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
fn contract() -> Contract {
    Contract {
        schema_version: 1,
        repository: REPO.into(),
        version: "0.3.0".into(),
        source_commit: "a".repeat(40),
        source_tree: "b".repeat(40),
        build_run: "https://github.com/docouno/devkit/actions/runs/123/attempts/1".into(),
        index_sha256: "c".repeat(64),
        previous: Selectors {
            environment: Some("0.2.1".into()),
            cli: Some("0.0.0-stage".into()),
            github: Some("v0.2.1".into()),
        },
    }
}
fn release(id: u64, draft: bool) -> Value {
    json!({"id":id,"tag_name":"v0.3.0","target_commitish":"a".repeat(40),"draft":draft,"prerelease":true,"immutable":!draft,"body":contract().body().unwrap(),"assets":[]})
}

#[test]
fn published_rest_fastpath_does_not_query_graphql() {
    let fixture = Fixture::new();
    fixture.rest("rest-tag", 200, release(42, false));
    fixture.graphql(json!({"errors":[{"message":"must not be called"}]}), 1);
    let found = fixture.provider().release("v0.3.0").unwrap().unwrap();
    assert_eq!(found.id, 42);
    assert!(!found.draft);
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn actual_adapter_finds_and_reloads_pending_draft_after_published_404() {
    let fixture = Fixture::new();
    let mut provider = fixture.provider();
    for _ in 0..2 {
        let found = provider.release("v0.3.0").unwrap().unwrap();
        assert_eq!(found.id, 42);
        assert!(found.draft);
        assert_eq!(found.contract().unwrap(), contract());
    }
    assert_eq!(fixture.calls().len(), 6);
    let request: Value = files::read_json(&fixture.root().join("graphql-request.json")).unwrap();
    assert_eq!(
        request["variables"],
        json!({"owner":"docouno","name":"devkit","tagName":"v0.3.0"})
    );
    assert!(
        request["query"]
            .as_str()
            .unwrap()
            .contains("release(tagName:$tagName)")
    );
}

#[test]
fn explicit_null_release_alone_proves_absence() {
    let fixture = Fixture::new();
    fixture.graphql(json!({"data":{"repository":{"release":null}}}), 0);
    assert!(fixture.provider().release("v0.3.0").unwrap().is_none());
    assert_eq!(fixture.calls().len(), 2);
}

#[test]
fn graphql_published_fallback_preserves_legitimate_state_transition() {
    let fixture = Fixture::new();
    let mut provider = fixture.provider();
    assert!(provider.release("v0.3.0").unwrap().unwrap().draft);
    fixture.graphql(
        json!({"data":{"repository":{"release":{"databaseId":42,"isDraft":false}}}}),
        0,
    );
    fixture.rest("rest-id", 200, release(42, false));
    let published = provider.release("v0.3.0").unwrap().unwrap();
    assert_eq!(published.id, 42);
    assert!(!published.draft);
    assert!(published.immutable);
}

#[test]
fn rest_authorization_rate_limit_and_network_failure_never_enter_fallback() {
    for status in [401, 403, 429, 500] {
        let fixture = Fixture::new();
        fixture.rest("rest-tag", status, json!({"message":"provider failure"}));
        assert!(fixture.provider().release("v0.3.0").is_err());
        assert_eq!(fixture.calls().len(), 1);
    }
    let fixture = Fixture::new();
    fs::write(fixture.root().join("rest-tag"), b"").unwrap();
    assert!(fixture.provider().release("v0.3.0").is_err());
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn graphql_errors_transport_and_unreadable_repository_never_mean_absence() {
    for response in [
        json!({"errors":[{"message":"forbidden"}],"data":{"repository":{"release":null}}}),
        json!({"data":{"repository":null}}),
        json!({"data":{}}),
        json!({"data":{"repository":{}}}),
        json!({}),
    ] {
        let fixture = Fixture::new();
        fixture.graphql(response, 0);
        assert!(fixture.provider().release("v0.3.0").is_err());
        assert_eq!(fixture.calls().len(), 2);
    }
    for response in [
        json!({"message":"HTTP403"}),
        json!({"errors":[{"message":"rate limit"}]}),
    ] {
        let fixture = Fixture::new();
        fixture.graphql(response, 1);
        assert!(fixture.provider().release("v0.3.0").is_err());
    }
    let fixture = Fixture::new();
    fs::write(fixture.root().join("graphql"), b"truncated JSON").unwrap();
    assert!(fixture.provider().release("v0.3.0").is_err());
    fs::write(fixture.root().join("graphql"), b"").unwrap();
    fs::write(fixture.root().join("graphql-code"), "1").unwrap();
    assert!(fixture.provider().release("v0.3.0").is_err());
}

#[test]
fn graphql_location_requires_positive_integer_id_and_boolean_state() {
    for id in [json!(null), json!(0), json!(-1), json!(42.5), json!("42")] {
        let fixture = Fixture::new();
        fixture.graphql(
            json!({"data":{"repository":{"release":{"databaseId":id,"isDraft":true}}}}),
            0,
        );
        assert!(fixture.provider().release("v0.3.0").is_err());
    }
    let fixture = Fixture::new();
    fixture.graphql(
        json!({"data":{"repository":{"release":{"databaseId":42,"isDraft":"true"}}}}),
        0,
    );
    assert!(fixture.provider().release("v0.3.0").is_err());
}

#[test]
fn fallback_rest_must_match_exact_location_tag_and_state() {
    for value in [release(43, true), release(42, false), {
        let mut wrong_tag = release(42, true);
        wrong_tag["tag_name"] = json!("v0.3.1");
        wrong_tag
    }] {
        let fixture = Fixture::new();
        fixture.rest("rest-id", 200, value);
        assert!(fixture.provider().release("v0.3.0").is_err());
    }
    for status in [404, 403] {
        let fixture = Fixture::new();
        fixture.rest("rest-id", status, json!({"message":"not readable"}));
        assert!(fixture.provider().release("v0.3.0").is_err());
    }
    let fixture = Fixture::new();
    let mut wrong = release(42, false);
    wrong["tag_name"] = json!("v0.3.1");
    fixture.rest("rest-tag", 200, wrong);
    assert!(fixture.provider().release("v0.3.0").is_err());
}

#[test]
fn observed_or_created_release_id_cannot_be_silently_replaced() {
    for created in [false, true] {
        let fixture = Fixture::new();
        let mut provider = fixture.provider();
        if created {
            assert_eq!(provider.create_draft(&contract()).unwrap().id, 42);
        } else {
            assert_eq!(provider.release("v0.3.0").unwrap().unwrap().id, 42);
        }
        fixture.rest("rest-tag", 200, release(43, false));
        assert!(provider.release("v0.3.0").is_err());
        fixture.rest("rest-tag", 404, json!({"message":"Not Found"}));
        fixture.graphql(json!({"data":{"repository":{"release":null}}}), 0);
        assert!(provider.release("v0.3.0").is_err());
    }
}

#[test]
#[ignore = "explicit operator-supplied existing draft; authenticated read-only"]
fn actual_existing_release_lookup_readonly() {
    let tag = std::env::var("DEVKIT_RELEASE_PROBE_TAG").expect("explicit release tag required");
    let expected_id: u64 = std::env::var("DEVKIT_RELEASE_PROBE_ID")
        .expect("explicit release id required")
        .parse()
        .unwrap();
    let expected_index_hash = std::env::var("DEVKIT_RELEASE_PROBE_INDEX_SHA256")
        .expect("explicit frozen index SHA256 required");
    let mut provider = GhNpm::new(ProcessRunner::new().unwrap()).unwrap();
    let first = provider
        .release(&tag)
        .unwrap()
        .expect("existing release must be found");
    assert_eq!(first.id, expected_id);
    let contract = first.contract().unwrap();
    assert_eq!(contract.index_sha256, expected_index_hash);
    let asset = first
        .assets
        .iter()
        .find(|a| a.name == "release-index.json")
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let index_path = directory.path().join("release-index.json");
    provider.download_asset(&first, asset, &index_path).unwrap();
    let raw = fs::read(index_path).unwrap();
    assert_eq!(digest(&raw), expected_index_hash);
    let index: ReleaseIndex = serde_json::from_slice(&raw).unwrap();
    index.validate().unwrap();
    assert_eq!(contract.source_commit, index.source_commit);
    assert_eq!(contract.source_tree, index.source_tree);
    assert_eq!(contract.build_run, index.build_run);
    assert_eq!(first.target_commitish, index.source_commit);
    let expected_names: BTreeSet<_> = index
        .files
        .keys()
        .map(String::as_str)
        .chain(["release-index.json"])
        .collect();
    assert_eq!(
        first
            .assets
            .iter()
            .map(|a| a.name.as_str())
            .collect::<BTreeSet<_>>(),
        expected_names
    );
    for asset in &first.assets {
        let (size, sha) = if asset.name == "release-index.json" {
            (raw.len() as u64, expected_index_hash.as_str())
        } else {
            let value = &index.files[&asset.name];
            (value.size, value.sha256.as_str())
        };
        assert_eq!(asset.size, size);
        assert_eq!(
            asset.digest.as_deref(),
            Some(format!("sha256:{sha}").as_str())
        );
        assert_eq!(asset.state, "uploaded");
    }
    let second = provider.release(&tag).unwrap().unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.contract().unwrap(), contract);
    assert_eq!(second.assets.len(), first.assets.len());
    println!(
        "READONLY release id={} tag={} draft={} assets={} index={} source={}",
        second.id,
        tag,
        second.draft,
        second.assets.len(),
        expected_index_hash,
        index.source_commit
    );
}
