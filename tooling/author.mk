# Docker argv and mounts only. Process ownership/cancellation stays in native Rust.
export DEVKIT_AUTHOR_ROOT := $(CURDIR)
export DEVKIT_AUTHOR_UID := $(shell id -u)
export DEVKIT_AUTHOR_GID := $(shell id -g)
export DEVKIT_AUTHOR_NODE_IMAGE := $(DEVKIT_NODE_IMAGE)
export DEVKIT_NODE_VERSION
export DEVKIT_RUST_VERSION
author_recipe_sha := $(word 1,$(shell sha256sum tooling/native.Dockerfile))
export DEVKIT_AUTHOR_RUST_IMAGE ?= devkit-author-rust:$(author_recipe_sha)
export DEVKIT_AUTHOR_CARGO_HOME ?= $(CURDIR)/.artifacts/author/cargo-home
export DEVKIT_AUTHOR_TARGET := $(CURDIR)/.artifacts/author/target
export DEVKIT_AUTHOR_ENGINE := $(CURDIR)/.artifacts/author/target/x86_64-unknown-linux-musl/debug/devkit
export DEVKIT_RELEASE_VERSION := $(or $(version),$(strip $(file <VERSION)))
export DEVKIT_FROM_VERSION := $(or $(from_version),0.2.1)
export DEVKIT_BUILD_ID := $(or $(build_id),$(shell cat /proc/sys/kernel/random/uuid))
export DEVKIT_BUILD_RUN := $(or $(build_run),local-author)
export DEVKIT_ASSEMBLY_OUTPUT := $(output)
export DEVKIT_BUILD_SET := $(build_set)
export DEVKIT_VERIFY_RELEASE := $(release)
export DEVKIT_VERIFY_PREVIOUS := $(previous)
export DEVKIT_LEGACY_ARCHIVE := $(legacy_archive)
export DEVKIT_INIT_TARGET := $(target)
export DEVKIT_INIT_TEMPLATE := $(or $(template),node-api)

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
	    --volume "$$DEVKIT_AUTHOR_ROOT:/workspace:ro,z" \
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
