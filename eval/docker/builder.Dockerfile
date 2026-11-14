# ch-eval-builder: builds the static `ch` that eval containers run. `ch-eval build` builds
# this image, then runs it with the source mounted read-only and the cargo registry and
# target directory in named volumes.
FROM rust:1.99-slim-trixie

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add "$(uname -m)-unknown-linux-musl"

# The workspace's rust-toolchain.toml asks for `stable` with clippy and rustfmt. Use the
# toolchain installed here instead, so a build doesn't download another one.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
