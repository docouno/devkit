# The project owns its root Makefile. Component registration is a managed projection.
.DEFAULT_GOAL := help
export DEVKIT_APP := $(app)
export DEVKIT_PACKAGE := $(package)
export DEVKIT_FOLLOW := $(follow)
export DEVKIT_TAIL := $(tail)
export DEVKIT_HEADLESS := $(or $(headless),0)
export DEVKIT_TARGET_COMPONENT :=
export DEVKIT_EXPECTED_MANIFEST := $(word 1,$(shell sha256sum .devkit/manifest.json 2>/dev/null))

# Kept at the public include path; old accepted payloads use their old runtime.
devkit_runtime_version := $(strip $(shell sed -n 's/^DEVKIT_VERSION=//p' .devkit/runtime/toolchains.env))
devkit_loader := $(if $(filter 0.2.0 0.2.1 0.2.2,$(devkit_runtime_version)),.devkit/runtime/devkit.sh,.devkit/runtime/native-devkit.sh)
include .devkit/project.mk

devkit_selected_apps := $(if $(strip $(app)),$(app),$(DEVKIT_REGISTERED_APPS))
devkit_selected_packages := $(if $(strip $(package)),$(package),$(DEVKIT_REGISTERED_PACKAGES))
devkit_targets = $(if $(strip $(app)),$(addprefix $(1)-,$(app)),$(if $(strip $(package)),$(addprefix $(1)/packages/,$(package)),$(addprefix $(1)-,$(DEVKIT_REGISTERED_APPS)) $(addprefix $(1)/packages/,$(DEVKIT_REGISTERED_PACKAGES))))

.PHONY: help doctor setup check fix test build dev up down logs status package setup-node-workspace
# One Make invocation shares dependencies/output paths, even when -j is inherited.
# Native tools may still parallelize internally; separate invocations use admission.
.NOTPARALLEL:

help doctor down logs status:
	@exec bash $(devkit_loader) $@

setup: $(call devkit_targets,setup)
check: $(call devkit_targets,check)
fix: $(call devkit_targets,fix)
test: $(call devkit_targets,test)
build: $(call devkit_targets,build)
setup check fix test build:
	@exec bash $(devkit_loader) snapshot

dev up: $(addprefix dev-,$(devkit_selected_apps))
dev up:
	@exec bash $(devkit_loader) snapshot
package: $(if $(strip $(package)),,$(addprefix package-,$(if $(strip $(app)),$(app),$(DEVKIT_DESKTOP_APPS))))
	@exec bash $(devkit_loader) package-snapshot
setup-node-workspace:
	@exec bash $(devkit_loader) setup-node
setup-node-workspace: export DEVKIT_TARGET_COMPONENT :=

.PHONY: $(foreach command,setup check fix test build dev package,$(addprefix $(command)-,$(DEVKIT_REGISTERED_APPS)))
.PHONY: $(foreach command,setup check fix test build,$(addprefix $(command)/packages/,$(DEVKIT_REGISTERED_PACKAGES)))
$(foreach command,setup check fix test build dev package,$(addprefix $(command)-,$(DEVKIT_REGISTERED_APPS))): export DEVKIT_TARGET_COMPONENT = apps/$*
$(foreach command,setup check fix test build,$(addprefix $(command)/packages/,$(DEVKIT_REGISTERED_PACKAGES))): export DEVKIT_TARGET_COMPONENT = packages/$*

$(addprefix setup-,$(DEVKIT_REGISTERED_APPS)): setup-%:
	@exec bash $(devkit_loader) setup
$(addprefix check-,$(DEVKIT_REGISTERED_APPS)): check-%:
	@exec bash $(devkit_loader) check
$(addprefix fix-,$(DEVKIT_REGISTERED_APPS)): fix-%:
	@exec bash $(devkit_loader) fix
$(addprefix test-,$(DEVKIT_REGISTERED_APPS)): test-%:
	@exec bash $(devkit_loader) test
$(addprefix build-,$(DEVKIT_REGISTERED_APPS)): build-%:
	@exec bash $(devkit_loader) build
$(addprefix dev-,$(DEVKIT_REGISTERED_APPS)): dev-%:
	@exec bash $(devkit_loader) dev
$(addprefix package-,$(DEVKIT_REGISTERED_APPS)): package-%:
	@exec bash $(devkit_loader) package

$(addprefix setup/packages/,$(DEVKIT_REGISTERED_PACKAGES)): setup/packages/%:
	@exec bash $(devkit_loader) setup
$(addprefix check/packages/,$(DEVKIT_REGISTERED_PACKAGES)): check/packages/%:
	@exec bash $(devkit_loader) check
$(addprefix fix/packages/,$(DEVKIT_REGISTERED_PACKAGES)): fix/packages/%:
	@exec bash $(devkit_loader) fix
$(addprefix test/packages/,$(DEVKIT_REGISTERED_PACKAGES)): test/packages/%:
	@exec bash $(devkit_loader) test
$(addprefix build/packages/,$(DEVKIT_REGISTERED_PACKAGES)): build/packages/%:
	@exec bash $(devkit_loader) build

# command= uses shellwords syntax without expansions. Exact argv: devkit exec -- ...
export DEVKIT_EXEC_COMMAND := $(value command)
export DEVKIT_EXEC_WRITE := $(or $(write),0)
export DEVKIT_EXEC_NETWORK := $(or $(network),none)
export DEVKIT_EXEC_PREPARE := $(or $(prepare),0)
.PHONY: exec
exec:
	@exec bash $(devkit_loader) exec
