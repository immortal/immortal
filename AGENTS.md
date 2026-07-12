# Agent guidance

These rules are mandatory for contributors and coding agents. Their purpose is
to preserve process-safety invariants, keep public contracts deliberate, and
ensure every change remains reviewable.

## Agent contract

- Follow this file strictly. If a request conflicts with it, explain the
  conflict and propose a compliant alternative.
- Keep diffs focused. Do not rename, reorder, refactor, or clean up unrelated
  code.
- Do not weaken validation, ownership checks, protocol bounds, cleanup, signal,
  or descriptor-safety guarantees.
- Do not hardcode runtime policy in entry points. Define CLI inputs in
  `commands`, convert them to typed values in `dispatch`, and validate them at
  the owning `immortal-core` boundary.
- When an invariant is unclear, inspect its contracts and documentation before
  changing behavior; do not silently guess across a process or trust boundary.

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

## Documentation requirements

Documentation is part of the implementation. A behavioral or architectural
change without synchronized documentation is incomplete.

- Write concise narrative documentation; avoid repetitive checklist labels on
  every item.
- Module documentation (`//!`) must explain the module's end-to-end role, why
  the design exists, and its important ownership, safety, and failure
  invariants. Protocols and multi-step lifecycle modules must include a short
  flow overview.
- Item documentation (`///`) should normally be one to five lines and focus on
  non-obvious behavior, side effects, invariants, and ownership.
- Add detailed item documentation for process creation and daemonization,
  descriptor and signal ownership, protocol state machines, lifecycle
  transitions, parsing and validation precedence, peer authorization, and
  platform-specific assumptions.
- Public functions returning `Result` must document their failure contract;
  functions with safety, cleanup, or authorization effects must state them.
- Do not duplicate module-level rationale on every item.
- Keep `README.md`, `DESIGN.md`, configuration examples, and implementation
  checklists synchronized with implemented behavior.
- Document and test every public CLI, configuration, and protocol field.

## Coding style and naming conventions

- Use Rust 2024 and let `rustfmt` determine layout.
- Clippy `all` and `pedantic` are denied. Warnings, unsafe code, `unwrap`,
  `expect`, panics, and unchecked indexing are denied by workspace policy.
- Production code must not contain `#[allow(...)]`, `#![allow(...)]`,
  `#[expect(...)]`, or any other local lint weakening. Refactor the design so it
  satisfies the workspace policy.
- Narrow item-level lint exceptions are permitted only in standalone files
  under `crates/*/tests/`, when the test shape genuinely requires one. Never
  weaken a lint for an entire test crate when a smaller scope works.
- File and module names use `snake_case`, types use `UpperCamelCase`, functions
  and variables use `snake_case`, and constants use `SCREAMING_SNAKE_CASE`.
- Keep functions focused. Group cohesive mutable lifecycle state into explicit
  structs rather than passing long loose parameter lists or maps.
- Prefer typed errors and `?` over sentinel values or lossy string errors.
- Group imports by root: standard library, external crates, then local `crate`
  or `super` modules, with a blank line between groups.
- When importing more than one path from the same crate, use one nested import
  tree such as `use immortal_core::{config::parse_str, control::{...}};` instead
  of repeating `use immortal_core::...` statements. Separate trees are allowed
  when they have different `cfg` conditions.
- Keep grouped entries in a predictable lexical order where practical and let
  `rustfmt` determine the final layout.
- Do not add braces around a single import merely for visual symmetry.

## Zero tolerance for panics

Production code must remain correct under expected absence, malformed input,
operating-system races, and resource failures. Returning a typed error,
isolating one invalid service, or making an explicit state transition is
required; aborting the supervisor is not.

All production paths must handle, as applicable:

- missing, removed, empty, or replaced configuration and runtime entries;
- malformed, truncated, oversized, unknown-version, and type-mismatched input;
- empty definition scans, command arguments, environment maps, and control
  results;
- child exit during startup, disappearing or reused process identifiers, and
  stale lifecycle generations;
- closed or invalid descriptors, partial reads and writes, broken pipes, and
  peer disconnects;
- permission errors, resource exhaustion, timeouts, and interrupted system
  calls;
- zero durations, counters at their bounds, and arithmetic which could
  overflow, underflow, or divide by zero.

Do not use `unwrap`, `expect`, `panic!`, unchecked indexing or slicing, or an
`unreachable!` assertion to avoid modeling one of these cases. Tests must cover
the relevant failure path whenever behavior is added or changed.

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

- Keep every workspace crate on `version.workspace = true`; Immortal binaries
  and `immortal-core` are released as one versioned product.
- Generate source revision metadata once in `immortal-core` and use its shared
  long-version helper in every executable.
- Declare shared versions in the root `Cargo.toml`.
- Consume shared dependencies with `dependency.workspace = true`.
- Add dependencies only for behavior currently being implemented.
- Prefer focused, maintained crates with compatible licenses.
- Update `Cargo.lock` whenever dependencies change.
- Dependencies must pass `cargo audit` and `cargo deny check`.

## Testing guidelines

- Keep focused unit tests beside their module in `#[cfg(test)]` modules. Use
  standalone integration or harness-free contract executables for process,
  daemon, CLI, and cross-component behavior.
- Name tests `<unit>_<behavior>`, for example
  `startup_handshake_rejects_truncated_record`.
- Every behavior change requires success and failure coverage. Every bug fix
  requires a regression test which fails without the fix.
- Configuration and protocol tests must cover fields, defaults, bounds,
  malformed input, unknown values, truncation, and unsupported versions.
- Process tests must have hard deadlines and cleanup guards which terminate and
  reap every child and owned process group on success and failure.
- Prefer event synchronization or bounded polling to fixed sleeps. Any required
  delay must be bounded and explain what race it closes.
- Tests must not depend on execution order, shared global child ownership, or
  residual files and processes from another test.
- Test public behavior through public APIs where practical; use private unit
  tests for internal invariants and codecs.
- Never leave background processes, sockets, PID files, runtime directories, or
  temporary definitions after a test.

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
