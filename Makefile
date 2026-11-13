SHELL := /bin/sh
.DEFAULT_GOAL := help
.NOTPARALLEL:
include toolchains.env
include tooling/author.mk

.PHONY: verify-engine fix-rust-format verify-core-legacy pack-guard verify-author fix-format help setup bootstrap check check-native check-env test test-native test-special \
	test-env test-npm-inputs build build-fixtures build-controller npm-lock npm-inputs notices pack pack-local verify verify-source-gates verify-runtime-gates verify-delivery-gates verify-runtime verify-rust verify-delivery \
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
	  'make verify-public from_version=0.2.1 version=0.4.0 expected_index_sha256=SHA256' \
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
	can_cpu=1; can_mem=1; \
	if [ -f /sys/fs/cgroup/cgroup.controllers ]; then \
	  if [ -d /sys/fs/cgroup/docker ]; then \
	    t=$$(cat /sys/fs/cgroup/docker/cgroup.type 2>/dev/null || printf domain); \
	    c=" $$(cat /sys/fs/cgroup/docker/cgroup.controllers 2>/dev/null || true) "; \
	    [ "$$t" = domain ] || { can_cpu=0; can_mem=0; }; \
	    case "$$c" in *" cpu "*) ;; *) can_cpu=0 ;; esac; \
	    case "$$c" in *" memory "*) ;; *) can_mem=0 ;; esac; \
	  elif [ "$$DEVKIT_AUTHOR_UID" != 0 ]; then \
	    rel=$$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null | head -n 1); \
	    c=" $$(cat "/sys/fs/cgroup$$rel/cgroup.controllers" 2>/dev/null || true) "; \
	    case "$$c" in *" cpu "*) ;; *) can_cpu=0 ;; esac; \
	    case "$$c" in *" memory "*) ;; *) can_mem=0 ;; esac; \
	  else \
	    c=" $$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || true) "; \
	    s=" $$(cat /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true) "; \
	    p=$$(cat /sys/fs/cgroup/cgroup.procs 2>/dev/null || true); \
	    case "$$c" in *" cpu "*) [ -z "$$p" ] || case "$$s" in *" cpu "*) ;; *) can_cpu=0 ;; esac ;; *) can_cpu=0 ;; esac; \
	    case "$$c" in *" memory "*) [ -z "$$p" ] || case "$$s" in *" memory "*) ;; *) can_mem=0 ;; esac ;; *) can_mem=0 ;; esac; \
	  fi; \
	fi; \
	cpus=2; mem_mb=2048; \
	if [ -f /sys/fs/cgroup/cpu.max ]; then \
	  set -- $$(cat /sys/fs/cgroup/cpu.max 2>/dev/null || printf 'max 100000'); \
	  if [ "$${1:-max}" != max ] && [ "$${2:-0}" -gt 0 ] 2>/dev/null; then \
	    milli=$$(( $$1 * 1000 / $$2 )); [ "$$milli" -ge 100 ] || milli=100; \
	    if [ "$$milli" -lt 2000 ]; then \
	      whole=$$(( milli / 1000 )); frac=$$(( (milli % 1000) / 100 )); \
	      if [ "$$frac" -eq 0 ] && [ "$$whole" -ge 1 ]; then cpus="$$whole"; else cpus="$$whole.$$frac"; fi; \
	    fi; \
	  fi; \
	fi; \
	if [ -f /sys/fs/cgroup/memory.max ]; then \
	  mmax=$$(cat /sys/fs/cgroup/memory.max 2>/dev/null || printf max); \
	  if [ "$$mmax" != max ] && [ "$$mmax" -gt 0 ] 2>/dev/null; then \
	    avail=$$(( (mmax / 1048576) * 80 / 100 )); [ "$$avail" -ge 256 ] || avail=256; \
	    [ "$$avail" -lt "$$mem_mb" ] && mem_mb="$$avail" || true; \
	  fi; \
	fi; \
	jobs=2; [ "$$mem_mb" -lt 1536 ] && jobs=1 || true; \
	set --; \
	[ "$$can_cpu" = 1 ] && set -- "$$@" --cpus "$$cpus" --cpu-shares 512; \
	[ "$$can_mem" = 1 ] && set -- "$$@" --memory "$${mem_mb}m"; \
	exec docker run --pull never --rm --init --name "devkit-bootstrap-$$token" \
	  --label "io.docouno.devkit.invocation=$$token" \
	  --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" --network bridge "$$@" \
	  --cap-drop ALL --security-opt no-new-privileges:true --read-only --tmpfs /tmp:rw,mode=1777,size=512m \
	  --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:ro,z" \
	  --volume "$$DEVKIT_AUTHOR_CARGO_HOME:/cargo:z" --volume "$$DEVKIT_AUTHOR_TARGET:/target:z" \
	  --workdir /workspace --env HOME=/tmp/devkit-author-home --env CARGO_HOME=/cargo \
	  --env CARGO_TARGET_DIR=/target --env "CARGO_BUILD_JOBS=$$jobs" --env RUSTUP_AUTO_INSTALL=0 \
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
build-controller:
	$(call author_rust,cargo build --locked --offline --target x86_64-unknown-linux-musl --bin devkit-release)
	@mkdir -p "$$DEVKIT_AUTHOR_ROOT/.artifacts/release-ops"
	@cp "$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/debug/devkit-release" "$$DEVKIT_AUTHOR_ROOT/.artifacts/release-ops/devkit-release"
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
verify: verify-source-gates verify-runtime-gates verify-delivery-gates verify-packaged verify-registry
verify-source-gates: check test verify-author verify-core-legacy
verify-runtime-gates: verify-runtime verify-rust
verify-delivery-gates: verify-delivery verify-bootstrap
verify-core-legacy:
	$(call author_rust,cargo test --locked --offline --target x86_64-unknown-linux-musl --test core --no-run)
	@set -eu; image=$$(docker image inspect --format '{{.Id}}' "$$DEVKIT_AUTHOR_RUST_IMAGE"); DEVKIT_TEST_RUST_IMAGE="$$image" DEVKIT_VERIFY_PREVIOUS="$$DEVKIT_VERIFY_PREVIOUS" python3 test/core_legacy.py < /dev/null
