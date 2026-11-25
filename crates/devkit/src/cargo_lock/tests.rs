use super::*;

const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn package(name: &str, version: &str, source: Option<&str>, deps: &[&str]) -> String {
    let mut text = format!("[[package]]\nname='{name}'\nversion='{version}'\n");
    if let Some(source) = source {
        text.push_str(&format!("source='{source}'\n"));
    }
    if !deps.is_empty() {
        let deps = deps
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("dependencies=[{deps}]\n"));
    }
    text
}

fn input(path: &str, records: &[String]) -> (String, Vec<u8>) {
    (
        format!("{path}/Cargo.lock"),
        format!("version = 4\n{}", records.concat()).into_bytes(),
    )
}

#[test]
fn migration_reencodes_edges_at_all_three_ambiguity_levels() {
    let a = input(
        "apps/a",
        &[
            package("a", "0.1.0", None, &["helper", "syn", "leaf"]),
            package("helper", "1.0.0", None, &[]),
            package("syn", "1.0.0", Some(REGISTRY), &[]),
            package("leaf", "2.5.0", Some(REGISTRY), &[]),
        ],
    );
    let b = input(
        "apps/b",
        &[
            package("b", "0.1.0", None, &["helper 1.0.0", "helper 1.0.1", "syn"]),
            package("helper", "1.0.0", Some(REGISTRY), &[]),
            package("helper", "1.0.1", None, &[]),
            package("syn", "2.0.0", Some(REGISTRY), &[]),
        ],
    );
    let locks = vec![a, b];
    let bytes = merge_legacy_locks(&locks).unwrap();
    let parsed = Lock::parse(&bytes).unwrap();
    for (_, source) in parse_inputs(&locks).unwrap() {
        for (id, record) in &source.packages {
            assert_eq!(parsed.packages[id], *record);
        }
    }
    let text = String::from_utf8(bytes).unwrap();
    for dep in [
        "helper 1.0.0",
        "helper 1.0.1",
        "syn 1.0.0",
        "syn 2.0.0",
        "leaf",
    ] {
        assert!(text.contains(&format!("\"{dep}\",")));
    }
    assert!(text.contains(&format!("\"helper 1.0.0 ({REGISTRY})\",")));
}

#[test]
fn strict_merges_reject_every_cargo_activation_collision_with_its_provenance() {
    for (left, right) in [
        ("1.0.228", "1.0.229"),
        ("1.0.0-alpha.1", "1.0.0-alpha.2"),
        ("1.0.0-alpha.1", "1.0.0"),
        ("0.1.0-alpha.1", "0.1.0"),
        ("0.0.0-alpha.1", "0.0.0"),
        ("1.0.0+b1", "1.0.0+b2"),
    ] {
        let locks = [
            input("apps/server", &[package("dep", left, Some(REGISTRY), &[])]),
            input("apps/worker", &[package("dep", right, Some(REGISTRY), &[])]),
        ];
        let err = merge_legacy_locks(&locks).unwrap_err().to_string();
        for expected in [
            left,
            right,
            "apps/server/Cargo.lock",
            "apps/worker/Cargo.lock",
            "0.5.x",
        ] {
            assert!(err.contains(expected), "{err}");
        }
    }
}

#[test]
fn strict_merges_compare_resolved_edges_and_checksums_independently_of_spelling() {
    let lock = |dep: &str, checksum: &str, origin: &str| {
        input(
            origin,
            &[
                package("shared", "1.0.0", Some(REGISTRY), &[dep]),
                format!("checksum='{checksum}'\n"),
                package("syn", "1.0.0", Some(REGISTRY), &[]),
                package("syn", "2.0.0", Some(REGISTRY), &[]),
            ],
        )
    };
    for (dep, checksum) in [("syn 2.0.0", "a"), ("syn 1.0.0", "b")] {
        let error =
            merge_legacy_locks(&[lock("syn 1.0.0", "a", "old"), lock(dep, checksum, "new")])
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("old/Cargo.lock")
                && error.contains("new/Cargo.lock")
                && error.contains("shared"),
            "{error}"
        );
    }
    let locks = [
        lock("syn 1.0.0", "a", "old"),
        lock(&format!("syn 1.0.0 ({REGISTRY})"), "a", "new"),
    ];
    assert!(merge_legacy_locks(&locks).is_ok());
}

