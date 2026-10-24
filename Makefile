SHELL := /bin/sh
.DEFAULT_GOAL := help
.NOTPARALLEL:
include toolchains.env
include tooling/author.mk

.PHONY: verify-engine fix-rust-format verify-core-legacy pack-guard verify-author fix-format help setup bootstrap check check-native check-env test test-native test-special \
	test-env test-npm-inputs build build-fixtures npm-lock npm-inputs notices pack pack-local verify verify-runtime verify-rust verify-delivery \
	verify-bootstrap verify-packaged verify-registry verify-public init
help:
	@printf '%s\n' 'Devkit author workspace (Linux x86_64 + Docker Compose >=5.0.0 + Python 3)' \
	  'make setup          Prepare pinned SDK images, native engine and npm dependencies' \
	  'make fix-format     Apply source formatting explicitly' \
	  'make check          Rust fmt/Clippy and env declarations/lint/format/shell checks' \
	  'make test           Native units, Cargo/Git/strace gates and Node env/npm tests' \
	  'make build          Build the attributed static release ELF, offline' \
	  'make pack output=/absolute/new/build-set  Assemble clean committed source' \
	  'make pack-local output=/absolute/new/build-set  Assemble an unpublished preview' \
	  'make verify build_set=/absolute/set release=/absolute/set/payload previous=/absolute/old/payload legacy_archive=/absolute/0.2.1.tar.gz' \
	  'make verify-bootstrap build_set=/absolute/set legacy_archive=/absolute/0.2.1.tar.gz' \
	  'make verify-public from_version=0.2.1 version=0.3.0 expected_index_sha256=SHA256' \
	  'make init release=/absolute/set/payload target=/absolute/new/project' \
	  'Ordinary checks use cached SDKs. Only setup prepares images and dependencies.'
setup: bootstrap
	@docker image inspect "$$DEVKIT_AUTHOR_NODE_IMAGE" >/dev/null 2>&1 || docker pull "$$DEVKIT_AUTHOR_NODE_IMAGE"
	$(call author_node,bridge,npm ci --include=dev --strict-peer-deps --bin-links=false)
# The only command before a native engine exists: exec Docker, with no host SDK.
bootstrap:
	@test "$$(uname -s):$$(uname -m)" = Linux:x86_64 || { printf '%s\n' 'Author builds require Linux x86_64.' >&2; exit 1; }
	@docker image inspect "$$DEVKIT_AUTHOR_RUST_IMAGE" >/dev/null 2>&1 || docker build --tag "$$DEVKIT_AUTHOR_RUST_IMAGE" - < tooling/native.Dockerfile
	@mkdir -p "$$DEVKIT_AUTHOR_CARGO_HOME" "$$DEVKIT_AUTHOR_TARGET" "$$DEVKIT_AUTHOR_ROOT/.artifacts/author"
	@set -eu; read -r token < /proc/sys/kernel/random/uuid; \
	exec docker run --pull never --rm --init --name "devkit-bootstrap-$$token" \
	  --label "io.docouno.devkit.invocation=$$token" \
	  --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" --network bridge --memory 2g --cpus 2 \
	  --cap-drop ALL --security-opt no-new-privileges:true --read-only --tmpfs /tmp:rw,mode=1777 \
	  --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:ro,z" \
	  --volume "$$DEVKIT_AUTHOR_CARGO_HOME:/cargo:z" --volume "$$DEVKIT_AUTHOR_TARGET:/target:z" \
	  --workdir /workspace --env HOME=/tmp/devkit-author-home --env CARGO_HOME=/cargo \
	  --env CARGO_TARGET_DIR=/target --env CARGO_BUILD_JOBS=1 --env RUSTUP_AUTO_INSTALL=0 \
	  "$$DEVKIT_AUTHOR_RUST_IMAGE" cargo build --locked --target x86_64-unknown-linux-musl --bin devkit
check: check-native check-env
check-native:
	$(call author_rust,cargo fmt --all -- --check)
	$(call author_rust,cargo clippy --workspace --all-targets --locked --offline --target x86_64-unknown-linux-musl -- -D warnings)
fix-rust-format:
	$(call author_rust,cargo fmt --all,rw)
fix-format: fix-rust-format
	$(call author_node,none,npm run fix:format,,rw)
check-env:
	$(call author_node,none,npm run check)
test: test-native test-special test-env test-npm-inputs
test-native:
	$(call author_rust,cargo test --workspace --locked --offline --target x86_64-unknown-linux-musl -- --test-threads=1)
test-special:
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test core git_tracks_output_named_component_sources_and_ignores_scoped_outputs -- --ignored --exact --test-threads=1)
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test native pinned_cargo_metadata -- --ignored --test-threads=1)
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test assembler -- --ignored --test-threads=1)
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test notices -- --ignored --test-threads=1)
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test core linux_strace_checks_actual_commit_and_recovery_durability_order -- --ignored --exact --test-threads=1)
test-env:
	$(call author_node,bridge,npm run test:env)
test-npm-inputs:
	$(call author_node,bridge,npm run test:npm-inputs,--env DEVKIT_TEST_NPM=1)
build:
	$(call author_rust,cargo build --release --locked --offline --target x86_64-unknown-linux-musl --bin devkit)
build-fixtures:
	$(call author_rust,cargo build --locked --offline --target x86_64-unknown-linux-musl --examples)
npm-lock:
	$(call author_node,bridge,npm install --package-lock-only --ignore-scripts --include=dev --strict-peer-deps,--volume "$$DEVKIT_AUTHOR_ROOT/package-lock.json:/workspace/package-lock.json:z")
