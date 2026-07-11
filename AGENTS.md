# Agent guidance

## Project direction

Immortal is being rebuilt from Go to Rust on the `rust` branch.

- Work only on the Rust implementation unless explicitly instructed otherwise.
- Treat `master` and `develop` as read-only references for the Go implementation.
- Never modify, merge, rebase, delete, or force-push `master` or `develop`.
- Do not restore old Go code or the previous Rust prototype into this branch.
- Do not claim production readiness or compatibility without passing contract tests.

## Workspace architecture

The Cargo workspace contains:

- `immortal-core`: reusable supervision and operating-system behavior.
- `immortal`: supervise one process.
- `immortalctl`: inspect and control supervisors.
- `immortaldir`: reconcile service definitions from a directory.
- `immortallog`: minimal replaceable file-logging adapter.

Keep process management, configuration, control protocols, logging, supervision,
and platform behavior in `immortal-core`.

CLI crates follow this one-way flow:

```text
commands -> dispatch -> actions -> start -> main
```

- `commands` defines Clap syntax and help text only.
- `dispatch` converts CLI matches into typed actions.
- `actions` coordinates application operations.
- `start` initializes diagnostics and runs dispatch.
- `main` remains a thin process entry point.

Do not put operating-system or supervision logic in CLI modules.

## Rust style

- Group imports by root: standard library, external crates, then local `crate`
  or `super` modules, with a blank line between groups.
- When importing more than one path from the same crate, use one nested import
  tree such as `use immortal_core::{config::parse_str, control::{...}};` instead
  of repeating `use immortal_core::...` statements. Separate trees are allowed
  when they have different `cfg` conditions.
- Keep grouped entries in a predictable lexical order where practical and let
  `rustfmt` determine the final layout.
- Do not add braces around a single import merely for visual symmetry.

## Process and daemon safety

The `fork` crate from `immortal/fork` is the canonical implementation for Unix
forking, daemonization, sessions, and child waiting.

- Access `fork` through `immortal-core::process` or its platform boundary.
- Do not call `libc::fork`, `setsid`, or `waitpid` directly elsewhere.
- Do not introduce another daemonization or fork library without approval.
- Treat process groups, PID reuse, signal delivery, descriptor inheritance,
  shutdown ordering, and child reaping as correctness requirements.
- Model process lifecycles with explicit states and transitions.
- Process tests must use timeouts and clean up every child and process group on
  success and failure paths.
- Never leave background processes running after tests.
- Unsafe code is forbidden by default. If it becomes unavoidable, document its
  invariants and request review before adding it.

## Go implementation as a reference

The Go implementation is a requirements and historical reference, not a design
constraint or compatibility target. Breaking CLI, configuration, and protocol
changes are allowed when they materially improve correctness, safety, clarity,
or operability.

- Inspect old behavior with `git show master:path/to/file`; keep the Go branches
  unchanged.
- Convert relevant Go behavior into Rust requirements or contract tests where it
  remains applicable.
- Accept only the strict `version: 2` configuration schema. Do not restore an
  unversioned Go compatibility or runtime migration parser.
- Test fields, defaults, environment handling, invalid input, and unsupported
  version rejection.
- Make breaking changes explicit and document their rationale and upgrade path.
- Do not claim compatibility unless the corresponding contract suite passes.
- Document and test every new configuration field.

## Platform support

The initial supported platforms are Linux, macOS, and FreeBSD.

- Keep platform-specific code under `immortal-core/src/platform`.
- Keep shared behavior outside platform modules and use narrow `cfg` gates.
- Do not add incomplete fallbacks that make unsupported platforms appear supported.
- Process or platform changes must compile for `x86_64-unknown-freebsd`.

## Dependencies

- Declare shared versions in the root `Cargo.toml`.
- Consume shared dependencies with `dependency.workspace = true`.
- Add dependencies only for behavior currently being implemented.
- Prefer focused, maintained crates with compatible licenses.
- Update `Cargo.lock` whenever dependencies change.
- Dependencies must pass `cargo audit` and `cargo deny check`.

## Required validation

Run project commands inside DevPod. Host-side commands are limited to DevPod
lifecycle commands and Git operations.

Before considering a change complete, run from the host:

```sh
scripts/dev-up
scripts/dev-ssh just ci
scripts/dev-ssh cargo check --workspace --target x86_64-unknown-freebsd --locked
```

For DevPod configuration or provisioning changes, recreate the environment first:

```sh
scripts/dev-up --recreate
scripts/dev-ssh just ci
```

Process, supervisor, control-protocol, and configuration changes require tests
for both successful and failing behavior.

## Repository safety

- Preserve user changes already present in the worktree.
- Do not use destructive Git commands or broad deletion without explicit approval.
- Do not commit, amend, push, force-push, or rewrite history without explicit
  authorization.
- Use concise, imperative, representative commit subjects without category
  prefixes such as `chore:` unless explicitly requested.
- Do not change public CLI, configuration, or protocol contracts incidentally.
- Keep documentation synchronized with implemented behavior.
