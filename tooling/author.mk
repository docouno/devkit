# Docker argv and mounts only. Process ownership/cancellation stays in native Rust.
# Freeze operator data before Make exports command-line variables to $(shell).
ifneq ($(origin target_inputs),undefined)
override target_inputs := $(value target_inputs)
endif
ifneq ($(origin npm_inputs),undefined)
override npm_inputs := $(value npm_inputs)
endif
ifneq ($(origin DEVKIT_AUTHOR_ENGINE),undefined)
override DEVKIT_AUTHOR_ENGINE := $(value DEVKIT_AUTHOR_ENGINE)
endif
ifneq ($(origin target),undefined)
override target := $(value target)
endif
ifneq ($(origin output),undefined)
override output := $(value output)
endif
ifneq ($(origin build_set),undefined)
override build_set := $(value build_set)
endif
ifneq ($(origin previous),undefined)
override previous := $(value previous)
endif
ifneq ($(origin release),undefined)
override release := $(value release)
endif
ifneq ($(origin baseline_set),undefined)
override baseline_set := $(value baseline_set)
endif
ifneq ($(origin fixtures),undefined)
override fixtures := $(value fixtures)
endif
ifneq ($(origin template),undefined)
override template := $(value template)
endif
ifneq ($(origin version),undefined)
override version := $(value version)
endif
ifneq ($(origin from_version),undefined)
override from_version := $(value from_version)
endif
ifneq ($(origin build_id),undefined)
override build_id := $(value build_id)
endif
ifneq ($(origin build_run),undefined)
override build_run := $(value build_run)
endif
ifneq ($(origin expected_index_sha256),undefined)
override expected_index_sha256 := $(value expected_index_sha256)
endif
ifneq ($(origin DEVKIT_AUTHOR_CARGO_HOME),undefined)
override DEVKIT_AUTHOR_CARGO_HOME := $(value DEVKIT_AUTHOR_CARGO_HOME)
endif
ifneq ($(origin DEVKIT_AUTHOR_RUST_IMAGE),undefined)
override DEVKIT_AUTHOR_RUST_IMAGE := $(value DEVKIT_AUTHOR_RUST_IMAGE)
endif
ifneq ($(origin DEVKIT_AUTHOR_PNPM_STORE),undefined)
override DEVKIT_AUTHOR_PNPM_STORE := $(value DEVKIT_AUTHOR_PNPM_STORE)
endif
ifneq ($(origin DEVKIT_AUTHOR_PNPM_CACHE),undefined)
override DEVKIT_AUTHOR_PNPM_CACHE := $(value DEVKIT_AUTHOR_PNPM_CACHE)
endif
override export DEVKIT_AUTHOR_TRIPLE := $(filter x86_64 aarch64,$(shell uname -m))-unknown-linux-musl
# The host SDK and local Linux daemon must use their native platform.
unexport DOCKER_DEFAULT_PLATFORM
export DEVKIT_AUTHOR_ROOT := $(CURDIR)
export DEVKIT_AUTHOR_UID := $(shell id -u)
export DEVKIT_AUTHOR_GID := $(shell id -g)
export DEVKIT_AUTHOR_NODE_IMAGE := $(DEVKIT_NODE_IMAGE)
export DEVKIT_NODE_VERSION
export DEVKIT_RUST_VERSION
export DEVKIT_PNPM_VERSION DEVKIT_PNPM_TARBALL_SHA256 DEVKIT_PNPM_SHA256
override DEVKIT_AUTHOR_PNPM_STORE := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_PNPM_STORE)),$(CURDIR)/.artifacts/author/pnpm-store,$(value DEVKIT_AUTHOR_PNPM_STORE))
override DEVKIT_AUTHOR_PNPM_CACHE := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_PNPM_CACHE)),$(CURDIR)/.artifacts/author/pnpm-cache,$(value DEVKIT_AUTHOR_PNPM_CACHE))
export DEVKIT_AUTHOR_PNPM_STORE DEVKIT_AUTHOR_PNPM_CACHE
author_recipe_sha := $(word 1,$(shell sha256sum tooling/native.Dockerfile))
override DEVKIT_AUTHOR_RUST_IMAGE := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_RUST_IMAGE)),devkit-author-rust:$(author_recipe_sha),$(value DEVKIT_AUTHOR_RUST_IMAGE))
export DEVKIT_AUTHOR_RUST_IMAGE
override DEVKIT_AUTHOR_CARGO_HOME := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_CARGO_HOME)),$(CURDIR)/.artifacts/author/cargo-home,$(value DEVKIT_AUTHOR_CARGO_HOME))
export DEVKIT_AUTHOR_CARGO_HOME
export DEVKIT_AUTHOR_TARGET := $(CURDIR)/.artifacts/author/target
override DEVKIT_AUTHOR_ENGINE := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_ENGINE)),$(CURDIR)/.artifacts/author/target/$(DEVKIT_AUTHOR_TRIPLE)/debug/devkit,$(value DEVKIT_AUTHOR_ENGINE))
export DEVKIT_AUTHOR_ENGINE
export DEVKIT_TARGET_INPUTS := $(value target_inputs)
export DEVKIT_RELEASE_VERSION := $(if $(filter undefined,$(origin version)),$(strip $(file <VERSION)),$(value version))
export DEVKIT_FROM_VERSION := $(if $(filter undefined,$(origin from_version)),0.5.1,$(value from_version))
export DEVKIT_BUILD_ID := $(if $(filter undefined,$(origin build_id)),$(shell cat /proc/sys/kernel/random/uuid),$(value build_id))
export DEVKIT_NPM_INPUTS := $(if $(filter undefined,$(origin npm_inputs)),$(CURDIR)/.artifacts/author/$(DEVKIT_BUILD_ID)-npm,$(value npm_inputs))
export DEVKIT_BUILD_RUN := $(if $(filter undefined,$(origin build_run)),local-author,$(value build_run))
export DEVKIT_ASSEMBLY_OUTPUT := $(value output)
export DEVKIT_BUILD_SET := $(value build_set)
export DEVKIT_VERIFY_RELEASE := $(value release)
export DEVKIT_VERIFY_PREVIOUS := $(value previous)
export DEVKIT_BASELINE_SET := $(value baseline_set)
export DEVKIT_INIT_TARGET := $(value target)
export DEVKIT_INIT_TEMPLATE := $(if $(filter undefined,$(origin template)),node-api,$(value template))

