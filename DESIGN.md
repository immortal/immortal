# Rust rewrite design

## Purpose

The current branch is a contract-first rebuild and remains a release candidate,
not a production-ready supervisor. Configuration, lifecycle policy, process
execution, control, logging, status, and reconciliation are connected through
fork-backed native contracts. The binaries expose only behavior covered by
those contracts and fail closed for deliberately gated compatibility options;
production readiness still requires the external evidence in `RELEASE.md`.

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

The CLI layers use the following flow, inspired by the `cron-when` and `s3m`
layouts:

```text
src/bin/<name>.rs
  -> cli::start
      -> commands: define Clap commands and options
      -> dispatch: validate matches and construct an actions-owned type
  -> exhaustive Action match
      -> actions/<operation>.rs: execute one typed operation
          -> immortal-core: reusable process and operating-system behavior
```

The layers have strict responsibilities:

- `commands` contains only CLI syntax and help text.
- `dispatch` converts untyped matches into `actions` types and reports usage
  errors; it owns no action contract.
- `actions/mod.rs` owns shared typed contracts and errors; focused action files
  coordinate individual operations without embedding OS primitives.
- `start` initializes diagnostics and returns the dispatched action.
- `src/bin/<name>.rs` makes the installed name visible in the source tree and
  contains only exhaustive action routing and process-exit translation.

All executable crates implement the per-action form. `immortal` routes
configuration checks and both supervision inputs; `immortalctl` routes each
control operation to a focused handler backed by one private transport engine;
`immortaldir` routes reconciliation through a handler which creates its process
broker before Tokio; and `immortallog` separates stream writing from archive
inspection. `commands`, `dispatch`, and `start` remain private implementation
modules, while `actions` and the deliberate startup/completion contract are the
only CLI surfaces needed by each separate binary target.

Entrypoints deliberately use explicit startup and completion handling rather
than `anyhow::Result` and `?`. This preserves the public usage, configuration,
temporary-failure, permission, I/O, and operating-system exit classes. Binary
matches contain routing only; runtime creation, daemonization, transport, and
supervision remain in action or core boundaries.

`immortal` uses the Go options as a requirements inventory while adopting typed
Clap names, validation, value hints, and conventions. Parsing an option does not
imply its process behavior is implemented. New behavior arrives through typed
dispatch and actions with contract tests.

## Core boundaries

- `config`: typed service definitions, validation, resolved defaults, and
  bounded pre-daemonization environment-directory snapshots.
- `process`: wraps the `fork` crate for Unix child creation, daemonization,
  waiting, sessions, process groups, environment, identity, and signals.
- `supervisor`: desired state, retries, transitions, and shutdown coordination.
- `logging`: stdout/stderr routing, files, rotation, and external sinks.
- `control`: local protocol types plus client and server responsibilities.
- `status`: bounded transport-independent supervisor observations.
- `readiness`: the `IMMORTAL_READY_FD` token and deadline contract.
- `runtime`: safe runtime-root and supervisor discovery policy.
- `reconcile`: stable launch/applied snapshots, a bounded deletion checkpoint,
  checked supervisor launch, and desired-state planning.
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

The broker remains the direct parent and sole reaper of every managed child.
Where the platform offers a child-subreaper role it also claims that role, so
descendants orphaned inside its subtree reparent to the broker and are reaped as
containment hygiene rather than leaking to init. It assigns no durable meaning to
a PID: every request and event includes the supervisor's monotonic generation
identity. The supervisor owns policy and desired state; the broker owns
operating-system process handles and performs only requested mechanisms.

Before each broker fork, argv, environment, user/group transition, working
directory, descriptor actions, and process-group identity are fully validated
and materialized. The post-fork child performs only reviewed async-signal-safe
descriptor, credential, group, and `execve` operations. An exec-status channel
distinguishes exec failure from a successfully executed program before the
generation becomes Running.

Account resolution follows the native directory authority before daemonization
or broker creation. Linux and FreeBSD use the safe `nix` account and group-list
interfaces. Apple deliberately excludes `getgrouplist`; on macOS the platform
boundary therefore executes absolute `/usr/bin/id -G -- NAME` without a shell
so Open Directory remains authoritative. Its output is size-bounded, must be
UTF-8 numeric group identifiers, is deduplicated, and must include at least one
reported group; command failure or malformed output aborts configuration
materialization. Only the resulting numeric identity crosses broker IPC.

