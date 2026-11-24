# The project owns its root Makefile. Component registration is a managed projection.
.DEFAULT_GOAL := help
export DEVKIT_APP := $(value app)
export DEVKIT_PACKAGE := $(value package)
export DEVKIT_FOLLOW := $(value follow)
export DEVKIT_TAIL := $(value tail)
export DEVKIT_HEADLESS := $(or $(value headless),0)
export DEVKIT_CLEANUP := $(or $(value cleanup),0)
export DEVKIT_SYNC_RECONCILE := $(or $(value reconcile),0)
export DEVKIT_SYNC_PREVIEW := $(or $(value preview),0)
export DEVKIT_INFO_JSON := $(or $(value json),0)
export DEVKIT_LIFECYCLE_NAME := $(value name)
export DEVKIT_LIFECYCLE_TEMPLATE := $(value template)
export DEVKIT_LIFECYCLE_API := $(value api)
export DEVKIT_LIFECYCLE_USE := $(value use)
export DEVKIT_LIFECYCLE_PROFILE := $(value profile)
export DEVKIT_LIFECYCLE_TO := $(value to)
export DEVKIT_PACKAGE_AGGREGATE := 0
export DEVKIT_DEV_GROUP :=
export DEVKIT_TARGET_COMPONENT :=

# Capture aliases before unexport, which defines an absent variable in GNU Make.
devkit_command_present := $(if $(filter undefined,$(origin command)),,1)
devkit_cmd_present := $(if $(filter undefined,$(origin cmd)),,1)
export DEVKIT_EXEC_CONFLICT := $(and $(devkit_command_present),$(devkit_cmd_present))
export DEVKIT_EXEC_COMMAND := $(if $(devkit_command_present),$(value command),$(value cmd))
export DEVKIT_EXEC_COMPONENT := $(value component)
export DEVKIT_EXEC_WRITE := $(or $(value write),0)
export DEVKIT_EXEC_NETWORK := $(or $(value network),none)
export DEVKIT_EXEC_PREPARE := $(or $(value prepare),0)
unexport app package follow tail headless cleanup reconcile preview json
unexport name template api use profile to command cmd component write network prepare
unexport DEVKIT_MAKE_DAG DEVKIT_MAKE_PLAN DEVKIT_EXPECTED_MANIFEST DEVKIT_EXPECTED_PLAN
# Clear projection and internal cache variables so ambient environment values
# can never inject targets, shell text, or cached manifest digests.
DEVKIT_REGISTERED_APPS :=
DEVKIT_REGISTERED_PACKAGES :=
DEVKIT_DESKTOP_APPS :=
DEVKIT_PACKAGEABLE_PACKAGES :=
override devkit_manifest_loaded :=
override devkit_manifest_saved :=
unexport DEVKIT_REGISTERED_APPS DEVKIT_REGISTERED_PACKAGES DEVKIT_DESKTOP_APPS DEVKIT_PACKAGEABLE_PACKAGES
unexport devkit_manifest_loaded devkit_manifest_saved

devkit_loader := .devkit/runtime/devkit.sh
devkit_projection := $(shell test -d .devkit && test ! -L .devkit && test -f .devkit/project.mk && test ! -L .devkit/project.mk && test -r .devkit/project.mk && printf yes)
ifeq ($(devkit_projection),yes)
include .devkit/project.mk
endif
# Only values defined by the checked-in projection file may shape Make targets or recipes.
devkit_apps := $(if $(filter file,$(origin DEVKIT_REGISTERED_APPS)),$(value DEVKIT_REGISTERED_APPS))
devkit_packages := $(if $(filter file,$(origin DEVKIT_REGISTERED_PACKAGES)),$(value DEVKIT_REGISTERED_PACKAGES))
devkit_desktop_apps := $(if $(filter file,$(origin DEVKIT_DESKTOP_APPS)),$(value DEVKIT_DESKTOP_APPS))
devkit_packageable_packages := $(if $(filter file,$(origin DEVKIT_PACKAGEABLE_PACKAGES)),$(value DEVKIT_PACKAGEABLE_PACKAGES))
# Only a single hexadecimal word from the checked-in projection may reach a recipe.
devkit_plan := $(if $(filter file,$(origin DEVKIT_EXPECTED_PLAN)),$(value DEVKIT_EXPECTED_PLAN))
devkit_plan_remainder := $(subst 0,,$(subst 1,,$(subst 2,,$(subst 3,,$(subst 4,,$(subst 5,,$(subst 6,,$(subst 7,,$(subst 8,,$(subst 9,,$(subst a,,$(subst b,,$(subst c,,$(subst d,,$(subst e,,$(subst f,,$(devkit_plan)))))))))))))))))
devkit_plan_safe := $(if $(and $(filter 1,$(words $(devkit_plan))),$(if $(devkit_plan_remainder),,1)),$(strip $(devkit_plan)))