#[test]
fn merge_preserves_replacements_unused_patches_metadata_and_v3_git_references() {
    let git = "git+https://example.org/crate?branch=topic/test#123abc";
    let git_ref = "git+https://example.org/crate?branch=topic/test";
    let old = format!(
        "version=3\n{}{}{}{}{}",
        package("a", "0.1.0", None, &[&format!("dep 1.0.0 ({REGISTRY})")]),
        package("dep", "1.0.0", Some(REGISTRY), &[]),
        format_args!("replace='dep 1.0.0 ({git_ref})'\n"),
        package("dep", "1.0.0", Some(git), &[]),
        "[[patch.unused]]\nname='unused'\nversion='2.0.0'\n[metadata]\nowner='product'\n"
    );
    let inputs = [("apps/a/Cargo.lock".into(), old.into_bytes())];
    let merged = String::from_utf8(merge_legacy_locks(&inputs).unwrap()).unwrap();
    assert!(merged.contains("version = 4"));
    assert!(merged.contains("topic%2Ftest#123abc"));
    assert!(merged.contains("topic%2Ftest)"));
    let roundtrip = Lock::parse(merged.as_bytes()).unwrap();
    assert_eq!(roundtrip.metadata["owner"], "product");
    assert_eq!(roundtrip.unused.keys().next().unwrap().name, "unused");
}

#[test]
fn unused_patch_checksums_merge_independently_of_declaration_order() -> Result<()> {
    let annotation = |checksum: &str| {
        format!("[[patch.unused]]\nname='widget'\nversion='1.0.0'\nsource='{REGISTRY}'\n{checksum}")
    };
    let bare = annotation("");
    let checksum = "a".repeat(64);
    let known = annotation(&format!("checksum='{checksum}'\n"));
    let different = annotation(&format!("checksum='{}'\n", "b".repeat(64)));
    let source = |name: &str, entries: &[&str]| {
        input(name, &[package(name, "1.0.0", None, &[]), entries.concat()])
    };
    for (first, second) in [(&bare, &known), (&known, &bare)] {
        let a = source("a", &[first, second]);
        let b = source("b", &[first]);
        for pair in [[a.clone(), b.clone()], [b, a]] {
            let merged = Lock::parse(&merge_legacy_locks(&pair)?)?;
            let patch = merged.unused.values().next().unwrap();
            assert_eq!(patch.checksum.as_ref(), Some(&checksum));
            assert_eq!(patch.occurrences, 2);
        }
    }
    assert!(Lock::parse(&source("a", &[&known, &different]).1).is_err());
    assert!(merge_legacy_locks(&[source("a", &[&known]), source("b", &[&different])]).is_err());
    Ok(())
}

#[test]
fn seed_version_conflicts_reach_the_resolver_before_transitive_edge_comparison() {
    let seed = |version: &str, owner: &str| {
        input(
            owner,
            &[
                package("alpha", "1.0.0", Some(REGISTRY), &["zeta"]),
                package("zeta", version, Some(REGISTRY), &[]),
            ],
        )
    };
    let locks = parse_inputs(&[seed("1.0.0", "a"), seed("1.0.1", "b")]).unwrap();
    assert!(matches!(
        merge(&locks).unwrap().conflict,
        Some(MergeConflict::Versions(_))
    ));
}

#[test]
fn v3_git_refs_encode_the_literal_ref_and_leave_the_precise_commit_in_the_record() {
    // These v3/v4 source pairs are emitted by Cargo for a crate declaring
    // rust-version 1.78 and 1.86 respectively, against the same Git branches.
    for (raw, escaped) in [
        ("topic+branch", "topic%2Bbranch"),
        ("topic%2Fbranch", "topic%252Fbranch"),
        ("topic/test", "topic%2Ftest"),
        ("topic&key=value", "topic%26key%3Dvalue"),
        ("topic#branch", "topic%23branch"),
        ("topic!$-_.()", "topic%21%24-_.%28%29"),
    ] {
        let v3 = format!("git+https://example.org/crate?branch={raw}");
        let v4 = format!("git+https://example.org/crate?branch={escaped}");
        let precise = "e76ccd6fefbc25d54ad47fcc1395a2cfff75ab2c";
        let origin = package("edge", "1.0.0", Some(&format!("{v3}#{precise}")), &[]);
        let registry = package("edge", "1.0.0", Some(REGISTRY), &[]);
        let member = package("app", "1.0.0", None, &[&format!("edge 1.0.0 ({v3})")]);
        let old = format!("version=3\n{member}{registry}{origin}");
        let graph = Lock::parse(old.as_bytes()).unwrap();
        let merged = String::from_utf8(graph.bytes().unwrap()).unwrap();
        assert!(merged.contains(&format!("{v4}#{precise}")), "{merged}");
        assert!(merged.contains(&format!("edge 1.0.0 ({v4})")), "{merged}");
        assert!(Lock::parse(merged.as_bytes()).unwrap() == graph);
    }
}