ifneq ($(origin fixtures),undefined)
export DEVKIT_AUTHOR_FIXTURES := $(value fixtures)
author_fixtures_build :=
else
export DEVKIT_AUTHOR_FIXTURES := $(CURDIR)/.artifacts/author/target/$(DEVKIT_AUTHOR_TRIPLE)/debug/examples
author_fixtures_build := build-fixtures
endif

ifneq ($(origin release),undefined)
export DEVKIT_VERIFY_ENGINE := $(value release)/$(if $(filter x86_64-unknown-linux-musl,$(DEVKIT_AUTHOR_TRIPLE)),devkit,devkit-$(DEVKIT_AUTHOR_TRIPLE))
else
export DEVKIT_VERIFY_ENGINE := $(CURDIR)/.artifacts/author/target/$(DEVKIT_AUTHOR_TRIPLE)/release/devkit
author_blackbox_build := build
endif

export DEVKIT_EXPECTED_INDEX_SHA256 := $(value expected_index_sha256)

define author_node
	@test -x "$$DEVKIT_AUTHOR_ENGINE" || { printf '%s\n' 'Native author engine is missing. Run make setup.' >&2; exit 1; }
	@mkdir -p "$$DEVKIT_AUTHOR_ROOT/node_modules" "$$DEVKIT_AUTHOR_ROOT/packages/env/node_modules" "$$DEVKIT_AUTHOR_PNPM_STORE" "$$DEVKIT_AUTHOR_PNPM_CACHE" "$$DEVKIT_AUTHOR_ROOT/.tmp"
	@set -eu; read -r token < /proc/sys/kernel/random/uuid; name="devkit-author-$$token"; \
	pnpm_bin=$$("$$DEVKIT_AUTHOR_ENGINE" --offline internal ensure-pnpm --pin-file "$$DEVKIT_AUTHOR_ROOT/toolchains.env"); \
	set --; \
	case "$$DEVKIT_AUTHOR_PNPM_STORE" in \
	  "$$DEVKIT_AUTHOR_ROOT"/*) store="/workspace/$${DEVKIT_AUTHOR_PNPM_STORE#"$$DEVKIT_AUTHOR_ROOT"/}" ;; \
	  *) store=/pnpm-store; set -- "$$@" --volume "$$DEVKIT_AUTHOR_PNPM_STORE:/pnpm-store:z" ;; \
	esac; \
	case "$$DEVKIT_AUTHOR_PNPM_CACHE" in \
	  "$$DEVKIT_AUTHOR_ROOT"/*) cache="/workspace/$${DEVKIT_AUTHOR_PNPM_CACHE#"$$DEVKIT_AUTHOR_ROOT"/}" ;; \
	  *) cache=/pnpm-cache; set -- "$$@" --volume "$$DEVKIT_AUTHOR_PNPM_CACHE:/pnpm-cache:z" ;; \
	esac; \
	if [ -z '$(4)' ] && [ -d "$$DEVKIT_AUTHOR_ROOT/node_modules/.pnpm" ]; then \
	  set -- "$$@" --volume "$$DEVKIT_AUTHOR_ROOT/node_modules/.pnpm:/workspace/node_modules/.pnpm:ro,z"; fi; \
	res_flags=$$("$$DEVKIT_AUTHOR_ENGINE" internal docker-flags --cpus 2 --memory 1024m --pids-limit 512 --node-fallback); \
	exec "$$DEVKIT_AUTHOR_ENGINE" internal guard "$$name" "$$token" -- \
	  docker run --pull never --rm --init --name "$$name" \
	    --label "io.docouno.devkit.invocation=$$token" \
	    --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" \
	    --network $(1) --read-only $$res_flags \
	    --cap-drop ALL --security-opt no-new-privileges:true --tmpfs /tmp:rw,exec,mode=1777,size=256m \
	    --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:$(or $(4),ro),z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/node_modules:/workspace/node_modules:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/packages/env/node_modules:/workspace/packages/env/node_modules:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/.artifacts:/workspace/.artifacts:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/.tmp:/workspace/.tmp:z" \
	    --volume "$$DEVKIT_AUTHOR_ENGINE:/devkit/devkit:ro,z" \
	    --volume "$$pnpm_bin:/devkit/bin/pnpm:ro,z" "$$@" \
	    --env PATH=/devkit/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
	    --workdir /workspace --env HOME=/tmp/devkit-author-home --env CI=true \
	    --env "XDG_CACHE_HOME=$$cache" --env "PNPM_CONFIG_STORE_DIR=$$store" \
	    --env npm_config_cache=/tmp/devkit-author-npm-cache \
	    --env npm_config_userconfig=/tmp/devkit-author-user.npmrc \
	    --env PNPM_CONFIG_IGNORE_PNPMFILE=true \
	    --env PNPM_CONFIG_PM_ON_FAIL=ignore \
	    --env PNPM_CONFIG_RUNTIME=false --env PNPM_CONFIG_RUNTIME_ON_FAIL=ignore \
	    --env npm_config_globalconfig=/tmp/devkit-author-global.npmrc \
	    --env npm_config_update_notifier=false --env npm_config_fund=false --env npm_config_audit=false \
	    $(3) "$$DEVKIT_AUTHOR_NODE_IMAGE" /devkit/devkit internal node "$$DEVKIT_NODE_VERSION" -- $(2)
endef

# A single writable bind keeps the store and node_modules on one vfsmount.
define author_node_setup
	@test -x "$$DEVKIT_AUTHOR_ENGINE" || { printf '%s\n' 'Native author engine is missing. Run make setup.' >&2; exit 1; }
	@mkdir -p "$$DEVKIT_AUTHOR_PNPM_STORE" "$$DEVKIT_AUTHOR_PNPM_CACHE"
	@set -eu; read -r token < /proc/sys/kernel/random/uuid; name="devkit-author-$$token"; \
	pnpm_bin=$$("$$DEVKIT_AUTHOR_ENGINE" internal ensure-pnpm --pin-file "$$DEVKIT_AUTHOR_ROOT/toolchains.env"); \
	set --; method=hardlink; \
	case "$$DEVKIT_AUTHOR_PNPM_STORE" in \
	  "$$DEVKIT_AUTHOR_ROOT"/*) store="/workspace/$${DEVKIT_AUTHOR_PNPM_STORE#"$$DEVKIT_AUTHOR_ROOT"/}" ;; \
	  *) store=/pnpm-store; method=auto; set -- "$$@" --volume "$$DEVKIT_AUTHOR_PNPM_STORE:/pnpm-store:z" ;; \
	esac; \
	case "$$DEVKIT_AUTHOR_PNPM_CACHE" in \
	  "$$DEVKIT_AUTHOR_ROOT"/*) cache="/workspace/$${DEVKIT_AUTHOR_PNPM_CACHE#"$$DEVKIT_AUTHOR_ROOT"/}" ;; \
	  *) cache=/pnpm-cache; set -- "$$@" --volume "$$DEVKIT_AUTHOR_PNPM_CACHE:/pnpm-cache:z" ;; \
	esac; \
	res_flags=$$("$$DEVKIT_AUTHOR_ENGINE" internal docker-flags --cpus 2 --memory 1024m --pids-limit 512 --node-fallback); \
	exec "$$DEVKIT_AUTHOR_ENGINE" internal guard "$$name" "$$token" -- \
	  docker run --pull never --rm --init --name "$$name" \
	    --label "io.docouno.devkit.invocation=$$token" \
	    --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" --network $(1) --read-only $$res_flags \
	    --cap-drop ALL --security-opt no-new-privileges:true --tmpfs /tmp:rw,exec,mode=1777,size=256m \
	    --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:rw,z" \
	    --volume "$$DEVKIT_AUTHOR_ENGINE:/devkit/devkit:ro,z" \
	    --volume "$$pnpm_bin:/devkit/bin/pnpm:ro,z" "$$@" \
	    --workdir /workspace --env HOME=/tmp/devkit-author-home --env CI=true \
	    --env PATH=/devkit/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
	    --env "XDG_CACHE_HOME=$$cache" --env "PNPM_CONFIG_STORE_DIR=$$store" \
	    --env PNPM_CONFIG_IGNORE_PNPMFILE=true \
	    --env PNPM_CONFIG_PM_ON_FAIL=ignore \
	    --env PNPM_CONFIG_RUNTIME=false --env PNPM_CONFIG_RUNTIME_ON_FAIL=ignore \
	    --env npm_config_cache=/tmp/devkit-author-npm-cache \
	    "$$DEVKIT_AUTHOR_NODE_IMAGE" /devkit/devkit internal node "$$DEVKIT_NODE_VERSION" -- \
	    $(2) --config.package-import-method="$$method"
endef

override author_frozen_env :=
define author_rust
	@test -x "$$DEVKIT_AUTHOR_ENGINE" || { printf '%s\n' 'Native author engine is missing. Run make setup.' >&2; exit 1; }
	@mkdir -p "$$DEVKIT_AUTHOR_CARGO_HOME" "$$DEVKIT_AUTHOR_TARGET" "$$DEVKIT_AUTHOR_ROOT/.artifacts/author"
	@set -eu; read -r token < /proc/sys/kernel/random/uuid; name="devkit-author-$$token"; \
	res_flags=$$("$$DEVKIT_AUTHOR_ENGINE" internal docker-flags --cpus 4 --memory 3072m --pids-limit 512); \
	exec "$$DEVKIT_AUTHOR_ENGINE" internal guard "$$name" "$$token" -- \
	  docker run --pull never --rm --init --name "$$name" \
	    --label "io.docouno.devkit.invocation=$$token" \
	    --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" --network none --read-only $$res_flags \
	    --cap-drop ALL --security-opt no-new-privileges:true --tmpfs /tmp:rw,exec,mode=1777,size=512m \
	    --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:$(or $(2),ro),z" \
	    --volume "$$DEVKIT_AUTHOR_CARGO_HOME:/cargo:z" --volume "$$DEVKIT_AUTHOR_TARGET:/target:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/.artifacts:/workspace/.artifacts:z" \
	    --workdir /workspace --env HOME=/tmp/devkit-author-home --env CARGO_HOME=/cargo \
	    --env CARGO_TARGET_DIR=/target --env "CARGO_BUILD_JOBS=$${CARGO_BUILD_JOBS:-2}" --env RUSTUP_AUTO_INSTALL=0 \
	    $(author_frozen_env) \
	    "$$DEVKIT_AUTHOR_RUST_IMAGE" $(1)
endef

# collect-notices attests the exact native executable and its toolchain.
define contribute
	@set -eu; notice_sha=$$(python3 -c 'import json,sys; print(json.load(sys.stdin)["noticeSetSha256"])' < "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-notice-producer.json"); \
	exec "$$DEVKIT_AUTHOR_TARGET/$$DEVKIT_AUTHOR_TRIPLE/release/devkit" internal contribute \
	  --root "$$DEVKIT_AUTHOR_ROOT" --engine "$$DEVKIT_AUTHOR_TARGET/$$DEVKIT_AUTHOR_TRIPLE/release/devkit" \
	  --notices "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-notices" --notice-set-sha256 "$$notice_sha" \
	  --build-run "$$DEVKIT_BUILD_RUN" --output "$(1)"
endef