# Use the literal inputs throughout the DAG as well as the recipe environment.
devkit_app_valid := $(if $(DEVKIT_APP),$(if $(findstring %,$(DEVKIT_APP)),,$(if $(filter 1,$(words $(DEVKIT_APP))),$(filter $(DEVKIT_APP),$(devkit_apps)),)),1)
devkit_package_valid := $(if $(DEVKIT_PACKAGE),$(if $(findstring %,$(DEVKIT_PACKAGE)),,$(if $(filter 1,$(words $(DEVKIT_PACKAGE))),$(filter $(DEVKIT_PACKAGE),$(devkit_packages)),)),1)
devkit_selectors_valid := $(if $(and $(DEVKIT_APP),$(DEVKIT_PACKAGE)),,$(and $(devkit_app_valid),$(devkit_package_valid)))
devkit_dev_selectors_valid := $(if $(DEVKIT_PACKAGE),,$(devkit_app_valid))

devkit_dev_aggregate := $(if $(DEVKIT_APP),0,$(if $(word 2,$(devkit_apps)),1,0))
devkit_needs_desktop_probe := $(if $(filter dev up,$(MAKECMDGOALS)),$(if $(filter 1,$(devkit_dev_aggregate)),$(if $(filter 0,$(DEVKIT_HEADLESS)),$(devkit_desktop_apps))))
ifneq ($(devkit_needs_desktop_probe),)
devkit_host_os := $(shell uname -s)
ifeq ($(devkit_host_os),Darwin)
devkit_skip_gui := 1
devkit_skip_reason = on macOS; use headless=1.
else
devkit_skip_gui := $(if $(value DISPLAY),,1)
devkit_skip_reason = without DISPLAY; use headless=1 or app=$(desktop).
endif
else
devkit_skip_gui :=
devkit_skip_reason =
endif
devkit_skipped_desktop := $(if $(devkit_needs_desktop_probe),$(if $(devkit_skip_gui),$(devkit_desktop_apps)))
devkit_selected_apps := $(if $(devkit_dev_selectors_valid),$(if $(DEVKIT_APP),$(filter $(DEVKIT_APP),$(devkit_apps)),$(filter-out $(devkit_skipped_desktop),$(devkit_apps))),)
devkit_targets = $(if $(devkit_selectors_valid),$(if $(DEVKIT_APP),$(addprefix $(1)-,$(filter $(DEVKIT_APP),$(devkit_apps))),$(if $(DEVKIT_PACKAGE),$(addprefix $(1)/packages/,$(filter $(DEVKIT_PACKAGE),$(devkit_packages))),$(addprefix $(1)-,$(devkit_apps)) $(addprefix $(1)/packages/,$(devkit_packages)))),)
devkit_app_targets := $(foreach command,setup check fix test build dev package,$(addprefix $(command)-,$(devkit_apps)))
devkit_package_targets := $(foreach command,setup check fix test build package,$(addprefix $(command)/packages/,$(devkit_packages)))
devkit_dag := setup check fix test build dev up package setup-node-workspace $(devkit_app_targets) $(devkit_package_targets)

.PHONY: help info status doctor down logs sync add-app add-package register update recover $(devkit_dag)
# One Make invocation shares dependencies/output paths, even when -j is inherited.
# Native tools may still parallelize internally; separate invocations use admission.
.NOTPARALLEL:

