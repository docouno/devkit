# The project owns its root Makefile. Component registration is a managed projection.
.DEFAULT_GOAL := help
export DEVKIT_APP := $(app)
export DEVKIT_PACKAGE := $(package)
export DEVKIT_FOLLOW := $(follow)
export DEVKIT_TAIL := $(tail)
export DEVKIT_HEADLESS := $(or $(headless),0)
export DEVKIT_TARGET_COMPONENT :=
export DEVKIT_EXPECTED_MANIFEST := $(word 1,$(shell sha256sum .devkit/manifest.json 2>/dev/null))

include .devkit/project.mk

devkit_selected_apps := $(if $(strip $(app)),$(app),$(DEVKIT_REGISTERED_APPS))
devkit_selected_packages := $(if $(strip $(package)),$(package),$(DEVKIT_REGISTERED_PACKAGES))
devkit_targets = $(if $(strip $(app)),$(addprefix $(1)-,$(app)),$(if $(strip $(package)),$(addprefix $(1)/packages/,$(package)),$(addprefix $(1)-,$(DEVKIT_REGISTERED_APPS)) $(addprefix $(1)/packages/,$(DEVKIT_REGISTERED_PACKAGES))))

.PHONY: help doctor setup check fix test build dev up down logs status package setup-node-workspace
# One Make invocation shares dependencies/output paths, even when -j is inherited.
# Native tools may still parallelize internally; separate invocations use admission.
.NOTPARALLEL:

help doctor down logs status:
	@exec bash .devkit/runtime/devkit.sh $@

setup: $(call devkit_targets,setup)
check: $(call devkit_targets,check)
fix: $(call devkit_targets,fix)
test: $(call devkit_targets,test)
build: $(call devkit_targets,build)
setup check fix test build:
	@exec bash .devkit/runtime/devkit.sh snapshot

dev up: $(addprefix dev-,$(devkit_selected_apps))
dev up:
	@exec bash .devkit/runtime/devkit.sh snapshot
package: $(if $(strip $(package)),,$(addprefix package-,$(if $(strip $(app)),$(app),$(DEVKIT_DESKTOP_APPS))))
	@exec bash .devkit/runtime/devkit.sh package-snapshot
setup-node-workspace:
	@exec bash .devkit/runtime/devkit.sh setup-node
setup-node-workspace: export DEVKIT_TARGET_COMPONENT :=

.PHONY: $(foreach command,setup check fix test build dev package,$(addprefix $(command)-,$(DEVKIT_REGISTERED_APPS)))
.PHONY: $(foreach command,setup check fix test build,$(addprefix $(command)/packages/,$(DEVKIT_REGISTERED_PACKAGES)))
$(foreach command,setup check fix test build dev package,$(addprefix $(command)-,$(DEVKIT_REGISTERED_APPS))): export DEVKIT_TARGET_COMPONENT = apps/$*
$(foreach command,setup check fix test build,$(addprefix $(command)/packages/,$(DEVKIT_REGISTERED_PACKAGES))): export DEVKIT_TARGET_COMPONENT = packages/$*

$(addprefix setup-,$(DEVKIT_REGISTERED_APPS)): setup-%:
	@exec bash .devkit/runtime/devkit.sh setup
$(addprefix check-,$(DEVKIT_REGISTERED_APPS)): check-%:
	@exec bash .devkit/runtime/devkit.sh check
$(addprefix fix-,$(DEVKIT_REGISTERED_APPS)): fix-%:
	@exec bash .devkit/runtime/devkit.sh fix
$(addprefix test-,$(DEVKIT_REGISTERED_APPS)): test-%:
	@exec bash .devkit/runtime/devkit.sh test
$(addprefix build-,$(DEVKIT_REGISTERED_APPS)): build-%:
	@exec bash .devkit/runtime/devkit.sh build
$(addprefix dev-,$(DEVKIT_REGISTERED_APPS)): dev-%:
	@exec bash .devkit/runtime/devkit.sh dev
$(addprefix package-,$(DEVKIT_REGISTERED_APPS)): package-%:
	@exec bash .devkit/runtime/devkit.sh package

$(addprefix setup/packages/,$(DEVKIT_REGISTERED_PACKAGES)): setup/packages/%:
	@exec bash .devkit/runtime/devkit.sh setup
$(addprefix check/packages/,$(DEVKIT_REGISTERED_PACKAGES)): check/packages/%:
	@exec bash .devkit/runtime/devkit.sh check
$(addprefix fix/packages/,$(DEVKIT_REGISTERED_PACKAGES)): fix/packages/%:
	@exec bash .devkit/runtime/devkit.sh fix
$(addprefix test/packages/,$(DEVKIT_REGISTERED_PACKAGES)): test/packages/%:
	@exec bash .devkit/runtime/devkit.sh test
$(addprefix build/packages/,$(DEVKIT_REGISTERED_PACKAGES)): build/packages/%:
	@exec bash .devkit/runtime/devkit.sh build