Broker IPC is bounded, versioned, and private. EOF or supervisor death makes the
broker stop and reap owned groups before exiting. For a descriptor-tracked
generation whose direct launcher has already exited, the broker instead owns a
pre-runtime materialized stop command: it executes that command with a hard
deadline and requires lifetime-descriptor EOF before reporting clean shutdown.
Broker death places the
supervisor in a terminal failure state; it must never spawn an untracked
replacement path. Daemon startup is reported to the invoking process only after
the broker, service lock, runtime directory, and control socket are ready.

Every broker-created service, logger, and hook group has a fail-closed lifetime
capability supplied by `fork`. Creation follows one ordered transaction:

1. a short-lived anchor reserves a fresh group before any workload exists;
2. an out-of-group helper starts with only its owner-lifetime socket;
3. the prepared workload joins the reserved group and completes exec startup;
4. the broker activates the guard by disarming and reaping the anchor; and
5. normal group cleanup disarms and reaps the persistent helper.

The broker exclusively owns the write endpoint and the typed guard value.
Prepared child descriptor closure prevents the service, logger, control IPC,
locks, and readiness channels from retaining that endpoint. If the broker is
forcibly killed, kernel descriptor closure wakes the helper, which sends
`SIGKILL` to the complete group even when that group is stopped. Unexpected
helper exit is a typed containment failure: the live broker kills the affected
group and the supervisor enters a terminal error instead of restarting through
an untracked path. Startup uses two helper forks and retains one sleeping helper
and one socket endpoint per active group. It introduces no `Arc`, mutex, shared
configuration copy, or asynchronous task; registration, lookup, and cleanup
remain logarithmic in the broker's existing ordered ownership maps.

This is process-group containment, not a portable cgroup or FreeBSD process
reaper. A workload which deliberately calls `setsid`, changes process group, or
otherwise escapes the reserved group has left the contract. Descriptor tracking
can intentionally support such self-daemonizing software through lifetime
capabilities and stop/reload hooks, but the group guard must not be represented
as containing the escaped daemon after forced broker death. The child-subreaper
role is a complementary hygiene mechanism, not stronger containment: while the
broker is alive it reaps descendants orphaned inside its subtree so their
zombies never leak to init, but it neither signals nor tracks them as owned work
and does not survive forced broker death. Platform-specific stronger containment
remains future work and cannot silently change this portable guarantee.

The implemented initial broker channel never accepts a PID from the supervisor.
Spawn and signal requests carry the supervisor's monotonic generation, and the
broker resolves that generation against its live child and process-group table.
Frames preserve non-UTF-8 Unix argv, environment, and path bytes while enforcing
hard bounds on the frame, individual fields, collection counts, and startup
deadline. The broker reports readiness only after its current-thread runtime and
`SIGCHLD` source are installed. Malformed frames and unexpected EOF fail closed;
supervisor EOF triggers group kill and reaping. Each coalesced `SIGCHLD` drains
every available wait event to exhaustion. A reaped process that matches no owned
generation or guard is classified by role: when the broker holds the
child-subreaper role it is an adopted orphan, reaped as hygiene and never
forwarded as a workload event nor allowed to abort the broker; without that role
an unowned reap remains a fatal ownership violation. A delayed 250 ms safety
sweep performs the same drain so a lost or platform-specific notification edge
cannot leave an exited child unreaped; it runs whenever the broker owns children
or holds the subreaper role, and stays idle only when neither applies.
The supervisor owns one persistent broker-reader task feeding a bounded queue;
selecting between process events, control work, timers, and Unix signals can
therefore cancel a queue receive without cancelling a partially read frame.
Long-running components share one owned TERM/INT intake boundary. The service
supervisor converts those signals into an ordered service/logger shutdown;
`immortaldir` observes them only between complete reconciliation operations and
then shuts down and reaps its launcher broker without cancelling a partial
mutation.
The controlled executor acquires exclusive runtime ownership before the broker
fork and binds its authenticated socket only after the Tokio runtime exists.
The supervisor event loop remains the sole lifecycle owner. Explicit `Exit`
sends a generation-bound detach request to the broker; only a successful detach
transitions the supervisor to `Exited`, after which the empty broker is shut
down and reaped while the service is deliberately reparented to the OS.

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

