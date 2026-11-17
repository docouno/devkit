use devkit::{
    files::*,
    stand::{epoch, ports::*},
    state::CheckoutLock,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    net::{TcpListener, UdpSocket},
    path::Path,
};

fn bindings(replicas: u16) -> Vec<Binding> {
    declarations(&json!({"services":{"web":{"deploy":{"replicas":replicas},"ports":[{"target":3000,"published":"0","host_ip":"127.0.0.1","protocol":"tcp"}]}}}), &BTreeSet::from(["web".into()])).unwrap()
}
fn allocate(ports: &mut Ports, bindings: &[Binding]) {
    ports
        .allocate(
            bindings,
            Some("web"),
            "same-seed",
            &BTreeSet::new(),
            &Default::default(),
        )
        .unwrap();
}

#[test]
fn concurrent_checkouts_lease_disjoint_replica_sets_and_reuse_them() {
    let cache = tempfile::tempdir().unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let cache = cache.path().to_owned();
            std::thread::spawn(move || {
                let root = tempfile::tempdir().unwrap();
                let mut ports = Ports::open(root.path(), &cache).unwrap();
                allocate(&mut ports, &bindings(2));
                let original = ports.groups.clone();
                allocate(&mut ports, &bindings(2));
                assert_eq!(ports.groups, original);
                (root, ports)
            })
        })
        .collect();
    let leases: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let mut all = BTreeSet::new();
    for (_, ports) in &leases {
        let group = &ports.groups[0];
        assert_eq!(group.last, group.first + 1);
        assert!(all.insert(group.first));
        assert!(all.insert(group.last));
    }
    for (_, ports) in leases {
        ports.release().unwrap();
    }
    let registry: serde_json::Value = read_json(&cache.path().join("port-leases.json")).unwrap();
    assert_eq!(registry["claims"], json!([]));
}

#[test]
fn occupied_tcp_and_udp_are_rejected_and_stopped_auto_lease_self_heals() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut ports = Ports::open(root.path(), cache.path()).unwrap();
    allocate(&mut ports, &bindings(1));
    let old = ports.groups[0].first;
    let socket = TcpListener::bind(("127.0.0.1", old)).unwrap();
    allocate(&mut ports, &bindings(1));
    assert_ne!(ports.groups[0].first, old);
    let mut explicit = bindings(1);
    explicit[0].explicit = Some((old, old));
    assert!(
        ports
            .allocate(
                &explicit,
                Some("web"),
                "seed",
                &BTreeSet::new(),
                &Default::default()
            )
            .is_err()
    );
    drop(socket);
    let udp = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    let number = udp.local_addr().unwrap().port();
    explicit[0].protocol = "udp".into();
    explicit[0].explicit = Some((number, number));
    assert!(
        ports
            .allocate(
                &explicit,
                Some("web"),
                "seed",
                &BTreeSet::new(),
                &Default::default()
            )
            .is_err()
    );
}

#[test]
fn explicit_range_does_not_become_auto_and_overlay_replaces_the_sequence() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut ports = Ports::open(root.path(), cache.path()).unwrap();
    let mut specs = bindings(2);
    // Allocate a genuinely free contiguous pair, then explicitly retain it.
    allocate(&mut ports, &specs);
    let (first, last) = (ports.groups[0].first, ports.groups[0].last);
    specs[0].explicit = Some((first, last));
    allocate(&mut ports, &specs);
    assert!(!ports.groups[0].automatic);
    assert_eq!((ports.groups[0].first, ports.groups[0].last), (first, last));
    let bytes = devkit::stand::port_overlay(&ports, &BTreeSet::from(["web".into()])).unwrap();
    let yaml = String::from_utf8(bytes).unwrap();
    assert!(yaml.contains("ports: !override"));
    assert!(yaml.contains(&format!("{first}-{last}")));
    assert!(!yaml.contains("127.0.0.1::"));
}

