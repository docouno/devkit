#!/bin/sh
# Build the host-only preview on native Apple Silicon without Docker.
set -eu
umask 022
case "$(uname -s):$(uname -m)" in Darwin:arm64) ;; *) echo 'Apple Silicon host required.' >&2; exit 1;; esac
test "$#" = 1 || { echo 'Usage: darwin-producer.sh OUTPUT | --test-only' >&2; exit 1; }
case "$1" in
  --build-frozen|--test-only|/*) arg=$1 ;;
  *) arg=$PWD/$1 ;;
esac
root=$(git rev-parse --show-toplevel)
cd "$root"
case "$arg" in
  --build-frozen) test -n "${DEVKIT_FROZEN_COMMIT:-}" && test -n "${DEVKIT_FROZEN_TREE:-}" || exit 1 ;;
  --test-only) unset DEVKIT_FROZEN_COMMIT DEVKIT_FROZEN_TREE ;;
  *) exec python3 tooling/frozen-producer.py "$arg" ;;
esac
version=$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)
test -n "$version"
rustup toolchain install "$version" --profile minimal
export RUSTUP_TOOLCHAIN="$version"

target=aarch64-apple-darwin
work="$root/.artifacts/darwin-host"
export CARGO_TARGET_DIR="$work/target" CARGO_HOME="$work/cargo-home"
mkdir -p "$CARGO_HOME"
sync_cargo_cache() {
  src=$1
  dst=$2
  python3 - "$src" "$dst" <<'PY'
import shutil, sys
from pathlib import Path
src, dst = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
if src != dst:
    for rel in ("registry/index", "registry/cache", "git/db"):
        if (src / rel).is_dir():
            shutil.copytree(src / rel, dst / rel, dirs_exist_ok=True)
PY
}
if [ -n "${DEVKIT_DARWIN_CARGO_HOME:-}" ]; then
  mkdir -p "$DEVKIT_DARWIN_CARGO_HOME"
  sync_cargo_cache "$DEVKIT_DARWIN_CARGO_HOME" "$CARGO_HOME"
fi
cargo fetch --locked --target "$target"
if [ -n "${DEVKIT_DARWIN_CARGO_HOME:-}" ]; then
  sync_cargo_cache "$CARGO_HOME" "$DEVKIT_DARWIN_CARGO_HOME"
fi
cargo test --locked --offline --target "$target" --lib --test archive --test delivery --test processes -- --test-threads=1
rss_test=pnpm_yaml_budget_precedes_native_admission_and_identity_rechecks
cargo test --locked --offline --target "$target" --test native -- --list > "$work/native-tests.txt"
grep -Fx "$rss_test: test" "$work/native-tests.txt"
cargo test --locked --offline --target "$target" --test native "$rss_test" -- --exact --test-threads=1
python3 test/bootstrap/portable.py
test "$arg" != --test-only || exit 0

sysroot=$(rustc --print sysroot)
if ! test -s "$sysroot/share/doc/rust/COPYRIGHT-library.html" || ! test -d "$sysroot/share/doc/rust/licenses"; then
  rustup component add rust-docs --toolchain "$version"
fi
test -s "$sysroot/share/doc/rust/COPYRIGHT-library.html"
test -d "$sysroot/share/doc/rust/licenses"

cargo build --release --locked --offline --target "$target" --bin devkit
engine="$CARGO_TARGET_DIR/$target/release/devkit"
/usr/bin/codesign --verify --strict "$engine"
"$engine" internal build-info
"$engine" internal collect-notices --root "$root" --cargo-home "$CARGO_HOME" \
  --rust-docs "$sysroot/share/doc/rust" --rust-version "$version" --output "$work/notices" > "$work/notice-producer.json"
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
