SHELL := /bin/bash
.DEFAULT_GOAL := help
VERSION := $(strip $(file <VERSION))
release_version := $(if $(version),$(version),$(VERSION))
export DEVKIT_RELEASE_VERSION := $(release_version)
export DEVKIT_NEXT_VERSION := $(or $(next_version),0.2.2)
export DEVKIT_INIT_TARGET := $(target)
export DEVKIT_INIT_TEMPLATE := $(or $(template),node)

.PHONY: help setup check test build pack pack-next verify verify-runtime verify-rust verify-bootstrap verify-packaged verify-registry verify-public init
help:
	@printf '%s\n' 'Devkit author workspace (Linux + Docker Compose)' \
	  'make setup                    Install the pinned author dependencies' \
	  'make check                    Typecheck, format-check and test the implementation' \
	  'make build                    Build the factory helper' \
	  'make pack [version=X.Y.Z]      Build a versioned local release and npm archives' \
	  'make verify                   Verify packaged consumers and release invariants' \
	  'make verify-public from_version=0.2.0 version=0.2.1  Verify public GitHub/npm flows' \
	  'make init template=node target=/path/to/project  Create an unpublished local consumer'
setup:
	@bash tooling/node.sh --setup npm ci --include=dev --strict-peer-deps
check:
	@bash tooling/node.sh npm run check
test:
	@bash tooling/node.sh npm run test:core
	@bash tooling/node.sh npm run test:env
build:
	@bash tooling/node.sh npm run build
pack: build
	@bash tooling/node.sh node .artifacts/cli-author/release.js --version "$$DEVKIT_RELEASE_VERSION"
pack-next: pack
	@bash tooling/node.sh node .artifacts/cli-author/release.js --version "$$DEVKIT_NEXT_VERSION"
verify-runtime:
	@python3 test/runtime/test_runtime.py
verify-rust:
	@python3 test/rust-check/test_rust_check.py
verify-bootstrap: pack-next
	@bash test/verify-bootstrap.sh --release ".artifacts/devkit-$$DEVKIT_RELEASE_VERSION.tar.gz" --next-release ".artifacts/devkit-$$DEVKIT_NEXT_VERSION.tar.gz"
verify-packaged: pack-next
	@DEVKIT_VERIFY_NEXT_RELEASE=".artifacts/releases/$$DEVKIT_NEXT_VERSION" bash test/verify-consumers.sh ".artifacts/releases/$$DEVKIT_RELEASE_VERSION"
verify-registry: pack-next
	@bash test/verify-registry.sh ".artifacts/releases/$$DEVKIT_RELEASE_VERSION" ".artifacts/releases/$$DEVKIT_NEXT_VERSION"
verify-public:
	@bash test/verify-public.sh --version "$(or $(from_version),0.2.0)" --next-version "$$DEVKIT_RELEASE_VERSION"
verify: check pack-next
	@$(MAKE) --no-print-directory verify-runtime verify-rust
	@bash test/verify-bootstrap.sh --release ".artifacts/devkit-$$DEVKIT_RELEASE_VERSION.tar.gz" --next-release ".artifacts/devkit-$$DEVKIT_NEXT_VERSION.tar.gz"
	@DEVKIT_VERIFY_NEXT_RELEASE=".artifacts/releases/$$DEVKIT_NEXT_VERSION" bash test/verify-consumers.sh ".artifacts/releases/$$DEVKIT_RELEASE_VERSION"
	@bash test/verify-registry.sh ".artifacts/releases/$$DEVKIT_RELEASE_VERSION" ".artifacts/releases/$$DEVKIT_NEXT_VERSION"
init: pack
	@".artifacts/releases/$$DEVKIT_RELEASE_VERSION/devkit" init "$$DEVKIT_INIT_TARGET" --template "$$DEVKIT_INIT_TEMPLATE" --package-source local
