# Continuous Integration

Devkit projects require Linux x86-64 or ARM64, Docker Engine with Docker Compose `5.0.0` or newer, and GNU Make on CI runners — no host Node.js or Rust toolchains are required to run `make setup`, `make check`, `make test`, `make build`, or `make package`.

## GitHub Actions

```yaml
name: ci

on:
  pull_request:
  push:
    branches: [main]

jobs:
  verify:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4

      - name: Set up Docker Compose
        uses: docker/setup-compose-action@54042514f505b273907334ae2b9cdbb9a0213c1a # v2.4.0
        with:
          version: v5.6.0

      - name: Configure Devkit cache directory
        run: echo "DEVKIT_CACHE_DIR=${RUNNER_TEMP}/devkit-cache" >> "$GITHUB_ENV"

      - name: Cache Devkit engine and project caches
        uses: actions/cache@v4
        with:
          path: |
            ${{ env.DEVKIT_CACHE_DIR }}
            .devkit/cache
          key: devkit-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('.devkit/manifest.json', 'pnpm-lock.yaml', 'Cargo.lock') }}
          restore-keys: |
            devkit-${{ runner.os }}-${{ runner.arch }}-

      - name: Setup pinned environment
        env:
          # Only needed if .npmrc references a private npm registry token:
          NPM_REGISTRY_TOKEN: ${{ secrets.NPM_REGISTRY_TOKEN }}
        run: make setup

      - name: Run quality gates and build
        run: |
          make check
          make test
          make build
          # make package  # Uncomment when packaging an Electron app or a "private": false library

      - name: Upload packaged release artifacts
        if: hashFiles('packages/*/release/*.tgz', 'apps/*/release/*.tar.gz') != ''
        uses: actions/upload-artifact@v4
        with:
          name: release-packages
          path: |
            packages/*/release/*
            apps/*/release/*
```

## GitLab CI (with Private Registry and Custom Enterprise CA)

```yaml
stages:
  - verify
  - publish

variables:
  DEVKIT_CACHE_DIR: '$CI_PROJECT_DIR/.devkit/cache/ci'
  GITLAB_NPM_TOKEN: '$CI_JOB_TOKEN'

cache:
  key:
    files:
      - .devkit/manifest.json
      - pnpm-lock.yaml
      # Add Cargo.lock for repositories with Rust components
  paths:
    - .devkit/cache/

verify:
  stage: verify
  tags:
    - linux-x86_64-docker
  before_script:
    # Optional: write a custom root CA PEM if GitLab uses an internal PKI
    - |
      if [ -n "${ENTERPRISE_CA_PEM:-}" ]; then
        printf '%s\n' "$ENTERPRISE_CA_PEM" > /tmp/enterprise-ca.pem
        export DEVKIT_CA_CERT=/tmp/enterprise-ca.pem
      fi
  script:
    - make setup
    - make check
    - make test
    - make build
    # - make package  # Uncomment when packaging an Electron app or a "private": false library
  artifacts:
    paths:
      - packages/*/release/
      - apps/*/release/

publish_packages:
  stage: publish
  rules:
    - if: '$CI_COMMIT_TAG =~ /^v[0-9]+\.[0-9]+\.[0-9]+$/'
  needs: [verify]
  before_script:
    - |
      if [ -n "${ENTERPRISE_CA_PEM:-}" ]; then
        printf '%s\n' "$ENTERPRISE_CA_PEM" > /tmp/enterprise-ca.pem
        export DEVKIT_CA_CERT=/tmp/enterprise-ca.pem
      fi
  script:
    - |
      for receipt in packages/*/release/package-receipt.json; do
        [ -f "$receipt" ] || continue
        pkg_dir="$(dirname "$(dirname "$receipt")")"
        pkg_slug="$(basename "$pkg_dir")"
        filename="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["filename"])' "$receipt")"
        make exec package="$pkg_slug" network=bridge prepare=1 \
          command="npm publish ./release/$filename"
      done
```

## Airgapped / Offline Runners

To run lifecycle commands (`init`, `update`, `sync`) without public internet access:

1. Pre-stage the verified release directory (`devkit-<version>.tar.gz` extracted to `/opt/devkit/releases/<version>`), pre-load the pinned container images listed in `.devkit/runtime/toolchains.env` (and the digest-pinned Rust base image in `.devkit/runtime/rust.Dockerfile` for Rust projects) into the runner's Docker daemon, and populate the Cargo cache under `DEVKIT_CACHE_DIR/cargo` when offline Cargo resolution is needed.
2. Prime the local cache once using `--release-root`:
   ```sh
   /opt/devkit/releases/0.6.0/bin/devkit sync . \
     --release-root /opt/devkit/releases/0.6.0 \
     --cache-dir /var/cache/devkit
   ```
   Use the version accepted by the checkout. Headless projects on `0.6.0+` require the checkout loader or an external CLI at `0.6.0+`.
