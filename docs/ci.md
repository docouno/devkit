# Continuous Integration

Devkit projects require only Linux `x86_64`, Docker Engine with Docker Compose `5.0.0` or newer, and GNU Make on CI runners — no host Node.js or Rust toolchains are required to run `make setup`, `make check`, `make test`, `make build`, or `make package`.

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
    env:
      DEVKIT_CACHE_DIR: ${{ runner.temp }}/devkit-cache
    steps:
      - uses: actions/checkout@v4

      - name: Ensure Docker Compose v5+
        run: |
          docker compose version

      - name: Cache Devkit engine and project caches
        uses: actions/cache@v4
        with:
          path: |
            ${{ runner.temp }}/devkit-cache
            .devkit/cache
          key: devkit-${{ runner.os }}-${{ hashFiles('.devkit/manifest.json', 'package-lock.json', '**/Cargo.lock') }}
          restore-keys: |
            devkit-${{ runner.os }}-

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
          make package

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
  DEVKIT_CACHE_DIR: '$CI_PROJECT_DIR/.ci-devkit-cache'
  GITLAB_NPM_TOKEN: '$CI_JOB_TOKEN'

cache:
  key:
    files:
      - .devkit/manifest.json
      - package-lock.json
  paths:
    - .ci-devkit-cache/
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
    - make package
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
        archive="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["archive"])' "$receipt")"
        .devkit/runtime/devkit.sh exec --component "$pkg_dir" --network bridge -- \
          npm publish "release/$archive"
      done
```

## Airgapped / Offline Runners

To run lifecycle commands (`init`, `update`, `sync`) without public internet access:

1. Pre-stage the verified release directory (`devkit-<version>.tar.gz` extracted to `/opt/devkit/releases/<version>`) and pre-load the pinned container images listed in `.devkit/runtime/toolchains.env` into the runner's Docker daemon (or mirror them carefully by digest).
2. Prime the local cache once using `--release-root`:
   ```sh
   /opt/devkit/releases/0.4.0/devkit sync . \
     --release-root /opt/devkit/releases/0.4.0 \
     --cache-dir /var/cache/devkit
   ```
3. Run subsequent lifecycle commands with `--offline --cache-dir /var/cache/devkit`, and point `.npmrc` to your internal npm mirror.
