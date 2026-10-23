# Rust API

This native Axum/Tokio application listens on container port 8080:

- GET /health returns 200 and {"status":"ok"}.
- GET /api/greeting?name=Ada returns {"message":"Hello, Ada!"}.
- Missing or blank names use world; surrounding whitespace is removed.

The process drains requests on SIGINT or SIGTERM. The first supported runtime is
Linux. Router tests run in process without opening a socket; a library doctest
also checks the greeting contract.

Use the project root Makefile for setup, check, test, build and fix. Setup fetches
the accepted dependencies. Ordinary Rust gates use --locked --offline and require
that cache; rerun setup if it is missing. Fix formats Rust sources; Clippy
suggestions are reviewed and applied separately. The root formatter configuration is
rustfmt.toml. Conflicting .rustfmt.toml files, unsupported formatter options and
globally disabling mandatory formatting are errors.

Rust test discovery uses standard Cargo libtest and rustdoc listings, subtracting
ignored cases before requiring a non-ignored native test case. This includes
compile-only doctests such as no_run: their code is compiled, but their bodies are
not executed. The policy does not guarantee a nonzero number of executed test
bodies. Custom harness=false executables are outside this discovery contract.
Dependencies whose own tests must run should
be members of the app's Cargo workspace or have a separately declared app gate;
a path dependency alone does not schedule its test suite.