#[test]
#[ignore = "requires the pinned author Cargo and Git"]
fn pinned_cargo_metadata_accepts_the_v4_merge_of_a_literal_v3_git_ref() -> Result<()> {
    use std::{fs, path::Path, process::Command};

    fn crate_at(root: &Path, name: &str, rust_version: &str, dependencies: &str) -> Result<()> {
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/lib.rs"), "")?;
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname='{name}'\nversion='1.0.0'\nedition='2021'\nrust-version='{rust_version}'\n{dependencies}"
            ),
        )?;
        Ok(())
    }
    fn run(mut command: Command) -> Result<()> {
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    let temp = tempfile::tempdir()?;
    let origin = temp.path().join("origin");
    crate_at(&origin, "edge", "1.78", "")?;
    let branch = "topic+literal/%2F&other=value#part";
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "Create fixture",
        ],
        &["branch", branch],
    ] {
        let mut git = Command::new("git");
        git.current_dir(&origin)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args);
        run(git)?;
    }
    let origin_url = url::Url::from_file_path(&origin).unwrap();
    let dep = format!("[dependencies]\nedge={{git='{origin_url}',branch='{branch}'}}\n");
    let cargo = |dir: &Path, args: &[&str]| {
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(dir)
            .env("CARGO_HOME", temp.path().join("cache"))
            .args(args);
        run(cargo)
    };
    let legacy = temp.path().join("legacy");
    let current = temp.path().join("current");
    for (root, minimum) in [(&legacy, "1.78"), (&current, "1.83")] {
        crate_at(root, "app", minimum, &dep)?;
        cargo(root, &["generate-lockfile"])?;
    }
    let legacy_path = legacy.join("Cargo.lock");
    let v3 = fs::read_to_string(&legacy_path)?;
    assert!(v3.contains("version = 3") && v3.contains(branch));
    let merged = merge_legacy_locks(&[("legacy/Cargo.lock".to_owned(), v3.into_bytes())])?;
    let expected = fs::read(current.join("Cargo.lock"))?;
    assert_eq!(merged, expected);
    fs::write(legacy_path, merged)?;
    cargo(
        &legacy,
        &["metadata", "--locked", "--offline", "--format-version=1"],
    )
}

#[test]
fn lock_bytes_orders_packages_by_semver_and_source_kind() {
    let git = "git+https://example.invalid/repo?branch=main#0123456789abcdef";
    let sparse = "sparse+https://example.invalid/index/";
    let text = format!(
        "version = 4\n\n\
         [[package]]\nname = 'z'\nversion = '0.10.0'\nsource = '{REGISTRY}'\n\n\
         [[package]]\nname = 'z'\nversion = '0.8.5'\nsource = '{REGISTRY}'\n\n\
         [[package]]\nname = 'a'\nversion = '1.0.0'\nsource = '{sparse}'\n\n\
         [[package]]\nname = 'a'\nversion = '1.0.0'\nsource = '{REGISTRY}'\n\n\
         [[package]]\nname = 'a'\nversion = '1.0.0'\n\n\
         [[package]]\nname = 'a'\nversion = '1.0.0'\nsource = '{git}'\n"
    );
    let parsed = Lock::parse(text.as_bytes()).unwrap();
    let rendered = String::from_utf8(parsed.bytes().unwrap()).unwrap();
    let doc: toml::Value = toml::from_str(&rendered).unwrap();
    let order: Vec<_> = doc["package"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap(),
                p["version"].as_str().unwrap(),
                p.get("source").and_then(toml::Value::as_str),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            ("a", "1.0.0", Some(git)),
            ("a", "1.0.0", None),
            ("a", "1.0.0", Some(REGISTRY)),
            ("a", "1.0.0", Some(sparse)),
            ("z", "0.8.5", Some(REGISTRY)),
            ("z", "0.10.0", Some(REGISTRY)),
        ]
    );
}

