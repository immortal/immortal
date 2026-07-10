# Rust rewrite design

## Purpose

The first milestone is a reliable project foundation, not a partial supervisor.
The workspace establishes ownership boundaries, development tooling, and a
consistent CLI architecture before any runtime contract becomes public.

The implementation should remain understandable without sacrificing operating
system correctness. New behavior must arrive with tests and documentation in the
same change.

## Executable responsibilities

### immortal

Owns the lifecycle of one supervised command. Future work belongs here when it
is specific to launching the supervisor from the command line; reusable process
and lifecycle behavior belongs in `immortal-core`.

### immortalctl

Discovers running supervisors, requests state changes, and formats status for a
human or another program. It must communicate through a documented core control
abstraction rather than reaching into supervisor state.

### immortaldir

Turns a directory of service definitions into a desired set of supervisors. It
will reconcile current and desired state so correctness does not depend on a
filesystem watcher delivering every event.

## CLI architecture

Every executable uses the same one-way flow inspired by the `cron-when` CLI
layout:

```text
main
  -> cli::start
      -> commands: define Clap commands and options
      -> dispatch: validate matches and construct a typed action
      -> actions: execute the selected application operation
```

The layers have strict responsibilities:

- `commands` contains only CLI syntax and help text.
- `dispatch` converts untyped matches into typed values and reports usage errors.
- `actions` coordinates application operations without embedding OS primitives.
- `start` initializes diagnostics and connects the layers.
- `main` translates the final result into process output and an exit status.

The initial skeleton stops after command parsing because there are no real
actions yet. `immortal` uses the Go options as a requirements inventory while
adopting idiomatic Clap names, validation, value hints, and conventions. Backward
compatibility is secondary to a safer and clearer interface. Parsing an option
does not imply its behavior is implemented. New behavior must arrive through
typed dispatch and actions with contract tests.

## Core boundaries

- `config`: typed service definitions, validation, and resolved defaults.
- `process`: wraps the `fork` crate for Unix child creation, daemonization,
  waiting, sessions, process groups, environment, identity, and signals.
- `supervisor`: desired state, retries, transitions, and shutdown coordination.
- `logging`: stdout/stderr routing, files, rotation, and external sinks.
- `control`: local protocol types plus client and server responsibilities.
- `platform`: the smallest possible Linux, macOS, and FreeBSD adaptations.

These modules are boundaries rather than finalized APIs. Types remain private
until more than one consumer needs them, and `immortal-core` stays unpublished
while the design is evolving.

## Engineering rules

- Linux, macOS, and FreeBSD are the only initial target systems.
- The immortal [`fork`](https://github.com/immortal/fork) crate is the canonical
  source of Unix fork, daemon, session, and wait primitives; direct use stays
  behind `immortal-core::process` and the platform boundary.
- Prefer safe standard-library interfaces and small focused dependencies.
- Do not add dependencies for hypothetical future behavior.
- Avoid panics in production paths; errors must retain actionable context.
- Keep parsing separate from side effects so behavior can be unit tested.
- Model supervision as explicit state transitions rather than shared flags.
- Treat process groups, locks, sockets, PID reuse, and shutdown ordering as
  correctness concerns, not implementation details.
- Filesystem notifications may accelerate reconciliation but never replace it.
- Public CLI, configuration, and protocol decisions require contract tests.
- Existing Go service files are the compatibility baseline for the Rust YAML
  schema; accepted fields, defaults, aliases, and errors require fixtures before
  compatibility can be claimed.
- The Go implementation is a reference for externally visible behavior, not a
  constraint on the Rust architecture or internal APIs.

## Incremental development

Future milestones should be vertical and independently useful:

1. Define and validate one minimal service configuration.
2. Run one foreground child and report its exit accurately.
3. Add an explicit supervisor state machine and controlled restart behavior.
4. Introduce local status/control communication and then `immortalctl` actions.
5. Add logging, process identity, PID following, and other advanced behavior.
6. Implement directory reconciliation and native platform verification.

Packaging, compatibility policy, release branches, and Go deprecation are
deliberately outside the skeleton milestone.