Descriptor tracking is an explicit exception to direct-child supervision, not
PID adoption. The broker creates a CLOEXEC socket pair, maps only the service
endpoint to descriptor 4, and publishes `IMMORTAL_LIFETIME_FD=4`. Application
forks may inherit that capability; the broker endpoint observes only EOF and
rejects data. Reaping the original launcher clears the observed main PID but
does not complete the logical generation. Completion requires both launcher
reaping and lifetime closure, in either order. A clean launcher followed by EOF
is published as `LifetimeClosed`; descriptor misuse is `LifetimeFailed`.

The supervisor owns stop and reload policy while the broker continues to own
all process creation and waiting. Stop, restart, halt, readiness failure, and OS
shutdown run the configured stop command as an isolated broker task and wait a
separate bounded interval for EOF. HUP runs the reload command; every other raw
signal and live-service `Exit` is rejected once descriptor tracking is selected.
A hook failure, timeout, or missing EOF restores the logical live state rather
than claiming Down. The immutable stop plan is copied once at the pre-Tokio
broker boundary so unexpected supervisor EOF can use the same bounded cleanup
contract without trusting a PID file.

Logging separates two user intents. `log` selects zero, one, or two local files;
its flat form combines stdout and stderr, while nested `stdout` and `stderr`
routes are strict and never capture an omitted stream. `logger` is one exact
external argv which always receives both child streams. The two may coexist.
`immortallog` is Immortal's replaceable local file writer and rotator, not the
user's centralized logger.

The complete graph is materialized before the broker starts. A combined local
file forms one adapter chain. Selected or split files each have their own input
pipe and adapter, while adapter pass-through descriptors and any locally
unselected child stream duplicate writers into one shared kernel pipe. That
pipe has exactly one reader: the external logger. The single-threaded broker
owns stable CLOEXEC masters and duplicates only the endpoints needed by a
spawn. The Tokio supervisor therefore never reads, copies, labels, reorders, or
buffers log bytes; kernel backpressure remains the lossless flow-control
boundary.

The shared downstream logger starts first, then local adapters, and the first
service generation is gated on every required exec succeeding. A crash leaves
pipe identity and already buffered bytes intact. `logger_restart` owns retry
limits and backoff only for the external logger; each file adapter remains an
independently supervised upstream process with the safe unbounded default.
Exhaustion before a service generation cancels childless pre-start work and
publishes `Failed`. Exhaustion after a service generation is live changes
logger health without silently stopping that service. Explicit start operations
reset failed stages.

Shutdown closes the broker's service-route and shared-pipe writer masters.
Adapter-held shared writers keep the external logger input open while adapters
drain. If an upstream adapter exceeds its deadline, it receives bounded TERM
then KILL escalation before the external logger is allowed to finish draining;
only after every upstream writer is gone can the shared reader observe EOF.
The external logger then receives its own drain and escalation interval before
broker reaping. Failed or backing-off stages own no drainable child and
normalize to Down. Permission denial, oversized lossless backpressure, split
fan-in, tail drain, and TERM-resistant expiry are process contract-tested.

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

Native lifecycle evidence is separated from cross-compilation. GitHub-hosted
Ubuntu and macOS jobs run all workspace contracts directly. The FreeBSD VM
action tracks its latest v1 release and runs the same suite on its current
default FreeBSD guest, while the Linux-hosted FreeBSD target check remains only
a fast compile gate. The benchmark metadata records the actual guest release;
a native job is not considered complete until its remote execution is green.
The harness-free resource contract additionally drives a bounded signal storm
through one broker and isolates descriptor exhaustion in a reduced-limit
subprocess, so neither fault can contaminate the test runner or leave ownership
to asynchronous cleanup.

Milestone 2 is implemented for foreground and checked daemon launches. The CLI
builds or loads the strict service model and resolves account data before any
fork. Daemon mode performs checked double-fork detachment before the core
executor creates its broker and current-thread Tokio runtime; the original
invoker returns only after broker and optional control-listener readiness.
Immediate readiness, restart/backoff decisions, bounded retry termination,
successful exit, failed exec, atomic PID publication, and final broker reaping
have black-box contracts.

