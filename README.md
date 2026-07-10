# immortal

Immortal is being rebuilt in Rust as a small, Unix-focused process supervisor.
This branch is a clean foundation for that rewrite; it does not yet supervise
processes and is not a replacement for the released Go implementation.

The Go project remains available on the `master` and `develop` branches while
the Rust design evolves independently.

## Workspace

The repository contains one shared library and three command-line applications:

- `immortal-core`: process-supervision domain boundaries.
- `immortal`: supervise one process.
- `immortalctl`: inspect and control supervisors.
- `immortaldir`: reconcile service definitions from a directory.

Each executable follows the same CLI flow:

```text
commands -> dispatch -> actions -> start -> main
```

The three parsers use the Go commands as requirements inventories while providing
clearer, typed interfaces through Clap. Breaking changes are allowed when they
improve correctness, safety, or operability. Process supervision, control, and
directory reconciliation are not yet implemented. See [`DESIGN.md`](DESIGN.md)
for the intended boundaries and development rules.

## Build and test

Rust stable is required.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
cargo fmt --all -- --check
```

The same checks are available through `just`:

```sh
just ci
```

## Try the command shells

```sh
cargo run -p immortal -- --help
cargo run -p immortalctl -- --help
cargo run -p immortaldir -- --help
```

## DevPod

The development container is a single image-based Rust environment for DevPod
and local rootless Podman. It does not require a Compose stack.

```sh
scripts/dev-up
scripts/dev-ssh
just ci
```

The local configuration forwards Git identity, optional SSH signing through
the 1Password agent, and optional chezmoi dotfiles. More details are in
[`.devcontainer/README.md`](.devcontainer/README.md).

## Status

This skeleton does not parse configuration yet, so YAML compatibility is not
implemented or claimed. The existing Go `.yml` format is the baseline for the
future Rust parser: its examples will become compatibility fixtures before the
format is changed or extended. Runtime paths, control protocols, logging, and
process lifecycle semantics will likewise be designed and tested feature by
feature.
