# Docker argv and mounts only. Process ownership/cancellation stays in native Rust.
# Freeze operator data before Make exports command-line variables to $(shell).
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
ifneq ($(origin legacy_archive),undefined)
override legacy_archive := $(value legacy_archive)
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
export DEVKIT_AUTHOR_ROOT := $(CURDIR)
export DEVKIT_AUTHOR_UID := $(shell id -u)
export DEVKIT_AUTHOR_GID := $(shell id -g)
export DEVKIT_AUTHOR_NODE_IMAGE := $(DEVKIT_NODE_IMAGE)
export DEVKIT_NODE_VERSION
export DEVKIT_RUST_VERSION
author_recipe_sha := $(word 1,$(shell sha256sum tooling/native.Dockerfile))
override DEVKIT_AUTHOR_RUST_IMAGE := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_RUST_IMAGE)),devkit-author-rust:$(author_recipe_sha),$(value DEVKIT_AUTHOR_RUST_IMAGE))
export DEVKIT_AUTHOR_RUST_IMAGE
override DEVKIT_AUTHOR_CARGO_HOME := $(if $(filter undefined,$(origin DEVKIT_AUTHOR_CARGO_HOME)),$(CURDIR)/.artifacts/author/cargo-home,$(value DEVKIT_AUTHOR_CARGO_HOME))
export DEVKIT_AUTHOR_CARGO_HOME
export DEVKIT_AUTHOR_TARGET := $(CURDIR)/.artifacts/author/target
export DEVKIT_AUTHOR_ENGINE := $(CURDIR)/.artifacts/author/target/x86_64-unknown-linux-musl/debug/devkit
export DEVKIT_RELEASE_VERSION := $(if $(filter undefined,$(origin version)),$(strip $(file <VERSION)),$(value version))
export DEVKIT_FROM_VERSION := $(if $(filter undefined,$(origin from_version)),0.2.1,$(value from_version))
export DEVKIT_BUILD_ID := $(if $(filter undefined,$(origin build_id)),$(shell cat /proc/sys/kernel/random/uuid),$(value build_id))
export DEVKIT_BUILD_RUN := $(if $(filter undefined,$(origin build_run)),local-author,$(value build_run))
export DEVKIT_ASSEMBLY_OUTPUT := $(value output)
export DEVKIT_BUILD_SET := $(value build_set)
export DEVKIT_VERIFY_RELEASE := $(value release)
export DEVKIT_VERIFY_PREVIOUS := $(value previous)
export DEVKIT_LEGACY_ARCHIVE := $(value legacy_archive)
export DEVKIT_INIT_TARGET := $(value target)
export DEVKIT_INIT_TEMPLATE := $(if $(filter undefined,$(origin template)),node-api,$(value template))

ifneq ($(origin release),undefined)
export DEVKIT_VERIFY_ENGINE := $(value release)/devkit
else
export DEVKIT_VERIFY_ENGINE := $(CURDIR)/.artifacts/author/target/x86_64-unknown-linux-musl/release/devkit
author_blackbox_build := build
endif

export DEVKIT_EXPECTED_INDEX_SHA256 := $(value expected_index_sha256)

define author_node
	@test -x "$$DEVKIT_AUTHOR_ENGINE" || { printf '%s\n' 'Native author engine is missing. Run make setup.' >&2; exit 1; }
	@mkdir -p "$$DEVKIT_AUTHOR_ROOT/node_modules" "$$DEVKIT_AUTHOR_ROOT/packages/env/node_modules" "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/npm" "$$DEVKIT_AUTHOR_ROOT/.tmp"
	@set -eu; read -r token < /proc/sys/kernel/random/uuid; name="devkit-author-$$token"; \
	exec "$$DEVKIT_AUTHOR_ENGINE" internal guard "$$name" "$$token" -- \
	  docker run --pull never --rm --init --name "$$name" \
	    --label "io.docouno.devkit.invocation=$$token" \
	    --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" \
	    --network $(1) --read-only --memory 768m --cpus 2 \
	    --cap-drop ALL --security-opt no-new-privileges:true --tmpfs /tmp:rw,exec,mode=1777 \
	    --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:$(or $(4),ro),z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/node_modules:/workspace/node_modules:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/packages/env/node_modules:/workspace/packages/env/node_modules:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/.artifacts:/workspace/.artifacts:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/.tmp:/workspace/.tmp:z" \
	    --volume "$$DEVKIT_AUTHOR_ENGINE:/devkit/devkit:ro,z" \
	    --workdir /workspace --env HOME=/tmp/devkit-author-home --env CI=true \
	    --env npm_config_cache=/workspace/.artifacts/author/npm \
	    --env npm_config_userconfig=/tmp/devkit-author-user.npmrc \
	    --env npm_config_globalconfig=/tmp/devkit-author-global.npmrc \
	    --env npm_config_update_notifier=false --env npm_config_fund=false --env npm_config_audit=false \
	    $(3) "$$DEVKIT_AUTHOR_NODE_IMAGE" /devkit/devkit internal node "$$DEVKIT_NODE_VERSION" -- $(2)