#[test]
fn deleted_worktree_is_collected_but_live_stopped_lease_is_reserved() {
    let cache = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut old = Ports::open(root.path(), cache.path()).unwrap();
    allocate(&mut old, &bindings(1));
    let number = old.groups[0].first;
    drop(root);
    let root = tempfile::tempdir().unwrap();
    let mut next = Ports::open(root.path(), cache.path()).unwrap();
    allocate(&mut next, &bindings(1));
    assert_eq!(next.groups[0].first, number);
    let another = tempfile::tempdir().unwrap();
    let mut live = Ports::open(another.path(), cache.path()).unwrap();
    allocate(&mut live, &bindings(1));
    assert_ne!(live.groups[0].first, number);
}

#[test]
fn invalid_or_unsafe_registry_does_not_discard_receipts() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut ports = Ports::open(root.path(), cache.path()).unwrap();
    let receipt = fs::read(root.path().join(RECEIPT)).unwrap();
    symlink("/dev/null", cache.path().join("port-leases.json")).unwrap();
    assert!(
        ports
            .allocate(
                &bindings(1),
                None,
                "seed",
                &BTreeSet::new(),
                &Default::default()
            )
            .is_err()
    );
    assert_eq!(receipt, fs::read(root.path().join(RECEIPT)).unwrap());
    for raw in ["", "0-1", "2-1", "1-65536", "$(id)", "1-2-3"] {
        assert!(port_range(Some(raw)).is_err());
    }
}

#[test]
fn daemon_bind_retries_are_attributed_and_never_move_explicit_or_healthy_groups() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut ports = Ports::open(root.path(), cache.path()).unwrap();
    let mut specs = bindings(1);
    let mut other = specs[0].clone();
    other.service = "api".into();
    specs.push(other);
    allocate(&mut ports, &specs);
    let original = ports.groups.clone();
    let failed = &original[0];
    let message = format!(
        "Error response from daemon: Bind for 127.0.0.1:{} failed: port is already allocated",
        failed.first
    );
    let conflicts = bind_conflicts(message.as_bytes(), &ports.groups).unwrap();
    assert_eq!(conflicts, BTreeSet::from([failed.binding.key()]));
    let modern = format!(
        "Error response from daemon: failed to bind host port for 127.0.0.1:{}:172.18.0.2:3000/tcp: address already in use",
        failed.first
    );
    assert_eq!(
        bind_conflicts(modern.as_bytes(), &ports.groups).unwrap(),
        conflicts
    );
    ports
        .allocate(
            &specs,
            Some("web"),
            "same-seed",
            &Default::default(),
            &conflicts,
        )
        .unwrap();
    assert_ne!(ports.groups[0].first, original[0].first);
    assert_eq!(ports.groups[1], original[1]);
    let mut explicit = original.clone();
    explicit[0].automatic = false;
    assert!(
        bind_conflicts(message.as_bytes(), &explicit)
            .unwrap()
            .is_empty()
    );
    assert!(
        bind_conflicts(b"application: address already in use", &original)
            .unwrap()
            .is_empty()
    );
}

fn modern(root: &Path) {
    let loader = root.join(".devkit/runtime/devkit.sh");
    write_atomic(&loader, epoch::CAPABILITY.as_bytes(), 0o644).unwrap();
    let empty = json!({"sha256":"0".repeat(64),"mode":420});
    let state = json!({"schemaVersion":2,"version":"0.5.0","generation":"00000000-0000-4000-8000-000000000001",
        "releaseIdentity":"0".repeat(64),"packageMode":"registry","components":{"apps/web":{
            "role":"app","toolchain":"node","toolingProfile":"web","origin":{"template":"react-web","version":"0.5.0"},
            "toolDependencies":{},"dependencies":[],"development":{"owner":"managed","kind":"compose","service":"web"}}},
        "managed":{".devkit/runtime/devkit.sh":identity(&loader).unwrap(),".devkit/runtime.sha256":empty,
            ".devkit/runtime/toolchains.env":empty,".devkit/project.mk":empty,".devkit/compose.apps.yaml":empty,".devkit/tooling-profiles.json":empty}});
    write_json(&root.join(".devkit/manifest.json"), &state).unwrap();
}

