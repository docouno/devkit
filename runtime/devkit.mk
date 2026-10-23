# Consumer root: DEVKIT_APPS := main (or: web api), then include this file.
# Deliberately no .ONESHELL: failures must not be hidden by a later command.
.DEFAULT_GOAL := help
export DEVKIT_APPS
export DEVKIT_APP := $(app)
export DEVKIT_FOLLOW := $(follow)
export DEVKIT_TAIL := $(tail)
# A version update between leaf recipes must not produce a mixed green result.
# The updater changes generation even when returning to an older release (ABA).
export DEVKIT_EXPECTED_MANIFEST := $(word 1,$(shell sha256sum .devkit/manifest.json 2>/dev/null))

.PHONY: help doctor setup check fix test build dev up down logs status
help doctor dev up down logs status:
	@exec bash .devkit/runtime/devkit.sh $@

setup: $(if $(strip $(app)),setup-$(app),$(addprefix setup-,$(DEVKIT_APPS)))
check: $(if $(strip $(app)),check-$(app),$(addprefix check-,$(DEVKIT_APPS)))
fix: $(if $(strip $(app)),fix-$(app),$(addprefix fix-,$(DEVKIT_APPS)))
test: $(if $(strip $(app)),test-$(app),$(addprefix test-,$(DEVKIT_APPS)))
build: $(if $(strip $(app)),build-$(app),$(addprefix build-,$(DEVKIT_APPS)))
setup check fix test build:
	@exec bash .devkit/runtime/devkit.sh snapshot

.PHONY: $(foreach command,setup check fix test build,$(addprefix $(command)-,$(DEVKIT_APPS)))
$(foreach command,setup check fix test build,$(addprefix $(command)-,$(DEVKIT_APPS))): export DEVKIT_TARGET_APP = $*
$(addprefix setup-,$(DEVKIT_APPS)): setup-%:
	@exec bash .devkit/runtime/devkit.sh setup
$(addprefix check-,$(DEVKIT_APPS)): check-%:
	@exec bash .devkit/runtime/devkit.sh check
$(addprefix fix-,$(DEVKIT_APPS)): fix-%:
	@exec bash .devkit/runtime/devkit.sh fix
$(addprefix test-,$(DEVKIT_APPS)): test-%:
	@exec bash .devkit/runtime/devkit.sh test
$(addprefix build-,$(DEVKIT_APPS)): build-%:
	@exec bash .devkit/runtime/devkit.sh build
