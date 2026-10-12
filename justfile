# Uses the rustup-managed toolchain from rust-toolchain.toml.
cargo := "rustup run stable cargo"

default: fmt lint test

fmt:
    {{cargo}} fmt --all

fmt-check:
    {{cargo}} fmt --all -- --check

lint:
    {{cargo}} clippy --workspace --all-targets -- -D warnings

test:
    {{cargo}} test --workspace

# Builds with the oldest supported Rust (`rust-version` in Cargo.toml), as CI does.
msrv:
    rustup toolchain install 1.88 --profile minimal
    rustup run 1.88 cargo check --workspace --all-targets --locked

run *ARGS:
    {{cargo}} run -p ch-cli -- {{ARGS}}