endef

define author_rust
	@test -x "$$DEVKIT_AUTHOR_ENGINE" || { printf '%s\n' 'Native author engine is missing. Run make setup.' >&2; exit 1; }
	@mkdir -p "$$DEVKIT_AUTHOR_CARGO_HOME" "$$DEVKIT_AUTHOR_TARGET" "$$DEVKIT_AUTHOR_ROOT/.artifacts/author"
	@set -eu; read -r token < /proc/sys/kernel/random/uuid; name="devkit-author-$$token"; \
	DEVKIT_SOURCE_COMMIT=$$(env -i PATH="$$PATH" git -C "$$DEVKIT_AUTHOR_ROOT" rev-parse HEAD); \
	DEVKIT_SOURCE_TREE=$$(env -i PATH="$$PATH" git -C "$$DEVKIT_AUTHOR_ROOT" rev-parse 'HEAD^{tree}'); \
	export DEVKIT_SOURCE_COMMIT DEVKIT_SOURCE_TREE; \
	exec "$$DEVKIT_AUTHOR_ENGINE" internal guard "$$name" "$$token" -- \
	  docker run --pull never --rm --init --name "$$name" \
	    --label "io.docouno.devkit.invocation=$$token" \
	    --user "$$DEVKIT_AUTHOR_UID:$$DEVKIT_AUTHOR_GID" --network none --read-only --memory 2g --cpus 2 \
	    --cap-drop ALL --security-opt no-new-privileges:true --tmpfs /tmp:rw,exec,mode=1777 \
	    --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:$(or $(2),ro),z" \
	    --volume "$$DEVKIT_AUTHOR_CARGO_HOME:/cargo:z" --volume "$$DEVKIT_AUTHOR_TARGET:/target:z" \
	    --volume "$$DEVKIT_AUTHOR_ROOT/.artifacts:/workspace/.artifacts:z" \
	    --workdir /workspace --env HOME=/tmp/devkit-author-home --env CARGO_HOME=/cargo \
	    --env CARGO_TARGET_DIR=/target --env CARGO_BUILD_JOBS=1 --env RUSTUP_AUTO_INSTALL=0 \
	    --env DEVKIT_SOURCE_COMMIT --env DEVKIT_SOURCE_TREE \
	    "$$DEVKIT_AUTHOR_RUST_IMAGE" $(1)
endef

define assemble
	@test -n "$$DEVKIT_ASSEMBLY_OUTPUT" || { printf '%s\n' 'Set output=/absolute/new/build-set; existing outputs are never replaced.' >&2; exit 1; }
	@set -eu; source_commit=$$(env -i PATH="$$PATH" git -C "$$DEVKIT_AUTHOR_ROOT" rev-parse HEAD); \
	source_tree=$$(env -i PATH="$$PATH" git -C "$$DEVKIT_AUTHOR_ROOT" rev-parse 'HEAD^{tree}'); \
	notice_sha=$$(python3 -c 'import json,sys; print(json.load(sys.stdin)["noticeSetSha256"])' < "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-notice-producer.json"); \
	exec "$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/release/devkit" internal assemble \
	  --root "$$DEVKIT_AUTHOR_ROOT" --engine "$$DEVKIT_AUTHOR_TARGET/x86_64-unknown-linux-musl/release/devkit" \
	  --npm-inputs "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-npm" \
	  --notices "$$DEVKIT_AUTHOR_ROOT/.artifacts/author/$$DEVKIT_BUILD_ID-notices" --notice-set-sha256 "$$notice_sha" \
	  --output "$$DEVKIT_ASSEMBLY_OUTPUT" --source-commit "$$source_commit" --source-tree "$$source_tree" \
	  --build-run "$$DEVKIT_BUILD_RUN" $${DEVKIT_ASSEMBLE_LOCAL:-}
endef
