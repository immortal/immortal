# Contributor and agent contract

These rules are mandatory for contributors and coding agents. Their purpose is
to preserve process-safety invariants, keep public contracts deliberate, and
ensure every change remains reviewable.

## Start here

Immortal is being rebuilt from Go to Rust on the `rust` branch.

- Follow this contract strictly. If a request conflicts with it, explain the
  conflict and propose a compliant alternative.
- Work only on the Rust implementation unless explicitly instructed otherwise.
- Treat `master` and `develop` as read-only references for the Go implementation.
- Never modify, merge, rebase, delete, or force-push `master` or `develop`.
- Do not restore old Go code or the previous Rust prototype into this branch.
- Keep diffs focused. Do not rename, reorder, refactor, or clean up unrelated
  code.
- Do not weaken validation, ownership checks, protocol bounds, cleanup, signal,
  or descriptor-safety guarantees.
- Do not hardcode runtime policy in executable entrypoints.
- When an invariant is unclear, inspect its contracts and documentation before
  changing behavior; never guess across a process or trust boundary.
- Do not claim production readiness or compatibility without the evidence
  required by `VALIDATION.md` and `RELEASE.md`.

## Documentation ownership

Update the document which owns the changed contract instead of repeating the
same detail everywhere.

| Document | Owns |
|---|---|
| `README.md` | Operator overview, supported configuration, logging, runtime, and CLI behavior |
| `INSTALL.md` | Installation, init integration, migration, and rollback |
| `DESIGN.md` | Architecture, ownership boundaries, and safety rationale |
| `TRACEABILITY.md` | Public inputs and outputs mapped to success and failure tests |
| `VALIDATION.md` | Correctness, resilience, performance, and platform evidence |
| `RELEASE.md` | Release-candidate gates and procedure |
| `AGENTS.md` | Mandatory contribution and implementation rules |

## Workspace architecture

The Cargo workspace contains:

- `immortal-core`: reusable supervision and operating-system behavior.
- `immortal`: supervise one process.
- `immortalctl`: inspect and control supervisors.
- `immortaldir`: reconcile service definitions from a directory.
- `immortallog`: minimal replaceable file-logging adapter.

Keep process management, configuration, control protocols, logging, supervision,
and platform behavior in `immortal-core`.

### File organization and module size

Keep Rust source files cohesive and generally under 1000 lines, and treat 1000
as a hard upper limit rather than a target. A file approaching it usually holds
more than one responsibility.

Split by responsibility, not by arbitrary size, and keep related types,
implementations, errors, and tests together. Name each module for what it owns,
such as `dispatch`, `errors`, `validation`, or `executor`; never create
dumping-ground modules such as `utils`, `helpers`, `common`, or `misc`.

When splitting a module, keep the original file as the module root and its
canonical public path; do not rename it to `mod.rs`. Declare each extracted part
as a private child module and re-export the exact prior surface through `self::`
so every public type keeps one canonical path:

- `mod errors;` holds production error types, re-exported with
  `pub use self::errors::Type;` only where the parent already exposed them.
- `#[cfg(test)] mod tests;` holds the file's unit tests.
- A narrowly named private child module holds genuinely shared internal helpers.

Keep child modules private (`mod`, not `pub mod`) unless an external caller needs
the submodule path, and give items shared between the parent and its tests the
narrowest visibility that works, normally `pub(super)`.

### CLI source layout

Every executable crate uses the `s3m`-inspired per-action layout. Preserve it
for new and changed CLI actions:

```text
crates/<name>/src/
|-- bin/
|   `-- <name>.rs
|-- lib.rs
`-- cli/
    |-- mod.rs
    |-- start.rs
    |-- commands/
    |   `-- mod.rs
    |-- dispatch/
    |   `-- mod.rs
    `-- actions/
        |-- mod.rs
        |-- <action>.rs
        `-- <shared_helper>.rs
```

The execution flow is:

```text
src/bin/<name>.rs
  -> cli::start
      -> commands
      -> dispatch
      -> actions::Action
  -> exhaustive Action match
      -> actions::<action>::execute
      -> immortal-core
  -> shared CLI completion and exit mapping
```

- `commands` defines Clap syntax, defaults, conflicts, and help text only.
- `dispatch` converts CLI matches into action-owned types. It must not execute
  operations or define a competing action contract.
- Keep `commands`, `dispatch`, and `start` modules private. Expose only
  `actions` and the deliberate startup/completion re-exports needed by the
  separate binary target.
