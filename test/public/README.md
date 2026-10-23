# Public release verification

```sh
bash test/verify-public.sh --version 0.2.0 --next-version 0.2.1
# or:
make verify-public from_version=0.2.0 version=0.2.1
```

The wrapper downloads immutable release archives and checksums from the real
GitHub HTTPS endpoint, plus actual npm version metadata and package bytes. npm
SHA512 must match both registry metadata and `release.json`. Public downloads use
no authentication and have no local archive or registry-fixture fallback.

It then runs the existing independent consumer matrix in registry mode. Every
init/add/update/recover command uses the downloaded public bootstrap. For the
canonical repository, no base-URL flag or environment override is supplied; this
checks the published default. Node/web/Rust consumers begin at the first version,
the fullstack web/Electron/Rust product uses the successor, and an edited Node
project accepts that successor through a real update.

The matrix includes native gates/expected failures, read-only inputs, source
purity, shared state-preserving HMR, production web, extracted packaged Electron
ELF/ASAR, real Rust HTTP, add-app/add-package/adoption preservation, conflicts, no-op and synthetic
journal recovery. Original schema-1 archives are not public releases and remain
a separate local compatibility test.

Python/curl and Linux Docker Compose are author requirements. Host language SDKs
are blocked in consumer operations; publisher credentials are not mounted or
forwarded. Evidence goes to `.artifacts/public/<run-id>` and the linked
`.artifacts/packaged/<run-id>` receipt, including URLs, hashes, per-project
versions and exact cleanup. Successful scratch directories are removed. Failed
work trees remain for diagnosis; there is no global Docker prune.
