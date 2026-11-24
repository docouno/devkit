use devkit::files::{digest, write_atomic};
use std::path::Path;

// Seal replicas of the runtime inputs used by the lifecycle APIs. The Compose
// template is shared with the assembler so profile selection exercises its anchors.
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
        (
            "compose.tools.yaml",
            include_bytes!("../../../../runtime/compose.tools.yaml"),
        ),
    ] {
        write_atomic(&runtime.join(name), bytes, 0o644).unwrap();
    }
    for (name, mode) in [("devkit.mk", 0o644), ("devkit.sh", 0o755)] {
        write_atomic(
            &runtime.join(name),
            format!("fixture {name} {version}\n").as_bytes(),
            mode,
        )
        .unwrap();
    }
    let recipe_hash = digest(recipe);
    let pins = format!(
        "DEVKIT_VERSION={version}\nDEVKIT_ENGINE_SHA256={engine_sha}\nDEVKIT_ENGINE_TARGET=x86_64-unknown-linux-musl\nDEVKIT_NODE_IMAGE=node:24.22.1-bookworm-slim@sha256:55fb5f5a527ea2c5c4a95f963e48ef90aa38c7b346054fdc8ee9957a2b55d205\nDEVKIT_NODE_VERSION=24.22.1\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH={recipe_hash}\nDEVKIT_DESKTOP_RECIPE_HASH={recipe_hash}\n"
    );
    write_atomic(&runtime.join("toolchains.env"), pins.as_bytes(), 0o644).unwrap();
}
