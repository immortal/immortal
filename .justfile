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

install-check:
    sh -n scripts/rehearse-upgrade scripts/soak scripts/summarize-lifecycle-benchmarks contrib/freebsd/immortaldir
    scripts/rehearse-upgrade
    awk -f scripts/summarize-lifecycle-benchmarks.awk /dev/null > /dev/null

audit:
    cargo audit

deny:
    cargo deny --all-features check

lint-policy:
    @matches="$(rg -n '#!?\[(allow|expect)\(' crates --glob '*.rs' --glob '!**/tests/**' || true)"; \
        if [[ -n "$matches" ]]; then \
            printf '%s\n' "$matches"; \
            echo "production lint exceptions are forbidden; refactor the code or keep a narrow exception in crates/*/tests/" >&2; \
            exit 1; \
        fi

ci: lint-policy fmt-check clippy check test audit deny install-check

build:
    cargo build --workspace --release --locked

help-immortal:
    cargo run --quiet -p immortal -- --help

help-immortalctl:
    cargo run --quiet -p immortalctl -- --help

help-immortaldir:
    cargo run --quiet -p immortaldir -- --help