#[test]
fn seed_pin_check_allows_transitive_upgrade_behind_redirected_winner_only() {
    // Live workspace has serde 1.0.230; incoming template seed has serde 1.0.229 -> serde_derive 1.0.229
    // plus an unrelated crate `other 1.0.0`.
    let live = Lock::parse(
        format!(
            "version = 4\n\
             [[package]]\nname = 'existing'\nversion = '1.0.0'\ndependencies = ['serde']\n\
             [[package]]\nname = 'serde'\nversion = '1.0.230'\nsource = '{REGISTRY}'\nchecksum = '{}'\n",
            "1".repeat(64)
        )
        .as_bytes(),
    )
    .unwrap();
    let seed = Lock::parse(
        format!(
            "version = 4\n\
             [[package]]\nname = 'incoming'\nversion = '1.0.0'\ndependencies = ['serde', 'other']\n\
             [[package]]\nname = 'serde'\nversion = '1.0.229'\nsource = '{REGISTRY}'\nchecksum = '{}'\ndependencies = ['serde_derive']\n\
             [[package]]\nname = 'serde_derive'\nversion = '1.0.229'\nsource = '{REGISTRY}'\nchecksum = '{}'\n\
             [[package]]\nname = 'other'\nversion = '1.0.0'\nsource = '{REGISTRY}'\nchecksum = '{}'\n",
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
        )
        .as_bytes(),
    )
    .unwrap();
    let merged = super::merge(&[
        ("Cargo.lock".into(), live),
        ("apps/api/Cargo.lock".into(), seed),
    ])
    .unwrap();
    assert!(merged.redirected_winners.contains(&CargoPackageId {
        name: "serde".into(),
        version: "1.0.230".into(),
        source: Some(REGISTRY.into()),
    }));
    // Cargo resolves serde 1.0.230 with serde_derive =1.0.230 while keeping other 1.0.0.
    let valid_resolved = Lock::parse(
        format!(
            "version = 4\n\
             [[package]]\nname = 'existing'\nversion = '1.0.0'\ndependencies = ['serde']\n\
             [[package]]\nname = 'incoming'\nversion = '1.0.0'\ndependencies = ['serde', 'other']\n\
             [[package]]\nname = 'serde'\nversion = '1.0.230'\nsource = '{REGISTRY}'\nchecksum = '{}'\ndependencies = ['serde_derive']\n\
             [[package]]\nname = 'serde_derive'\nversion = '1.0.230'\nsource = '{REGISTRY}'\nchecksum = '{}'\n\
             [[package]]\nname = 'other'\nversion = '1.0.0'\nsource = '{REGISTRY}'\nchecksum = '{}'\n",
            "1".repeat(64),
            "5".repeat(64),
            "4".repeat(64),
        )
        .as_bytes(),
    )
    .unwrap();
    super::reconcile::check_seed_pins_for_test(
        &merged.seed,
        &valid_resolved,
        &merged.redirected_winners,
    )
    .unwrap();

    // Unrelated incoming pin drift (`other 1.0.0 -> 1.0.1`) is still rejected.
    let drifted_other = Lock::parse(
        format!(
            "version = 4\n\
             [[package]]\nname = 'existing'\nversion = '1.0.0'\ndependencies = ['serde']\n\
             [[package]]\nname = 'incoming'\nversion = '1.0.0'\ndependencies = ['serde', 'other']\n\
             [[package]]\nname = 'serde'\nversion = '1.0.230'\nsource = '{REGISTRY}'\nchecksum = '{}'\ndependencies = ['serde_derive']\n\
             [[package]]\nname = 'serde_derive'\nversion = '1.0.230'\nsource = '{REGISTRY}'\nchecksum = '{}'\n\
             [[package]]\nname = 'other'\nversion = '1.0.1'\nsource = '{REGISTRY}'\nchecksum = '{}'\n",
            "1".repeat(64),
            "5".repeat(64),
            "6".repeat(64),
        )
        .as_bytes(),
    )
    .unwrap();
    assert!(
        super::reconcile::check_seed_pins_for_test(
            &merged.seed,
            &drifted_other,
            &merged.redirected_winners,
        )
        .is_err()
    );
}
