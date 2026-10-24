# Performance gate

Run after building the frozen release assets and the image from
`tooling/native.Dockerfile`:

```sh
DEVKIT_PERFORMANCE_TRACE_IMAGE=sha256:IMAGE_ID \
  python3 test/performance/verify.py \
    --build-set .artifacts/release/build-set \
    --output .artifacts/performance.json
```

The image must already exist locally. The harness uses its Cargo to generate
real fixture locks, and extracts its `strace` and dynamic libraries into a
private temporary directory. It does not install a host SDK or change global
configuration. `--baseline-release` can point to the exact public 0.2.1 archive
or extracted payload; otherwise the immutable GitHub archive is downloaded and
checked against its pinned digest.

Both actual release factories create independent products outside the checkout.
Their Rust code and Cargo-generated locks are identical: 1, 5, and 20 components,
plus a product with 80 local transitive crates. Real setup, format, Clippy, tests,
and initial builds must pass before measurement. These small std-only products
expose controller overhead without making claims about application compiler
speed. They complement the fullstack and native admission suites.

Warm measurements start fresh commands, alternate old/new order, and retain at
least 30 samples after two warmups. The receipt contains raw elapsed samples,
median, nearest-rank p95, min/max, exact release-index/source identities, host and
tool identities, paired comparisons, and input bytes/mode preservation. Native
doctor, status, and aggregate builds must improve on their matched baseline
medians. Help is reported without an improvement threshold.

Docker argv and host exec syscalls are traced in separate runs. Instrumented
timings never enter latency samples. Docker counts distinguish metadata control
containers and actual Rust tools; host exec counts exclude processes created by
the Docker daemon. The trace also reports time inside Rust tool container
invocations separately from the remaining host orchestration. Native metadata
must start no containers, and describe must invoke no Docker CLI.

The output has `success: false` until all required shapes, measurements, traces,
and stale-lock rejection checks complete. A failure retains partial measurements,
command logs, and temporary products for diagnosis. The harness does not prune
Docker resources or caches, drop OS caches, or edit release inputs.

This gate measures prepared warm execution. Actual cold public npm/curl
acquisition is measured by the public release verification and must be checked
separately before promotion. The receipt states this scope explicitly.

Fast failure-oriented harness checks need only Python:

```sh
python3 -m unittest discover -s test/performance -p 'test_*.py'
```
