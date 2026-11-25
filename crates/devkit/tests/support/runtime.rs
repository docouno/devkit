use devkit::{
    distribution::Target,
    files::{SemVer, digest, write_atomic},
};
use std::path::Path;

// Seal replicas of the runtime inputs used by the lifecycle APIs.
pub fn write_runtime(release: &Path, version: &str, engine_sha: &str) {
    let runtime = release.join("runtime");
    let recipe = b"FROM scratch\n";
    for (name, bytes) in [
        ("rust.Dockerfile", recipe.as_slice()),
        ("desktop.Dockerfile", recipe.as_slice()),
        ("desktop-seccomp.json", b"{}\n"),
        ("licenses/Apache-2.0.txt", b"fixture license\n"),
        ("licenses/moby-profiles.NOTICE", b"fixture attribution\n"),
        ("LICENSE", b"fixture Devkit license\n"),
    ] {
        write_atomic(&runtime.join(name), bytes, 0o644).unwrap();
    }
    if devkit::state::uses_tool_compose_projections(version).unwrap() {
        write_atomic(
            &runtime.join("compose.tools.yaml"),
            b"services: {}\n",
            0o644,
        )
        .unwrap();
    }
    for (name, mode) in [("devkit.mk", 0o644), ("devkit.sh", 0o755)] {
        write_atomic(
            &runtime.join(name),
            format!("fixture {name} {version}\n").as_bytes(),
            mode,
        )
        .unwrap();
    }
    let host = Target::host().unwrap();
    let multi_target = SemVer::parse(version).unwrap() >= SemVer::parse("0.6.0").unwrap()
        || host != Target::LinuxX64;
    let x64_sha = if host == Target::LinuxX64 {
        engine_sha.to_owned()
    } else {
        digest(format!("fixture-engine-{version}-{}", Target::LinuxX64.triple()).as_bytes())
    };
    let mut engine_pins = format!(
        "DEVKIT_ENGINE_TARGET={}\nDEVKIT_ENGINE_SHA256={x64_sha}\n",
        Target::LinuxX64.triple()
    );
    if multi_target {
        let targets = Target::ALL.map(Target::triple).join(",");
        engine_pins.push_str(&format!("DEVKIT_ENGINE_TARGETS={targets}\n"));
        for target in Target::ALL {
            let sha = if target == host {
                engine_sha.to_owned()
            } else if target == Target::LinuxX64 {
                x64_sha.clone()
            } else {
                digest(format!("fixture-engine-{version}-{}", target.triple()).as_bytes())
            };
            engine_pins.push_str(&format!(
                "DEVKIT_ENGINE_SHA256_{}={sha}\n",
                target.key_suffix()
            ));
        }
    }
    let recipe_hash = digest(recipe);
    let pins = format!(
        "DEVKIT_VERSION={version}\n{engine_pins}DEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(&runtime.join("toolchains.env"), pins.as_bytes(), 0o644).unwrap();
}
