# Changelog

## [Unreleased]

### Changed

- Generate GitHub release descriptions from the matching version entry in this changelog.
- Keep build contracts and verification receipts in a collapsed section below the release notes.

## [0.3.2] - 2026-10-24

### Fixed

- Wait for npm to expose a successful `latest` update before completing release promotion.
- Compact verification receipts without dropping their measurements or integrity bindings.
- Keep repeated release-note updates stable and preserve surrounding operator notes.

## [0.3.1] - 2026-10-24

### Fixed

- Wait for newly published npm versions to become readable instead of failing immediately after a successful publish.
- Require an existing stable version before the normal candidate-to-latest release flow.

### Verification

- Add a public native-to-native upgrade check, including accepted-version pins, source preservation and recovery.

## [0.3.0] - 2026-10-24

### Added

- A standalone Rust CLI for project creation, component registration, environment updates, recovery and daily orchestration.
- Direct npm and curl entry points, an interactive wizard and JSON output for automation.
- A verified release cache and explicit accepted-version pins for each project.
- Containerized native command execution for editing dependencies with the accepted toolchain.

### Changed

- Replace the TypeScript/Bash management core and Node control containers with the native CLI.
- Preserve the root Makefile's task graph and product-owned source, dependencies and configuration during updates.
- Build and verify one frozen artifact set in GitHub Actions before publishing and promoting it.

### Upgrade

- Use this release's native npm/curl entry point to upgrade 0.2.0 or 0.2.1 projects. Older versioned bootstrap scripts retain their original behavior.

## [0.2.1] - 2026-10-23

### Fixed

- Use the public GitHub release endpoint by default in the bootstrap.
- Correct curl protocol-option quoting for Bash and zsh.
- Add repository, homepage and issue links to the environment package.

### Verification

- Verify actual GitHub downloads, npm integrity and explicit environment upgrades through the public distribution path.

## [0.2.0] - 2026-10-23

### Added

- Modular Node API, Rust API, React web and Electron templates, plus a shared React client package.
- A fullstack desktop preset combining a Rust API, web, Electron and shared client code.
- Pinned container tooling, common Make commands and explicit environment updates with conflict detection and recovery.

### Platform

- Linux with local Docker Compose. Desktop packaging produces a Linux directory and archive.