Milestone 4 is implemented for controlled foreground and daemon paths. Runtime
locking, stale-socket safety, peer-authenticated bounded serving, optimistic
generation matching, all lifecycle operations, raw signal delivery, live-child
detach, and complete status publication share the same serialized executor.
Logger Starting, Ready, Backoff, and Failed health is published from the same
owner; bounded exhaustion and explicit operator recovery are contract-tested.
Once the authenticated socket is bound, the controlled state machine remains
`Initializing` while required logger stages start or back off. It publishes no
service generation during that interval, then transitions exactly once to
normal start processing or to bounded logger failure.

`immortal` resolves runtime identity before daemonization. An exact
`--control-dir` is preserved unchanged; otherwise config launches derive a safe
filename stem and direct launches require a safe `-n`/`--name`. The owning
`immortal-core` boundary creates `$HOME/.immortal` only as a canonical
effective-UID-owned mode-`0700` directory below a trusted effective-UID-owned
home. It rejects symlinks, unsafe existing modes, hidden names, unbounded
identities, and control-socket paths beyond the portable Unix limit before
creating service artifacts. No PID-derived fallback exists. `immortaldir`
continues to pass exact service directories below the platform-native
ephemeral system root.

`immortalctl` treats runtime discovery as a bounded multi-root trust boundary.
Automatic mode scans the native system root and the effective user's
`$HOME/.immortal`; exact `--runtime-dir` and `IMMORTAL_SDIR` inputs disable that
aggregation. Root failures are isolated, user ownership is checked against the
effective UID, and one global service bound covers both scans. User-home
resolution failure is also isolated so system discovery continues. Scope
remains in status output, and duplicate names require an explicit system or
user scope for mutation rather than relying on precedence.

The readiness, lifecycle-hook, and descriptor-tracking portions of milestone 3
are implemented. For each
`notify-fd` generation the broker creates a CLOEXEC socket pair, maps the child
endpoint to descriptor 3, publishes `IMMORTAL_READY_FD=3`, and monitors the
other endpoint on its current-thread Tokio runtime. Exact-token success,
invalid input, early close, and timeout are generation-bound broker events;
failure stops the group and feeds restart policy without treating a PID as
identity. Pre-start conditions have independent bounded retry policy. Post-exit
hooks run after reaping with typed exit, signal, generation, exec-failure, and
readiness-failure context; hook failure, timeout, and shutdown cleanup are
contract-tested. Descriptor tracking covers inherited lifetime EOF, launcher
PID clearing, strict stop/reload hooks, HUP mapping, raw-signal rejection,
hook failure and timeout recovery, and broker cleanup after supervisor loss.
Broker-owned logger chains now cover external commands,
replaceable file adapters, stable pipes across logger failure, combined and
separate streams, multi-stage passthrough, status, EOF drain, and bounded
downstream-first shutdown.

The sequential portion of milestone 5 is operational. `immortaldir` creates
one checked launcher broker before Tokio, compares complete scans with live
authenticated supervisors, and persists normalized launch and applied-state
snapshots below the owner-only runtime root. A bounded owner-only ledger records
desired names and consecutive absence counts atomically before mutations;
applied snapshots remain configuration authority. It starts missing definitions,
preserves unchanged and operator-stopped supervisors, applies valid changes,
stops disabled services, and halts only confirmed deletions. Replacement waits
for both control-socket disappearance and advisory-lock release. An active lock
without a control socket is deferred and retried rather than guessed from PID
metadata; an exited enabled supervisor is recreated after confirmed lock
release. Typed failed mutations remain pending for later scans while independent
services continue in the current scan. Independent starts are submitted as
bounded broker task batches within deterministic dependency waves; later waves
wait for Ready, and later dependency failure does not cascade.
Confirmed deletions remain in the ledger until Halt, socket/lock disappearance,
and applied-snapshot removal succeed. A black-box contract replaces
`immortaldir` between the two required absence scans and proves the restarted
manager completes cleanup. TERM and INT are polled while idle and alongside an
in-flight reconciliation; a received signal is latched until that mutation
reaches its safe boundary, after which the launcher broker is shut down and
reaped.

Portable start conditions remain supervisor policy, not directory policy. A
checked daemon may publish `WaitingCondition` while its broker retries the
condition with independent backoff; `immortaldir` waits for Ready before
advancing dependent waves. The operational contract proves that failed
conditions leave the service start counter at zero and that later success
unblocks reconciliation.
