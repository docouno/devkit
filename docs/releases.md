# Releases and updates

## Prepare locally

`make pack` builds `.artifacts/releases/0.2.1`, `.artifacts/devkit-0.2.1.tar.gz`, `.artifacts/devkit-0.2.1.sh` and checksum sidecars. `make pack version=X.Y.Z` builds another preview without changing source versions. `make pack-next` also prepares the next test version. The unpacked archive contains:

```text
devkit                       # host Bash launcher
compose.factory.yaml
cli/                         # compiled Node factory, with bundled parser dependencies
runtime/                     # Make/Compose/native helpers, recipes and notices
templates/                   # app/library starters and shared-client variants
npm/docouno-env-X.Y.Z.tgz
release.json                 # version, package integrity, payload hashes and modes
LICENSE
```

The npm archive contains public exports, the verifier, compiler baselines, README and license. It excludes tests, caches, fixtures and private instructions. The CLI bundles its YAML/TOML parser dependencies; it does not look for an author's `node_modules`. Release-only registry/hydration tools are not consumer entrypoints.

The builder stages output, creates a real npm archive and resolves template locks against that archive. Incomplete historical locks acquire exact-version registry identities; selected product versions are preserved. Native npm then normalizes the result, and all registry entries must have explicit resolved URLs and integrity. The builder also requires all named runtime notices to be nonempty regular files and verifies payload identities before replacing the local preview. Different bytes with the same accepted version are rejected by consumers: published releases must be immutable.

## Remote protocol

The published bootstrap defaults to `https://github.com/docouno/devkit/releases/download`. An operator can explicitly override that trusted HTTPS release base for another distribution endpoint. For version `X.Y.Z`, the bootstrap requests:

```text
<base>/vX.Y.Z/devkit-X.Y.Z.tar.gz.sha256
<base>/vX.Y.Z/devkit-X.Y.Z.tar.gz
```

Each GitHub release serves the rendered `.sh` both under its versioned filename and as `<base>/vX.Y.Z/bootstrap.sh`; the two files have identical bytes. Published versions and their assets are immutable. The wrapper can also consume `--release-archive /path/devkit-X.Y.Z.tar.gz` with an adjacent checksum sidecar.

Init, update and recovery require `--version X.Y.Z`. Add-app, add-package and register read the project's accepted version; an explicit different version is refused. There is no implicit `latest`. The same native CLI is available from an unpacked release.

The bootstrap checks the download and validates bounded archive contents before invoking the release. Temporary acquisition files are removed. Redirects must remain HTTPS; explicit localhost HTTP is available for the test fixture. Checksums protect against corruption, not a compromised distribution source. The script itself and the release source must be trusted.

## Package sources

Local mode copies the real environment archive into `.devkit/npm` and records relative `file:` dependencies in Node components. Commit the archive and root native lock with the runtime. The consumer remains independent of the author checkout and test registry.

Registry mode uses the exact environment version and canonical npm lock identities. It is the default for released CLI init. The public npm package and its release carry matching integrity. Use `--package-source local` only when an explicit bundled-archive workflow is desired. Update retains the mode unless explicitly changed. Mode changes are reviewable manifest/lock changes.

The author registry fixture serves real package metadata/bytes for the tested environment versions and redirects public dependency requests to npm. It supports no publish or authentication API. Fresh-cache installs, export loading, native gates, bad integrity and missing versions are tested separately from local mode.

## Ownership and transactions

Schema 2 records an accepted release, generation, components, tooling profiles, native dependency edges and managed file identities. Template origin is provenance; current tooling profile controls future tool updates. The CLI never reapplies a template over existing product source.

Managed files are the runtime, tool profile table, generated Make/Compose integration, local environment archives and Rust toolchain pins. Named tooling dependency fields and the root npm workspace list are field-owned. Application scripts, configuration, product dependencies, root Make/Compose additions and environment values remain project-owned.

Init creates selected apps or a preset. Add creates one new component and updates the shared integration. Register adopts an existing component only when its native tooling meets the selected profile. Component paths are distinct from npm package names. Libraries expose native public interfaces; application dependencies on another registered application are refused.

Each mutation validates the old accepted state, stages the change, runs native lock resolution and compares the product-rooted dependency graph. An unrelated runtime dependency must not silently change because tooling changed. The complete native input closure is rechecked before commit. The root `npm-shrinkwrap.json` conflicts with this package-lock-based mode and is rejected rather than ignored.

The common checkout lock serializes runtime tasks and lifecycle operations. Managed edits, incompatible peers, missing inputs or conflicting Compose service names fail before applying. A write interruption leaves a journal with saved before/after identities; ordinary tasks refuse pending state. `devkit recover TARGET` restores known prior bytes and refuses unknown concurrent edits. It does not stash, reset, commit or create a PR.

## Migrating schema 1

Updating a 0.1.0 consumer converts registration to schema 2 and imports existing per-app npm locks into a native root workspace lock. Product dependency identities, application source and user root Make/Compose are preserved. Existing development services remain project-owned, so migration does not introduce a second generated service beneath them. Review this diff and run setup and all project gates before merging.

Some historical schema-1 locks omitted registry URLs or integrity. That transition fills only missing identities for the already selected exact versions, preserves existing identities and dependency topology, and hashes local archives from the staged input. The manifest records this normalization and its original lock/archive hashes. These are identities established during migration, not proof of which bytes an earlier installation used. A conflicting existing identity stops the update. Normal schema-2 operations retain strict identity checks and do not perform this legacy repair.

Template improvements are not automatic source migrations. Explicit future migrations may change managed integration; product-code refactors require a separate task.

## Publication procedure

Prepare and verify a release before assigning its public version. Publish the verified npm archive with public access, then expose the matching GitHub assets and checksums. Publishing requires the account’s npm authorization/2FA; these credentials belong outside the checkout and must not be passed into consumer tests. The repository does not install publisher credentials or publish during normal build/check commands.

The release includes the standalone tar/checksum, versioned bootstrap/checksum, `bootstrap.sh` alias/checksum, npm archive and `release.json`. Do not rebuild different bytes under an already published version. A correction requires a new patch release.

After publication, `make verify-public from_version=X.Y.Z version=A.B.C` downloads both actual releases over HTTPS, compares the npm distribution integrity with each release, then creates and updates real registry consumers through the public bootstrap. The canonical endpoint is exercised without a URL override. No local archive or temporary registry fallback is used. This verification is separate from archive-failure fixtures and publication authentication.

Installer signing, hosted publishing automation and support for additional operating systems remain separate work.
