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
reconciles current and desired state so correctness does not depend on a
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
- `reconcile`: stable definition/applied snapshots, checked supervisor launch,
  and desired-state planning.
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

The implemented initial broker channel never accepts a PID from the supervisor.
Spawn and signal requests carry the supervisor's monotonic generation, and the
broker resolves that generation against its live child and process-group table.
Frames preserve non-UTF-8 Unix argv, environment, and path bytes while enforcing
hard bounds on the frame, individual fields, collection counts, and startup
deadline. The broker reports readiness only after its current-thread runtime and
`SIGCHLD` source are installed. Malformed frames, unknown child ownership, and
unexpected EOF fail closed; supervisor EOF triggers group kill and reaping.
The supervisor owns one persistent broker-reader task feeding a bounded queue;
selecting between process events, control work, timers, and Unix signals can
therefore cancel a queue receive without cancelling a partially read frame.
The controlled foreground executor acquires exclusive runtime ownership before
the broker fork and binds its authenticated socket only after the Tokio runtime
exists. The supervisor event loop remains the sole lifecycle owner. Explicit
`Exit` sends a generation-bound detach request to the broker; only a successful
detach transitions the supervisor to `Exited`, after which the empty broker is
shut down and reaped while the service is deliberately reparented to the OS.

Pre-start conditions and post-exit hooks use broker task identities rather than
service generations, but retain the same bounded IPC, process-group isolation,
timeout, signal, and reaping guarantees. A condition completes before a service
generation is allocated and keeps independent backoff state. Once a service
generation is reaped, its terminal identity remains in `Completed` while the
post-exit hook runs. Only after hook cleanup does the event loop publish the
already selected Backoff, Down, Failed, or Exited transition. Hook failure does
not rewrite the service result. Supervisor shutdown cancels and reaps either
kind of auxiliary task before broker shutdown.

The post-exit command receives the resolved service execution context plus
`IMMORTAL_EXIT_KIND`, `IMMORTAL_EXIT_STATUS`, `IMMORTAL_GENERATION`,
`IMMORTAL_START_FAILED`, and `IMMORTAL_READINESS_FAILED`. These fields describe
the main generation, including failed exec and readiness paths; they do not
describe the hook's own outcome.

Logging pipelines are materialized before the broker starts. The single-threaded
broker owns stable CLOEXEC pipe masters and duplicates only the endpoints needed
by each service or logger spawn. The Tokio supervisor owns logger restart policy
and health but never reads or copies log bytes. Logger exec success gates the
first service generation. A logger crash leaves its pipe identity and buffered
bytes intact while independent exponential backoff runs. Each logger stage owns
its failure streak; the immutable configuration is borrowed only while handling
an event. Configured exhaustion before a service generation cancels childless
pre-start work and publishes `Failed`. Exhaustion after a service generation is
live changes logger health without silently stopping that service. Explicit
start operations reset failed logger stages. Final shutdown closes broker
writer masters, permits bounded EOF drain, then signals logger groups from
downstream to upstream before broker reaping.

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
- `fork` owns reusable Unix mechanisms, not supervision policy. Immortal owns
  broker IPC, generation identity, restart decisions, readiness, logger graphs,
  control, status, and reconciliation.
- Prefer safe standard-library interfaces and small focused dependencies.
- Do not add dependencies for hypothetical future behavior.
- Avoid panics in production paths; errors must retain actionable context.
- Keep parsing separate from side effects so behavior can be unit tested.
- Model supervision as explicit state transitions rather than shared flags.
- Treat process groups, locks, sockets, PID reuse, and shutdown ordering as
  correctness concerns, not implementation details.
- Filesystem notifications may accelerate reconciliation but never replace it.
- Public CLI, configuration, and protocol decisions require contract tests.
- The Rust rewrite accepts only its strict `version: 2` YAML schema. Unversioned
  Go definitions are requirements references, not accepted runtime input.
- The Go implementation is a reference for externally visible behavior, not a
  constraint on the Rust architecture or internal APIs.

## Incremental development

Remaining milestones are vertical and contract-tested:

1. Extend `immortal/fork` and prove the broker primitives on all three targets.
2. Run one foreground process group and report exec/exit/signal accurately.
3. Integrate readiness, hooks, logger chains, restart policy, and shutdown.
4. Run the authenticated control loop and populate complete typed status.
5. Apply `immortaldir` plans with bounded concurrency and idempotent recovery.
6. Pass native lifecycle jobs, benchmarks, fuzzing, and packaging gates before
   any production-readiness claim.

Packaging, release branches, and Go deprecation are deliberately outside the
skeleton milestone.

Milestone 2 is implemented for foreground and checked daemon launches. The CLI
builds or loads the strict service model and resolves account data before any
fork. Daemon mode performs checked double-fork detachment before the core
executor creates its broker and current-thread Tokio runtime; the original
invoker returns only after broker and optional control-listener readiness.
Immediate readiness, restart/backoff decisions, bounded retry termination,
successful exit, failed exec, atomic PID publication, and final broker reaping
have black-box contracts.

Milestone 4 is implemented for the controlled foreground path. Runtime locking,
stale-socket safety, peer-authenticated bounded serving, optimistic generation
matching, all lifecycle operations, raw signal delivery, live-child detach, and
complete status publication share the same serialized executor. Logger
Starting, Ready, Backoff, and Failed health is published from the same owner;
bounded exhaustion and explicit operator recovery are contract-tested.

The readiness and lifecycle-hook portions of milestone 3 are implemented. For each
`notify-fd` generation the broker creates a CLOEXEC socket pair, maps the child
endpoint to descriptor 3, publishes `IMMORTAL_READY_FD=3`, and monitors the
other endpoint on its current-thread Tokio runtime. Exact-token success,
invalid input, early close, and timeout are generation-bound broker events;
failure stops the group and feeds restart policy without treating a PID as
identity. Pre-start conditions have independent bounded retry policy. Post-exit
hooks run after reaping with typed exit, signal, generation, exec-failure, and
readiness-failure context; hook failure, timeout, and shutdown cleanup are
contract-tested. Broker-owned logger chains now cover external commands,
replaceable file adapters, stable pipes across logger failure, combined and
separate streams, multi-stage passthrough, status, EOF drain, and bounded
downstream-first shutdown.

The sequential portion of milestone 5 is operational. `immortaldir` creates
one checked launcher broker before Tokio, compares complete scans with live
authenticated supervisors, and persists normalized launch and applied-state
snapshots below the owner-only runtime root. It starts missing definitions,
preserves unchanged and operator-stopped supervisors, applies valid changes,
stops disabled services, and halts only confirmed deletions. Replacement waits
for both control-socket disappearance and advisory-lock release. An active lock
without a control socket is deferred and retried rather than guessed from PID
metadata; an exited enabled supervisor is recreated after confirmed lock
release. Typed failed mutations remain pending for later scans while independent
services continue in the current scan. Independent starts are submitted as
bounded broker task batches within deterministic dependency waves; later waves
wait for Ready, and later dependency failure does not cascade.
Deletion-confirmation state across `immortaldir` restarts remains pending.

Portable start conditions remain supervisor policy, not directory policy. A
checked daemon may publish `WaitingCondition` while its broker retries the
condition with independent backoff; `immortaldir` waits for Ready before
advancing dependent waves. The operational contract proves that failed
conditions leave the service start counter at zero and that later success
unblocks reconciliation.
