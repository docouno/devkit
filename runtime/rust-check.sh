#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'devkit: %s\n' "$*" >&2
    exit 1
}

if [[ $# -ne 1 ]]; then
    fail 'rust-check expects one command: setup, check, test, build or fix'
fi
command=$1
case "$command" in
    setup|check|test|build|fix) ;;
    *) fail "unknown Rust command: $command" ;;
esac

[[ ${DEVKIT_RUST_VERSION:-} == 1.99.1 ]] || fail 'this runtime requires DEVKIT_RUST_VERSION=1.99.1'
[[ -z ${RUSTUP_TOOLCHAIN:-} ]] || fail 'RUSTUP_TOOLCHAIN masks rust-toolchain.toml; remove the override'
export RUSTUP_AUTO_INSTALL=0
export CARGO_TERM_COLOR=never

for file in Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml; do
    [[ -f $file ]] || fail "missing $file; run this command from the registered Rust app directory"
done
[[ ! -e rust-toolchain && ! -L rust-toolchain ]] || fail 'rust-toolchain conflicts with the required rust-toolchain.toml'
[[ ! -e .rustfmt.toml && ! -L .rustfmt.toml ]] || fail '.rustfmt.toml conflicts with the required rustfmt.toml'

active=$(rustup show active-toolchain) || fail 'the app toolchain is unavailable; run make setup with a compatible devkit release'
[[ $active == "$DEVKIT_RUST_VERSION-"* && $active == *rust-toolchain.toml* ]] ||
    fail "rust-toolchain.toml must select exactly $DEVKIT_RUST_VERSION (active: $active)"
compiler=$(rustc --version) || fail 'rustc is unavailable; run make setup'
[[ $compiler == "rustc $DEVKIT_RUST_VERSION "* ]] || fail "compiler mismatch: expected $DEVKIT_RUST_VERSION, got $compiler"
formatter=$(rustfmt --version) || fail 'rustfmt is missing from the tool image; run make setup'
[[ $formatter == 'rustfmt 1.10.0-stable '* ]] || fail "formatter mismatch: $formatter"
clippy=$(cargo clippy --version) || fail 'Clippy is missing from the tool image; run make setup'
[[ $clippy == 'clippy 0.1.99 '* ]] || fail "Clippy mismatch: $clippy"

scratch=$(mktemp -d "${TMPDIR:-/tmp}/devkit-rust.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
touch "$scratch/empty.rs"

config_warning() {
    grep -Eq "^Warning: (Unknown configuration option|can't set .*unstable)" "$1"
}

validate_format_config() {
    local status=0
    rustfmt --print-config current "$scratch/empty.rs" --config-path "$PWD/rustfmt.toml" \
        >"$scratch/config.out" 2>"$scratch/config.err" || status=$?
    cat "$scratch/config.err" >&2
    [[ $status -eq 0 ]] || return "$status"
    if config_warning "$scratch/config.err"; then
        fail 'rustfmt ignored a configuration option; use supported stable options'
    fi
    grep -Fxq 'disable_all_formatting = false' "$scratch/config.out" ||
        fail 'rustfmt configuration disables mandatory formatting'
}

format() {
    local status=0
    local -a args=(fmt --all --)
    if [[ $1 == check ]]; then
        args+=(--check)
    fi
    args+=(--config-path "$PWD/rustfmt.toml")
    cargo "${args[@]}" >"$scratch/format.out" 2>"$scratch/format.err" || status=$?
    cat "$scratch/format.out"
    cat "$scratch/format.err" >&2
    [[ $status -eq 0 ]] || return "$status"
    if config_warning "$scratch/format.err"; then
        fail 'rustfmt ignored a configuration option; use supported stable options'
    fi
}

cargo_gate() {
    local status=0
    cargo "$@" || status=$?
    if [[ $status -ne 0 ]]; then
        printf 'devkit: Rust command failed; if dependency caches are missing, run make setup\n' >&2
        return "$status"
    fi
}

list_tests() {
    local output=$1
    shift
    local status=0
    cargo test --workspace --locked --offline -- --list --color never "$@" \
        >"$output" 2>"$scratch/list.err" || status=$?
    if [[ $status -ne 0 ]]; then
        cat "$output"
        cat "$scratch/list.err" >&2
        printf 'devkit: Rust test discovery failed; run make setup if dependencies are missing\n' >&2
        return "$status"
    fi
    cat "$scratch/list.err" >&2
}

test_app() {
    list_tests "$scratch/all-tests.out"
    list_tests "$scratch/ignored-tests.out" --ignored
    local total ignored
    total=$(awk '/: test$/ { count++ } END { print count+0 }' "$scratch/all-tests.out")
    ignored=$(awk '/: test$/ { count++ } END { print count+0 }' "$scratch/ignored-tests.out")
    [[ $ignored -le $total ]] || fail 'inconsistent native test discovery output'
    [[ $total -gt $ignored ]] || fail 'no non-ignored native Rust test cases found; add a non-ignored unit, integration or documentation test'
    printf 'devkit: Rust native test cases: %s non-ignored, %s ignored; discovery may include compile-only doctests\n' "$((total - ignored))" "$ignored"
    cargo_gate test --workspace --locked --offline
}

validate_format_config
case "$command" in
    setup)
        cargo_gate fetch --locked
        ;;
    check)
        format check
        cargo_gate clippy --workspace --all-targets --locked --offline -- -D warnings
        test_app
        ;;
    test)
        test_app
        ;;
    build)
        cargo_gate build --workspace --locked --offline
        ;;
    fix)
        format fix
        ;;
esac