verify-author:
	@python3 test/author.py < /dev/null
verify-engine:
	@python3 test/author.py --engine-only < /dev/null
verify-runtime: $(author_blackbox_build) build-fixtures verify-engine
	@DEVKIT_TEST_ENGINE="$$DEVKIT_VERIFY_ENGINE" DEVKIT_TEST_FIXTURE="$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/debug/examples/runtime_fixture" python3 test/runtime/test_runtime.py < /dev/null
verify-rust: $(author_blackbox_build) verify-engine
	@DEVKIT_TEST_ENGINE="$$DEVKIT_VERIFY_ENGINE" python3 test/rust-check/test_rust_check.py < /dev/null
verify-delivery:
	@test -n "$$DEVKIT_BUILD_SET" || { printf '%s\n' 'Set build_set=/absolute/build-set.' >&2; exit 1; }
	@DEVKIT_TEST_BUILD_SET="$$DEVKIT_BUILD_SET" DEVKIT_TEST_DOCKER=1 python3 test/delivery/test_delivery.py < /dev/null
verify-bootstrap:
	@test -n "$$DEVKIT_LEGACY_ARCHIVE" || { printf '%s\n' 'Set legacy_archive=/absolute/public-0.2.1.tar.gz with adjacent checksum.' >&2; exit 1; }
	@test "$$(uname -s):$$(uname -m)" = Linux:x86_64 || { printf '%s\n' 'Author builds require Linux x86_64.' >&2; exit 1; }
	@test -n "$$DEVKIT_BUILD_SET" || { printf '%s\n' 'Set build_set=/absolute/build-set.' >&2; exit 1; }
	@python3 test/bootstrap/verify.py --build-set "$$DEVKIT_BUILD_SET" --legacy-release "$$DEVKIT_LEGACY_ARCHIVE" < /dev/null
verify-packaged:
	@test -n "$$DEVKIT_VERIFY_RELEASE" -a -n "$$DEVKIT_VERIFY_PREVIOUS" || { printf '%s\n' 'Set release= and previous= to two distinct immutable payloads.' >&2; exit 1; }
	@DEVKIT_VERIFY_NEXT_RELEASE="$$DEVKIT_VERIFY_RELEASE" python3 test/packaged/verify.py "$$DEVKIT_VERIFY_PREVIOUS" < /dev/null
verify-registry: build-fixtures
	@test -n "$$DEVKIT_VERIFY_RELEASE" -a -n "$$DEVKIT_VERIFY_PREVIOUS" || { printf '%s\n' 'Set release= and previous= to two distinct immutable payloads.' >&2; exit 1; }
	@bash test/verify-registry.sh "$$DEVKIT_VERIFY_PREVIOUS" "$$DEVKIT_VERIFY_RELEASE" < /dev/null
verify-public:
	@test -n "$$DEVKIT_EXPECTED_INDEX_SHA256" || { printf '%s\n' 'Set expected_index_sha256= to the frozen release-index.json SHA256.' >&2; exit 1; }
	@python3 test/public/verify.py --version "$$DEVKIT_FROM_VERSION" --next-version "$$DEVKIT_RELEASE_VERSION" --expected-index-sha256 "$$DEVKIT_EXPECTED_INDEX_SHA256" < /dev/null
init: build
	@test -n "$$DEVKIT_VERIFY_RELEASE" -a -n "$$DEVKIT_INIT_TARGET" || { printf '%s\n' 'Set release= and target=.' >&2; exit 1; }
	@"$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/release/devkit" init "$$DEVKIT_INIT_TARGET" --release-root "$$DEVKIT_VERIFY_RELEASE" --template "$$DEVKIT_INIT_TEMPLATE" --package-source local --non-interactive
