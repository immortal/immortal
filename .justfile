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

soak iterations="10":
    scripts/soak "{{ iterations }}"

audit:
    cargo audit

deny:
    cargo deny check

lint-policy:
    @matches="$(rg -n '#!?\[(allow|expect)\(' crates --glob '*.rs' --glob '!**/tests/**' || true)"; \
        if [[ -n "$matches" ]]; then \
            printf '%s\n' "$matches"; \
            echo "production lint exceptions are forbidden; refactor the code or keep a narrow exception in crates/*/tests/" >&2; \
            exit 1; \
        fi

ci: lint-policy fmt-check clippy check test audit deny

build:
    cargo build --workspace --release --locked

help-immortal:
    cargo run --quiet -p immortal -- --help

help-immortalctl:
    cargo run --quiet -p immortalctl -- --help

help-immortaldir:
    cargo run --quiet -p immortaldir -- --help
