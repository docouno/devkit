# Publishing Libraries

Devkit `ts-library` and `react-ui` packages are created with `"private": true` so internal monorepo libraries never publish accidentally. Opting a library into publication requires only native `package.json` edits followed by `devkit sync` and `make package`.

## 1. Opt In to Publication

Edit `packages/<slug>/package.json`:

```json
{
  "name": "@acme/design-tokens",
  "version": "0.1.0",
  "private": false,
  "type": "module",
  "files": ["dist"],
  "exports": {
    ".": {
      "types": "./dist/index.d.ts",
      "import": "./dist/index.js"
    }
  },
  "publishConfig": {
    "access": "public"
  }
}
```

For `react-ui` libraries that emit `dist/styles.css`, keep the CSS subpath export as well:

```json
  "exports": {
    ".": {
      "types": "./dist/index.d.ts",
      "import": "./dist/index.js"
    },
    "./styles.css": "./dist/styles.css"
  }
```

### Publishable Manifest Contract

Devkit enforces the following rules when `"private": false` is set on a `ts-library` or `react-ui` component:

- `role` must be `library` with profile `ts-library` or `react-ui` (`apps/*` and `react-client` must stay private).
- `version` must be valid SemVer (for example `0.1.0` or `1.0.0-rc.1`).
- `files` must be a non-empty array including `"dist"` (or `"dist/..."`).
- `exports` must define `"."` with `"types"` listed **before** `"import"` (TypeScript module resolution requires `"types"` first in conditional export maps).
- Scoped packages (`@scope/pkg`) targeting the public npm registry (`https://registry.npmjs.org/`) without a custom registry in `publishConfig.registry` or `.npmrc` must set `"publishConfig": { "access": "public" | "restricted" }`.
- `dependencies`, `peerDependencies`, and `optionalDependencies` cannot use `file:` workspace links unless the referenced package is also `"private": false` and declared with a SemVer range.

After editing `packages/<slug>/package.json`, synchronize the workspace lockfile and projections:

```sh
npx @docouno/devkit@0.4.0 sync .
make setup
```

## 2. Bump Package Versions

Always bump versions inside the pinned Node container so both `packages/<slug>/package.json` and root `package-lock.json` stay consistent:

```sh
npx @docouno/devkit@0.4.0 exec --component packages/ui --write -- \
  npm version 0.2.0 --no-git-tag-version
make setup
```

## 3. Build and Validate Tarballs Locally (`make package`)

Run the full validation and packaging pipeline offline:

```sh
make check
make test
make package package=ui
```

`make package` runs `build/packages/ui` (`tsdown`) and then executes `devkit-pack` inside an isolated `--network none` container (`dist/` mounted read-only, `release/` writable):

1. Verifies `private === false` and all `exports` targets exist inside `dist/`.
2. Runs `npm pack --ignore-scripts` into a temporary staging directory.
3. Validates package structure and export conditions with `publint`.
4. Validates TypeScript declaration resolution (`node16` / `bundler` ESM) with `@arethetypeswrong/core`.
5. Publishes two deterministic outputs in `packages/<slug>/release/`:
   - `<safe-name>-<version>.tgz` (for example `acme-ui-0.2.0.tgz`)
   - `package-receipt.json` (`schemaVersion: 1`, `name`, `version`, `archive`, `sha256`, `integrity`, `size`, `files`, `publint`, `attw`)

If `"private": true` is still set, `make package package=<slug>` fails with an actionable hint explaining how to enable `"private": false` and run `devkit sync`.

## 4. Publish to Public npm (GitHub Actions OIDC Trusted Publishing)

Publish the verified `.tgz` artifact produced by `make package` with npm provenance — no long-lived npm token required when Trusted Publishing is configured on npmjs.com:

```yaml
jobs:
  publish:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@v4
      - name: Build and verify package archive
        run: |
          make setup
          make check
          make test
          make package package=ui
      - uses: actions/setup-node@v4
        with:
          node-version: '24'
          registry-url: 'https://registry.npmjs.org'
      - name: Publish verified tarball with provenance
        run: |
          TARBALL="$(jq -r '.archive' packages/ui/release/package-receipt.json)"
          npm publish "packages/ui/release/${TARBALL}" --provenance --access public
```

## 5. Publish to GitLab Package Registry (including Custom TLS CA)

1. Configure `publishConfig.registry` in `packages/ui/package.json` (or `@corp:registry` in root `.npmrc`):
   ```json
   "publishConfig": {
     "registry": "https://gitlab.example.com/api/v4/projects/42/packages/npm/"
   }
   ```
2. Configure root `.npmrc` with environment-variable token expansion:
   ```ini
   @corp:registry=https://gitlab.example.com/api/v4/projects/42/packages/npm/
   //gitlab.example.com/api/v4/projects/42/packages/npm/:_authToken=${GITLAB_NPM_TOKEN}
   //gitlab.example.com/api/v4/projects/42/packages/npm/:always-auth=true
   ```
3. Run `npx @docouno/devkit@0.4.0 sync .` and commit `package.json`, `.npmrc`, and `package-lock.json`.
4. In GitLab CI, map `CI_JOB_TOKEN` (or a Deploy Token) to `GITLAB_NPM_TOKEN`, pass `DEVKIT_CA_CERT` if your GitLab instance uses a private enterprise CA, build with `make package`, and publish the verified tarball:
   ```sh
   export GITLAB_NPM_TOKEN="${CI_JOB_TOKEN}"
   export DEVKIT_CA_CERT="/etc/ssl/certs/enterprise-root-ca.pem"
   make setup
   make check && make test && make package package=ui
   TARBALL="$(node -p "require('./packages/ui/release/package-receipt.json').archive")"
   npx @docouno/devkit@0.4.0 exec --component packages/ui --network bridge -- \
     npm publish "release/${TARBALL}"
   ```