#[test]
fn physical_epoch_survives_reads_and_detects_same_byte_replacement() {
    let root = tempfile::tempdir().unwrap();
    modern(root.path());
    drop(CheckoutLock::acquire(root.path()).unwrap());
    assert!(epoch::trusted(root.path()).unwrap());
    let manifest = root.path().join(".devkit/manifest.json");
    let bytes = fs::read(&manifest).unwrap();
    assert!(epoch::trusted(root.path()).unwrap());
    write_atomic(&manifest, &bytes, 0o644).unwrap();
    assert!(!epoch::trusted(root.path()).unwrap());
    drop(CheckoutLock::acquire(root.path()).unwrap());
    assert!(!epoch::trusted(root.path()).unwrap());
}

#[test]
fn state_creator_can_finish_after_peer_creates_the_stable_lock() {
    let root = tempfile::tempdir().unwrap();
    modern(root.path());
    let ticket = epoch::before_lock(root.path()).unwrap();
    assert!(ticket.is_some());
    drop(CheckoutLock::acquire_shared(root.path(), "describe", None).unwrap());
    assert!(!epoch::trusted(root.path()).unwrap());
    let _lock = CheckoutLock::acquire(root.path()).unwrap();
    epoch::after_lock(root.path(), ticket).unwrap();
    assert!(epoch::trusted(root.path()).unwrap());
}

#[test]
fn crashed_creator_and_unexplained_state_never_become_fresh() {
    let root = tempfile::tempdir().unwrap();
    modern(root.path());
    drop(epoch::before_lock(root.path()).unwrap());
    drop(CheckoutLock::acquire(root.path()).unwrap());
    assert!(!epoch::trusted(root.path()).unwrap());
    let other = tempfile::tempdir().unwrap();
    modern(other.path());
    let ticket = epoch::before_lock(other.path()).unwrap();
    fs::write(other.path().join(".devkit/state/stand.active"), "unknown").unwrap();
    let _lock = CheckoutLock::acquire(other.path()).unwrap();
    epoch::after_lock(other.path(), ticket).unwrap();
    assert!(!epoch::trusted(other.path()).unwrap());
}

#[test]
fn transaction_commit_and_rollback_carry_forward_the_exact_leased_history() {
    use devkit::{
        state::MANIFEST_PATH,
        transaction::{MutationHooks, PreparedTransaction},
    };
    struct Fail;
    impl MutationHooks for Fail {
        fn after_write(&self, _: usize, _: &str) -> devkit::Result<()> {
            anyhow::bail!("injected write failure")
        }
    }
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        modern(root.path());
        let _lock = CheckoutLock::acquire(root.path()).unwrap();
        let mut ports = Ports::open(root.path(), cache.path()).unwrap();
        allocate(&mut ports, &bindings(1));
        let history = fs::read(root.path().join(RECEIPT)).unwrap();
        let staged = tempfile::tempdir().unwrap();
        let mut state: serde_json::Value = read_json(&root.path().join(MANIFEST_PATH)).unwrap();
        state["generation"] = json!(uuid::Uuid::new_v4().to_string());
        write_json(&staged.path().join(MANIFEST_PATH), &state).unwrap();
        let stage = format!(".devkit/state/update-{}", uuid::Uuid::new_v4());
        let transaction = PreparedTransaction::prepare(
            root.path(),
            &stage,
            staged.path(),
            &BTreeSet::from([MANIFEST_PATH.into()]),
            vec![],
        )
        .unwrap();
        let outcome = transaction.commit(
            root.path(),
            if fail {
                Some(&Fail as &dyn MutationHooks)
            } else {
                None
            },
            &|| Ok(()),
        );
        assert_eq!(outcome.is_err(), fail);
        assert!(epoch::trusted(root.path()).unwrap());
        assert_eq!(history, fs::read(root.path().join(RECEIPT)).unwrap());
        assert!(!root.path().join(".devkit/update-pending.json").exists());
    }
}