- `actions/mod.rs` owns `Action`, shared action inputs, and shared typed errors.
  It declares focused action modules; do not hide binary routing in a central
  `actions::execute` match.
- Each user-visible operation has a `snake_case` action file whose public handler
  accepts typed values and coordinates only that operation. Put genuinely
  shared coordination in a narrowly named private module rather than duplicating
  it across handlers.
- `start` initializes diagnostics, parses arguments, runs dispatch, and returns
  the typed action. It does not execute the action.
- `src/bin/<name>.rs` makes the installed executable name explicit, calls
  `start`, exhaustively matches `Action`, delegates each variant to its action
  module, and translates the final result. The compiler must make a newly added
  variant fail to build until its route is wired.
- Keep the binary match declarative: no configuration parsing, runtime
  construction, process behavior, business logic, or duplicated validation.
- Keep executable entry points synchronous. Tokio runtimes belong in the action
  or core boundary after all required fork, daemon, broker, signal, and
  descriptor setup. Do not use `#[tokio::main]` where it can create a runtime
  before those boundaries.
- Preserve each executable's typed exit contract. A generic
  `main() -> anyhow::Result<()>` and `?` are not substitutes when they collapse
  usage, configuration, temporary, permission, I/O, and OS failures to status
  1; use shared startup reporting and completion helpers instead.

CLI action modules may coordinate calls into `immortal-core`, but process
management, configuration semantics, control protocols, logging, supervision,
and platform behavior remain implemented and validated in `immortal-core`.

Test dispatch independently from action handlers. Every new action needs
dispatch coverage, handler success and failure coverage, and a black-box CLI
contract proving that the named binary routes the variant and preserves its
exit status.

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
- Keep the owning documents above and maintained examples synchronized with
  implemented behavior.
- Document and test every public CLI, configuration, and protocol field.
- Start every maintained YAML document with `---` and keep it compliant with
  the repository yamllint policy.

## Coding style and naming conventions

- Use Rust 2024 and let `rustfmt` determine layout.
- When multiple styles remain valid after `rustfmt`, prefer the Rust API
  Guidelines, then conventions used by the standard library and `rust-lang`
  projects, and keep the choice consistent across the workspace.
- Write child-module re-exports through `self::`, for example
  `pub use self::start::start`; group multiple items and retain `rustfmt`'s
  version-sorted order.
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
- Use idiomatic stable Rust rather than translating Java, C++, or
  object-oriented ownership patterns. Prefer ownership and short lexical borrows
  over shared mutable state.
- Do not clone unless ownership must cross a boundary and the copy is justified.
  Do not box values or introduce `Arc<Mutex<T>>` by default; document the
  ownership or concurrency requirement when either is necessary.
- Keep every public type reachable through one canonical path. Internal modules
  may re-export an item while assembling that path, but users must not see
  duplicate paths such as both `crate::Type` and `crate::module::Type`.
- Before adding explicit lifetimes, shared ownership, locking, or nontrivial
  allocation, explain who owns each value, where it is borrowed, why the
  mechanism is required, and its allocation, copying, scheduling, memory, and
  complexity costs.
- Prefer small functions and strong domain types whose observable behavior can
  be tested independently. Tests must prove useful properties and failure
  behavior rather than restating implementation constants.
- Organize each Rust import section into three visual groups, omitting groups
  that are empty: standard library roots (`std`, `core`, and `alloc`), external
  and workspace crates, then local roots (`crate`, `self`, and `super`). Separate
  groups with exactly one blank line.
- Within each import group, sort declarations alphabetically, combine imports
  from the same crate into one nested declaration where that improves
  readability, and remove duplicates. Preserve aliases and attached comments,
  and retain `rustfmt`'s ordering within nested declarations.
- Import-only cleanups must not modify code outside the import section.
- If a module is used repeatedly, import the module and qualify uses
  consistently, for example `use std::env;` followed by `env::args_os()`. Do not
  mix that form with `std::env::args_os()` in the same module.
- Prefer module imports over individual functions when qualification makes
  ownership clearer. Combine imports from one root when that remains readable;
  do not force nested trees or braces around a single import.

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
- When an inline `#[cfg(test)]` module grows large enough to push a file toward
  the size limit, move it unchanged into a sibling `tests.rs` child module
  declared with `#[cfg(test)] mod tests;`, keeping tests beside the code they
  exercise.
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
