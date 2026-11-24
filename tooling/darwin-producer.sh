#!/bin/sh
# Build the host-only preview on native Apple Silicon without Docker.
set -eu
umask 022
case "$(uname -s):$(uname -m)" in Darwin:arm64) ;; *) echo 'Apple Silicon host required.' >&2; exit 1;; esac
test "$#" = 1 || { echo 'Usage: darwin-producer.sh OUTPUT | --test-only' >&2; exit 1; }
root=$(git rev-parse --show-toplevel)
cd "$root"
version=$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)
test -n "$version"
rustup toolchain install "$version" --profile minimal
export RUSTUP_TOOLCHAIN="$version"
sysroot=$(rustc --print sysroot)
if ! test -s "$sysroot/share/doc/rust/COPYRIGHT-library.html" || ! test -d "$sysroot/share/doc/rust/licenses"; then
  rustup component add rust-docs --toolchain "$version"
fi
test -s "$sysroot/share/doc/rust/COPYRIGHT-library.html"
test -d "$sysroot/share/doc/rust/licenses"

target=aarch64-apple-darwin
work="$root/.artifacts/darwin-host"
export CARGO_TARGET_DIR="$work/target" CARGO_HOME="$work/cargo-home"
mkdir -p "$CARGO_HOME"
DEVKIT_SOURCE_COMMIT=$(git rev-parse HEAD)
DEVKIT_SOURCE_TREE=$(git rev-parse 'HEAD^{tree}')
export DEVKIT_SOURCE_COMMIT DEVKIT_SOURCE_TREE
cargo fetch --locked --target "$target"
cargo test --locked --offline --target "$target" --lib --test archive --test delivery --test processes -- --test-threads=1
python3 test/bootstrap/portable.py
test "$1" != --test-only || exit 0

cargo build --release --locked --offline --target "$target" --bin devkit
engine="$CARGO_TARGET_DIR/$target/release/devkit"
/usr/bin/codesign --verify --strict "$engine"
"$engine" internal build-info
"$engine" internal collect-notices --root "$root" --cargo-home "$CARGO_HOME" \
  --rust-docs "$sysroot/share/doc/rust" --rust-version "$version" --output "$work/notices" > "$work/notice-producer.json"
notice_sha=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["noticeSetSha256"])' < "$work/notice-producer.json")
python3 - "$work/host.json" <<'PY'
import json, subprocess, sys
from pathlib import Path

commands = {
    'macOS': ['sw_vers', '-productVersion'],
    'xcode': ['xcodebuild', '-version'],
    'sdk': ['xcrun', '--sdk', 'macosx', '--show-sdk-version'],
    'rust': ['rustc', '--version'],
    'make': ['/usr/bin/make', '--version'],
}
versions = {name: subprocess.check_output(command, text=True).splitlines()[0]
            for name, command in commands.items()}
Path(sys.argv[1]).write_text(json.dumps(versions) + '\n')
PY
"$engine" internal contribute --root "$root" --engine "$engine" --notices "$work/notices" \
  --notice-set-sha256 "$notice_sha" --build-run "${BUILD_RUN:-local-author}" --host-env "$work/host.json" --output "$1"
