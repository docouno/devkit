# Author toolchain. Generated Rust components use runtime/rust.Dockerfile.
FROM rust:1.99.1-slim-bookworm@sha256:86234e857a413004348b471d11a4ebe1aa92d2eae2b6b143cac68fd1ac3c48be

RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        binutils ca-certificates file git musl-tools strace \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add --toolchain 1.99.1 x86_64-unknown-linux-musl \
    && rustup component add --toolchain 1.99.1 rustfmt clippy

ENV CARGO_BUILD_JOBS=1 \
    RUSTUP_AUTO_INSTALL=0