help info status doctor down logs sync add-app add-package register update recover:
	@exec sh $(devkit_loader) $@

# Cache the first DAG recipe's manifest digest for this entire Make invocation.
# The plan token comes from the projection Make parsed, before any prerequisite.
devkit_manifest_digest = $(if $(devkit_manifest_loaded),,$(eval override devkit_manifest_saved := $(shell sh .devkit/runtime/devkit.sh make-manifest | awk 'NR == 1 && length($$0) == 64 && $$0 !~ /[^a-f0-9]/ { print; exit }'))$(eval override devkit_manifest_loaded := 1))$(devkit_manifest_saved)
devkit_task = DEVKIT_MAKE_DAG=1 DEVKIT_MAKE_PLAN=$(devkit_plan_safe) DEVKIT_EXPECTED_MANIFEST=$(devkit_manifest_digest) exec sh $(devkit_loader)

setup: $(call devkit_targets,setup)
check: $(call devkit_targets,check)
fix: $(call devkit_targets,fix)
test: $(call devkit_targets,test)
build: $(call devkit_targets,build)
setup check fix test build:
	@$(devkit_task) snapshot

dev up: export DEVKIT_DEV_GROUP := $(devkit_selected_apps)
dev up: devkit-dev-selection $(addprefix dev-,$(devkit_selected_apps))
.PHONY: devkit-dev-selection
devkit-dev-selection:
	@$(foreach desktop,$(devkit_skipped_desktop),printf '%s\n' 'devkit: warning: skipping Electron app $(desktop) $(devkit_skip_reason)' >&2;):
dev up:
	@$(devkit_task) dev-snapshot
package: export DEVKIT_PACKAGE_AGGREGATE := $(if $(DEVKIT_APP)$(DEVKIT_PACKAGE),0,1)
package: $(if $(DEVKIT_APP)$(DEVKIT_PACKAGE),$(call devkit_targets,package),$(addprefix package-,$(devkit_desktop_apps)) $(addprefix package/packages/,$(devkit_packageable_packages)))
	@$(devkit_task) package-snapshot
setup-node-workspace:
	@$(devkit_task) setup-node
setup-node-workspace: export DEVKIT_TARGET_COMPONENT :=

ifneq ($(devkit_app_targets),)
$(devkit_app_targets): export DEVKIT_TARGET_COMPONENT = apps/$*
endif
ifneq ($(devkit_package_targets),)
$(devkit_package_targets): export DEVKIT_TARGET_COMPONENT = packages/$*
endif

ifneq ($(devkit_apps),)
$(addprefix setup-,$(devkit_apps)): setup-%:
	@$(devkit_task) setup
$(addprefix check-,$(devkit_apps)): check-%:
	@$(devkit_task) check
$(addprefix fix-,$(devkit_apps)): fix-%:
	@$(devkit_task) fix
$(addprefix test-,$(devkit_apps)): test-%:
	@$(devkit_task) test
$(addprefix build-,$(devkit_apps)): build-%:
	@$(devkit_task) build
$(addprefix dev-,$(devkit_apps)): dev-%:
	@$(devkit_task) dev
$(addprefix package-,$(devkit_apps)): package-%:
	@$(devkit_task) package
endif

ifneq ($(devkit_packages),)
$(addprefix setup/packages/,$(devkit_packages)): setup/packages/%:
	@$(devkit_task) setup
$(addprefix check/packages/,$(devkit_packages)): check/packages/%:
	@$(devkit_task) check
$(addprefix fix/packages/,$(devkit_packages)): fix/packages/%:
	@$(devkit_task) fix
$(addprefix test/packages/,$(devkit_packages)): test/packages/%:
	@$(devkit_task) test
$(addprefix build/packages/,$(devkit_packages)): build/packages/%:
	@$(devkit_task) build
$(addprefix package/packages/,$(devkit_packages)): package/packages/%:
	@$(devkit_task) package
endif

.PHONY: exec
exec:
	@exec sh $(devkit_loader) exec
