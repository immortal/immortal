set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --all-features

check:
    cargo check --workspace --all-targets --all-features

test:
    cargo test --workspace

audit:
    cargo audit

deny:
    cargo deny check

ci: fmt-check clippy check test audit deny

build:
    cargo build --workspace --release --locked

help-immortal:
    cargo run --quiet -p immortal -- --help

help-immortalctl:
    cargo run --quiet -p immortalctl -- --help

help-immortaldir:
    cargo run --quiet -p immortaldir -- --help
