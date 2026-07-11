# Rust rewrite design

## Purpose

The current branch is a contract-first rebuild, not a production-ready partial
supervisor. Configuration, lifecycle policy, control, logging, status, and
reconciliation can be implemented and tested before the process executor, but
the binaries must fail closed until the complete fork-backed lifecycle passes
native contract tests.

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

`immortal` uses the Go options as a requirements inventory while adopting typed
Clap names, validation, value hints, and conventions. Parsing an option does not
imply its process behavior is implemented. New behavior arrives through typed
dispatch and actions with contract tests.

## Core boundaries

- `config`: typed service definitions, validation, and resolved defaults.
- `process`: wraps the `fork` crate for Unix child creation, daemonization,
  waiting, sessions, process groups, environment, identity, and signals.
- `supervisor`: desired state, retries, transitions, and shutdown coordination.
- `logging`: stdout/stderr routing, files, rotation, and external sinks.
- `control`: local protocol types plus client and server responsibilities.
- `status`: bounded transport-independent supervisor observations.
- `readiness`: the `IMMORTAL_READY_FD` token and deadline contract.
- `runtime`: safe runtime-root and supervisor discovery policy.
- `reconcile`: stable definition snapshots and desired-state planning.
- `watch`: native notification hints plus periodic reconciliation triggers.
- `platform`: the smallest possible Linux, macOS, and FreeBSD adaptations.

These modules are boundaries rather than finalized APIs. Types remain private
until more than one consumer needs them, and `immortal-core` stays unpublished
while the design is evolving.

## Process broker architecture

Calling `fork()` after Tokio or any other thread exists is not an acceptable
restart strategy. The child inherits process-wide locks and library state from
threads which no longer exist, and only async-signal-safe operations are valid
before `exec`. A dedicated single-threaded process broker avoids that hazard.

```text
invoking process
  -> checked daemonization and startup channel
      -> supervisor (still single-threaded)
          -> create bounded broker channel
          -> fork one process broker
          -> only now create the Tokio runtime

Tokio supervisor <---- bounded typed IPC ----> single-threaded broker
                                               -> service process groups
                                               -> logger chains
                                               -> readiness/exec-status pipes
                                               -> bounded hooks
                                               -> wait/reap events
```

The broker remains the direct parent and sole reaper of every managed child. It
assigns no durable meaning to a PID: every request and event includes the
supervisor's monotonic generation identity. The supervisor owns policy and
desired state; the broker owns operating-system process handles and performs
only requested mechanisms.

Before each broker fork, argv, environment, user/group transition, working
directory, descriptor actions, and process-group identity are fully validated
and materialized. The post-fork child performs only reviewed async-signal-safe
descriptor, credential, group, and `execve` operations. An exec-status channel
distinguishes exec failure from a successfully executed program before the
generation becomes Running.

Broker IPC is bounded, versioned, and private. EOF or supervisor death makes the
broker stop and reap owned groups before exiting. Broker death places the
supervisor in a terminal failure state; it must never spawn an untracked
replacement path. Daemon startup is reported to the invoking process only after
the broker, service lock, runtime directory, and control socket are ready.

### Required `immortal/fork` contract

The canonical sibling `fork` crate must provide safe, portable primitives for:

- typed nonblocking `Exited`, `Signalled`, `Stopped`, and `Continued` events,
  with child PID and `EINTR` handling;
- positive PID/process-group newtypes, `setpgid`, and explicit process versus
  process-group signal delivery without accepting ambiguous raw negative PIDs;
- CLOEXEC pipe/socket pairs, owned descriptors, reviewed `dup2`/close actions,
  and an explicit inherited-descriptor allow-list;
- checked double-fork/session setup which does not exit the invoking process
  before a bounded startup result is received;
- a precomputed exec specification whose post-fork path performs no allocation
  or non-async-signal-safe Rust cleanup.

Upstream acceptance tests must cover exit, signal, stop/continue, exec failure,
group cleanup, descriptor inheritance, startup failure, `EINTR`, and child
draining on Linux, macOS, and FreeBSD. Immortal will not reproduce these calls
with direct `libc` or add a second process library.

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

Remaining milestones are vertical and contract-tested:

1. Extend `immortal/fork` and prove the broker primitives on all three targets.
2. Run one foreground process group and report exec/exit/signal accurately.
3. Integrate readiness, hooks, logger chains, restart policy, and shutdown.
4. Run the authenticated control loop and populate complete typed status.
5. Apply `immortaldir` plans with bounded concurrency and idempotent recovery.
6. Pass Go migration contracts, native lifecycle jobs, benchmarks, fuzzing, and
   packaging gates before any compatibility or production-readiness claim.

Packaging, compatibility policy, release branches, and Go deprecation are
deliberately outside the skeleton milestone.