3. Run subsequent lifecycle commands with `--offline --cache-dir /var/cache/devkit`, and point `.npmrc` to your internal npm mirror.

## Containerized and Docker-in-Docker Runners

Devkit automatically detects cgroup v2 CPU and memory quotas on containerized CI runners so compilers, bundlers, and Vitest worker pools stay within the job's real slice even when `nproc` reports the full physical host:

- **Preflight diagnostics**: Run `sh .devkit/runtime/devkit.sh cli doctor . --json` in CI setup jobs to inspect Docker Compose compatibility, cgroup v2 controller delegation, host overcommit ratios, free disk/inodes, bridge MTU mismatches, and storage drivers (`overlay2` vs `vfs`).
- **Explicit resource overrides**: Export `DEVKIT_CPUS` (for example `DEVKIT_CPUS=2`), `DEVKIT_MEMORY_LIMIT` (for example `DEVKIT_MEMORY_LIMIT=4g`), and `DEVKIT_CARGO_BUILD_JOBS` (for example `DEVKIT_CARGO_BUILD_JOBS=2`) when sharing a runner across concurrent jobs.
- **Nested cgroup v2 delegation**: If a Docker-in-Docker container keeps entrypoint processes in its root cgroup (`/sys/fs/cgroup/cgroup.procs`), Linux cgroup v2 blocks `+cpu +memory` in `cgroup.subtree_control`. Devkit automatically falls back to in-process toolchain bounds without failing the job; to enable hard container cgroup limits in custom DinD images, move entrypoint processes into a child cgroup (such as `/sys/fs/cgroup/init`) before enabling `+cpu +memory +pids` on `cgroup.subtree_control`.

## Devkit release verification on Apple Silicon

While Apple Silicon remains behind the publication gate, the `build-darwin-arm64` job (`macos-15`) and the `darwin-preview` workflow (assembling and uploading `devkit-darwin-preview-<attempt>` on `ubuntu-24.04`, then verifying it on `macos-15`) run in parallel with the required Linux assembly and verification checks without blocking Linux PR checks or Linux-only releases. They exercise the native CLI and the stock shell and Make entry points. The Docker gate runs the same consumer commit on Linux x86-64, Linux ARM64, and Apple Silicon with a local Linux Docker VM. All three hosts need the Devkit repository at the candidate source commit, Git, Make, Python 3.12 or newer, and local Docker with Compose 5 or newer. The Mac also needs Worktrunk 0.74.0.

On Linux x86-64, take the run ID, three-target preview artifact name, and preview index SHA-256 from the candidate CI Actions run and its `darwin-preview` assembly summary. Set `run_id`, `preview_artifact`, and `preview_sha256` accordingly. From the candidate checkout, generate a fullstack web project, run its gates, and send the preview, Git bundle, and x86-64 receipt to the ARM64 runner at `linux_arm_host`.

```sh
handoff="$PWD/.artifacts/devkit-preview-handoff"
mkdir -p "$handoff/preview"
gh run download "$run_id" --repo docouno/devkit \
  --name "$preview_artifact" --dir "$handoff/preview"
python3 test/packaged/darwin_vm.py \
  --build-set "$handoff/preview" \
  --expected-index-sha256 "$preview_sha256" \
  --export-checkout-bundle "$handoff/checkout.bundle"
scp -r "$handoff" "$linux_arm_host:."
```

On Linux ARM64, set `preview_sha256` from the same assembly summary. Run from the candidate checkout and copy the handoff, now including both Linux receipts, to the Mac at `mac_host`.

```sh
handoff="$HOME/devkit-preview-handoff"
python3 test/packaged/darwin_vm.py \
  --build-set "$handoff/preview" \
  --expected-index-sha256 "$preview_sha256" \
  --checkout-bundle "$handoff/checkout.bundle"
scp -r "$handoff" "$mac_host:."
```

On the Mac, set `preview_sha256` from the same assembly summary. Run from the candidate checkout using the name of the local VM's Docker context. Each host verifies the frozen assets and the extracted manifest and engines before executing the preview.

```sh
python3 test/packaged/darwin_vm.py \
  --build-set "$HOME/devkit-preview-handoff/preview" \
  --expected-index-sha256 "$preview_sha256" \
  --checkout-bundle "$HOME/devkit-preview-handoff/checkout.bundle" \
  --docker-context desktop-linux
```

Keep the two `checkout.bundle.<target>.json` receipts beside the bundle and the Mac's `darwin-vm.json` receipt printed under `.artifacts/packaged`. The Mac gate verifies that both Linux receipts identify the same preview index, release identity, and consumer commit as its own run. Publication of the Darwin binary requires this gate to pass on the candidate bytes.