npm-inputs:
	$(call author_node,bridge,node tooling/prepare-npm-inputs.mjs --root /workspace --version "$$DEVKIT_RELEASE_VERSION" --output "/workspace/.artifacts/author/$$DEVKIT_BUILD_ID-npm" > "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-npm-producer.json")
notices:
	$(call author_rust,/workspace/.artifacts/author/target/x86_64-unknown-linux-musl/release/devkit internal collect-notices --root /workspace --cargo-home /cargo --rust-docs /usr/local/rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/share/doc/rust --rust-version "$$DEVKIT_RUST_VERSION" --output "/workspace/.artifacts/author/$$DEVKIT_BUILD_ID-notices" > "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-notice-producer.json")
pack: override export DEVKIT_ASSEMBLE_LOCAL :=
pack: pack-guard build npm-inputs notices
	$(assemble)
pack-local: override export DEVKIT_ASSEMBLE_LOCAL := --local
pack-local: pack-guard build npm-inputs notices
	$(assemble)
pack-guard:
	@test -n "$$DEVKIT_ASSEMBLY_OUTPUT" || { printf '%s\n' 'Set output=/absolute/new/build-set.' >&2; exit 1; }
	@test ! -e "$$DEVKIT_ASSEMBLY_OUTPUT" -a ! -L "$$DEVKIT_ASSEMBLY_OUTPUT" || { printf '%s\n' 'Assembly output must be new.' >&2; exit 1; }
verify: check test verify-author verify-core-legacy build-fixtures verify-runtime verify-rust verify-delivery verify-bootstrap verify-packaged verify-registry
verify-core-legacy:
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test core --no-run)
	@set -eu; image=$$(docker image inspect --format '{{.Id}}' "$$DEVKIT_AUTHOR_RUST_IMAGE"); DEVKIT_TEST_RUST_IMAGE="$$image" DEVKIT_VERIFY_PREVIOUS="$$DEVKIT_VERIFY_PREVIOUS" python3 test/core_legacy.py
verify-author:
	@python3 test/author.py
verify-engine:
	@python3 test/author.py --engine-only
verify-runtime: $(author_blackbox_build) build-fixtures verify-engine
	@DEVKIT_TEST_ENGINE="$$DEVKIT_VERIFY_ENGINE" DEVKIT_TEST_FIXTURE="$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/debug/examples/runtime_fixture" python3 test/runtime/test_runtime.py
verify-rust: $(author_blackbox_build) verify-engine
	@DEVKIT_TEST_ENGINE="$$DEVKIT_VERIFY_ENGINE" python3 test/rust-check/test_rust_check.py
verify-delivery:
	@test -n "$$DEVKIT_BUILD_SET" || { printf '%s\n' 'Set build_set=/absolute/build-set.' >&2; exit 1; }
	@DEVKIT_TEST_BUILD_SET="$$DEVKIT_BUILD_SET" DEVKIT_TEST_DOCKER=1 python3 test/delivery/test_delivery.py
verify-bootstrap:
	@test -n "$$DEVKIT_LEGACY_ARCHIVE" || { printf '%s\n' 'Set legacy_archive=/absolute/public-0.2.1.tar.gz with adjacent checksum.' >&2; exit 1; }
	@test "$$(uname -s):$$(uname -m)" = Linux:x86_64 || { printf '%s\n' 'Author builds require Linux x86_64.' >&2; exit 1; }
	@test -n "$$DEVKIT_BUILD_SET" || { printf '%s\n' 'Set build_set=/absolute/build-set.' >&2; exit 1; }
	@python3 test/bootstrap/verify.py --build-set "$$DEVKIT_BUILD_SET" --legacy-release "$$DEVKIT_LEGACY_ARCHIVE"
verify-packaged:
	@test -n "$$DEVKIT_VERIFY_RELEASE" -a -n "$$DEVKIT_VERIFY_PREVIOUS" || { printf '%s\n' 'Set release= and previous= to two distinct immutable payloads.' >&2; exit 1; }
	@DEVKIT_VERIFY_NEXT_RELEASE="$$DEVKIT_VERIFY_RELEASE" python3 test/packaged/verify.py "$$DEVKIT_VERIFY_PREVIOUS"
verify-registry: build-fixtures
	@test -n "$$DEVKIT_VERIFY_RELEASE" -a -n "$$DEVKIT_VERIFY_PREVIOUS" || { printf '%s\n' 'Set release= and previous= to two distinct immutable payloads.' >&2; exit 1; }
	@bash test/verify-registry.sh "$$DEVKIT_VERIFY_PREVIOUS" "$$DEVKIT_VERIFY_RELEASE"
verify-public:
	@test -n "$$DEVKIT_EXPECTED_INDEX_SHA256" || { printf '%s\n' 'Set expected_index_sha256= to the frozen release-index.json SHA256.' >&2; exit 1; }
	@python3 test/public/verify.py --version "$$DEVKIT_FROM_VERSION" --next-version "$$DEVKIT_RELEASE_VERSION" --expected-index-sha256 "$$DEVKIT_EXPECTED_INDEX_SHA256"
init: build
	@test -n "$$DEVKIT_VERIFY_RELEASE" -a -n "$$DEVKIT_INIT_TARGET" || { printf '%s\n' 'Set release= and target=.' >&2; exit 1; }
	@"$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/release/devkit" init "$$DEVKIT_INIT_TARGET" --release-root "$$DEVKIT_VERIFY_RELEASE" --template "$$DEVKIT_INIT_TEMPLATE" --package-source local --non-interactive
