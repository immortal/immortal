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
filesystem watcher delivering every event. That independence extends to the
watcher dying: a closed native event channel is reported once and its trigger
arm is retired, leaving periodic safety scans as the sole trigger rather than a
permanently ready receiver spinning the trigger loop.

### immortallog

Writes one stream to a file and rotates it, as a separate process reading its
own standard input. Keeping the byte copy and archive sweep out of the
supervisor means a slow or failing log destination cannot stall supervision,
and makes the adapter replaceable through `log_adapter` by anything which
implements the same command-line contract.

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

The supervisor deliberately owns a waitable-child policy. At the single-threaded
executor boundary, before preparation or daemonization, and at the shared
process-broker start boundary (including direct `immortaldir` launches), core's
idempotent `prepare_child_policy` calls `process::ensure_waitable_children`.
Correction and its warning are implemented in one place; a second call with
valid policy emits nothing. Failure returns before socket creation, fork or
runtime construction. Inherited `SIGCHLD=SIG_IGN` survives exec;
explicit ignoring or `SA_NOCLDWAIT` is corrected to default disposition, empty
handler mask and no flags, with one diagnostic. A valid existing handler is left
unchanged. This is core supervisor process hygiene, not executable-entrypoint
policy; embedding callers must enter before children, threads or dependent
handlers exist and retain exclusive policy/collection ownership afterwards.

Broker deadline cleanup rechecks waitable policy and performs an exact-child
nonblocking wait through `process::signal_child`. A terminal event is consumed
and returned to the owner instead of followed by a signal; a nonterminal result
leaves the child unreaped, reserving its PID even if it exits before delivery.
`ECHILD`, auto-reaping policy and observation errors never authorize delivery.
This uses fork's existing consuming wait API without needing a third public API:
only the nonterminal branch authorizes signaling. Neither the policy query nor
the wait is a lock against another handler/thread stealing child ownership.

Fixture guards follow the same ownership discipline. Broker and directly owned
leader guards collect exact PIDs, while indirect members are released through
private file handshakes rather than signal-0 observations. Auxiliary broker
STOP/CONT/KILL actions borrow the same guard, which caches any consumed terminal
event so a later wait cannot lose the outcome or retain signal authority. A final signal-free
subtree guard drains adopted children only after signal-capable fixture owners
have dropped; consumed terminal events never retain signal authority.
Daemon-wrapper returned IDs and cleanup groups are likewise observation-only,
not tokens authorizing later numeric signals.

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
It then waits a further bounded interval for group members which closed the
descriptor but are still exiting, and fails closed if one survives.
Broker death places the
supervisor in a terminal failure state; it must never spawn an untracked
replacement path. Daemon startup is reported to the invoking process only after
the broker, service lock, runtime directory, and control socket are ready.

Every broker-created service, logger, and hook group has a fail-closed lifetime
capability supplied by `fork`. Creation follows one ordered transaction:

1. an out-of-group helper starts with only its owner-lifetime socket;
2. the helper forks an anchor which creates a fresh group and exits at once,
   and leaves it unreaped, so the anchor's zombie keeps the group and its ID
   reserved before any workload exists;
3. the prepared workload joins the reserved group and completes exec startup;
4. the broker activates the guard; and
5. normal group cleanup disarms the guard: the helper reaps the anchor,
   releasing the reservation, and exits, and the broker reaps the helper.

The broker exclusively owns the write endpoint and the typed guard value. It
keeps `SIGCHLD` non-ignored without `SA_NOCLDWAIT`, so helpers and workloads remain
waitable until its serialized collection. Fork retains guard/spawn creation
under auto-reaping for 0.10.0 compatibility, but owner-side cleanup refuses
signals without waitable policy and a child pin; stopped helpers or failed
spawn children can remain as cleanup obligations. The supervisor deliberately
normalizes policy so it can collect children and complete that cleanup.
Checked daemonization alone rejects auto-reaping before fork because its
mandatory intermediate wait could not succeed under that policy in 0.10.0.
Prepared child descriptor closure prevents the service, logger, control IPC,
locks, and readiness channels from retaining that endpoint. If the broker is
forcibly killed, kernel descriptor closure wakes the helper, which sends
`SIGKILL` to the complete group even when that group is stopped, while the
anchor still pins its ID, and then reaps the anchor. Unexpected
helper exit is a typed containment failure: the live broker kills the affected
group only if a verified unreaped member leader still pins it; otherwise it
awaits the group without signalling. The supervisor enters a terminal error
instead of restarting through an untracked path. Startup uses two forks, the
helper and its anchor, and retains one sleeping helper, one zombie anchor, and
one socket endpoint per active group. The fork guard's owner-side mutex
serializes signal exchanges; no `Arc`, shared configuration copy, or
asynchronous task is introduced. Registration, lookup, and cleanup remain
logarithmic in the broker's existing ordered ownership maps.

This is process-group containment, not a portable cgroup or FreeBSD process
reaper. A workload which deliberately calls `setsid`, changes process group, or
otherwise escapes the reserved group has left the contract. Descriptor tracking
can intentionally support such self-daemonizing software through lifetime
capabilities and stop/reload hooks, but the group guard must not be represented
as containing the escaped daemon after forced broker death. The child-subreaper
role is a complementary hygiene mechanism, not stronger containment: while the
broker is alive it reaps descendants orphaned inside its subtree so their
zombies never leak to init, but it neither signals nor tracks them as owned
work. The broker's own role ends when the broker does, so the supervisor claims
a backing child-subreaper role before it forks the broker; on forced broker
death the broker's orphaned subtree reparents to the supervisor, which drains it
to exhaustion instead of leaking zombies to init. This nesting still provides
only reaping hygiene, not signal-level containment of a workload that has
deliberately escaped its reserved group. On FreeBSD the backing role must be
acquired before the broker fork because a process's reaper is fixed at fork
time; [FreeBSD.md](FreeBSD.md) explains that platform rule with a diagram and a
runnable reproducer. Platform-specific stronger containment remains future work
and cannot silently change this portable guarantee.

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
cannot leave an exited child unreaped; it runs whenever the broker owns children,
holds the subreaper role, or awaits a killed group, and stays idle otherwise.

The broker signals a process group only while something pins its numeric ID
to the group the broker created; no portable handle otherwise stops an empty
group's ID from being reused, and the `fork` probe never proves identity. An
armed group's signals are delivered by the guard helper while it holds its
unreaped anchor. The ID stays reserved even after every workload member has
gone, the launcher of a self-daemonizing service included. A dead helper makes
these requests fail instead of reaching a reused ID, even before its exit event
is observed. There is never a direct group-signal fallback for an armed guard.
Outside cleanup phases each helper round trip uses `GROUP_SIGNAL_TIMEOUT`
(500 ms). During ordered shutdown or a supervisor-loss stage, each exchange
uses the lesser of that cap and the phase's remaining time. Once the deadline
expires no further helper request is queued; skipped or failed kills still
retire the group. The active deadline belongs to broker runtime state, so
foreground-reap kills during a drain cannot start fresh timeouts. Request IDs
make success acknowledge the exact request; timeout does not cancel it.
After a foreground reap, only an acknowledged group kill or ESRCH permits
intentional disarm. Otherwise the broker closes the guard owner fail-closed,
so the resumed helper consumes any queued request and then kills the still-pinned
group on EOF. A retiring-set no-op is not a delivery acknowledgment and also
selects fail-closed release, without another explicit group request. The group
stays retired until it drains. This release uses fork's three-second helper
cleanup deadline, followed by kernel-dependent final reaping; safety cleanup
can outlast the signal/settle budget. Descriptor completion and detach still
disarm intentionally.
Without an armed guard, for example once the helper exited, only an unreaped
direct child whose PID equals the PGID and which is still a member pins the
group; membership is checked with `getpgid` immediately before each group signal. A live nonleader can leave
between that query and delivery, so its membership never authorizes an unarmed
group signal: only its owned PID is signalled and the old group is retired.
A leader's PID remains reserved even if it moves after the query. FreeBSD and
macOS do not report a zombie's group, so there an exited child pins nothing and a helper
which dies with it leaves its group awaited without a signal. A child that left its
group in place, for example by calling `setsid` itself, is also signalled as a
process, which its unreaped PID pins, and that delivery answers the
supervisor's request: the group signal then only reaches members left behind,
and macOS, which skips zombie members, answers it with `EPERM` once only the
anchor remains. Without the guard's pin, the group the child left is awaited
without a signal. The kernel frees a group ID only once the old
group is gone, so when another live generation's armed guard or member leader
pins a group with the same ID, that ID names the other generation's group: no
retire or kill made on behalf of a generation which no longer pins it touches
that ID, or the other generation could never be stopped. A daemon which calls
`setsid` leaves the reserved group and is beyond any group signal. A probe
reporting a group present is never a reason to signal it.

Killing a group does not end the broker's ownership of it, and neither does
releasing one it can no longer signal. Every kill goes through one path that
records the group in a retiring set: a foreground generation's group when its
main child is reaped, through the armed helper, a pinned group whose
guard exits, a group the supervisor asks it to kill, every pinned group at
supervisor loss and at shutdown escalation, an overrunning supervisor-loss stop
hook's group, and the reserved group of a guarded spawn that fails, which the
dropped guard kills while its anchor still pins it. A descriptor group whose
guard exits has no pin left and is retired without any signal. A tracked
generation that completes, whichever of launcher reaping and lifetime closure
comes last, has ended by its own contract, so its group is retired without a
signal before its guard is disarmed. A member which closed the lifetime
descriptor may still be running, so it is awaited rather than released
untracked. A group leaves the set once the `fork`
null-signal probe reports that it has no member left; zombies count as members,
so a group drains only once every member has been reaped. Without this wait a
member still exiting outlived the broker and reparented to the supervisor. A
reused ID, an `EPERM` member, or a probe failure only keeps a group retiring
until the bounded wait fails closed. The set offers no iteration, and no group
signal, a supervisor request included, reaches a group it contains; a retiring
generation's unreaped child can still be signalled as a process. A foreground
group whose guard already exited is not killed again when its main child is
reaped. An ID which a newly registered or detached
generation leads has provably drained and is forgotten, at registration and
before any acknowledgement is written.

Ordered shutdown sends TERM to every live generation as above and to every
descriptor group whose guard is still armed, spends a 2-second grace on both
signal delivery and settling, then kills the same targets and retires their
groups, with 3 more seconds for delivery and settling. Retiring groups are awaited and never signalled. A tracked generation
completes only when its lifetime descriptor closes, never because its group
looks empty: a daemon that called `setsid` leaves its reserved group while it
still holds the descriptor. Shutdown
applies and forwards lifetime observations while it waits, so a service that
ends during the wait completes normally and the supervisor never misses the
closure. Shutdown completes only
when no workload, open tracked lifetime, or retiring group remains; otherwise
it reports `ShutdownFailed` and keeps every remaining generation, armed guard,
and retiring group, so supervisor-loss cleanup can still run a tracked
service's stop command. A tracked service whose escaped daemon still holds the
lifetime descriptor, or whose members outlive its completed generation,
therefore fails shutdown closed instead of being killed by an ID nothing pins.
Both phase deadlines start before their signal sweeps, keeping delivery plus
settling within two plus three seconds regardless of helper count. As with all
process cleanup, final kernel reaping and event-write scheduling are not hard
real-time guarantees. The supervisor waits for the shutdown answer longer than
the broker's 5-second signal/settle budget, so it reports the
broker's own failure rather than a timeout of its own, and it ignores reaps
and generation observations that race the request while still rejecting an
owed signal acknowledgement. Each settle step drains reaps
and wakes on `SIGCHLD` or a short backed-off poll, because a member reaped by
another parent, or by launchd on macOS, sends the broker no signal.
Supervisor-loss cleanup waits only for owned workloads before it runs the
descriptor stop command, so a killed group held by a process outside the
broker's reach cannot keep the command from stopping the service, and drains
every retiring group last, within the same 3-second bound, on every path.
Each supervisor-loss stage likewise shares its deadline between helper
exchanges, foreground-reap kills, and settling, rather than restarting the
clock after a signal sweep.
Processes which escaped the reserved group remain outside this guarantee.
The supervisor owns one persistent broker-reader task feeding a bounded queue;
selecting between process events, control work, timers, and Unix signals can
therefore cancel a queue receive without cancelling a partially read frame.
A lost supervisor is recognized from either direction of that socket — a read
observing end of file or a connection reset, and a write observing a broken
pipe — and every such observation converges on one cleanup path, because a
descriptor-tracked generation's configured stop command runs only there.
Long-running components share one owned TERM/INT intake boundary. The broker is
forked into its own process group so a terminal interrupt, which is delivered
to the whole foreground group, reaches only the supervisor: the broker has no
terminal-signal handler, and its death fails every containment guard closed
before the ordered shutdown can run. The service
supervisor converts those signals into an ordered service/logger shutdown;
`immortaldir` observes them only between complete reconciliation operations and
then shuts down and reaps its launcher broker without cancelling a partial
mutation.
The controlled executor acquires exclusive runtime ownership before the broker
fork and binds its authenticated socket only after the Tokio runtime exists.
The broker's readiness watcher is independent of supervisor state, so a
declaration or readiness failure can arrive after a concurrent lifecycle or
job-control command has already moved the generation out of `Running`. The
executor therefore accepts readiness for any generation the supervisor still
owns: a paused generation records it so resuming observes the declared value,
a stopping or terminal generation discards it without disturbing the stop
grace deadline, and only an unowned generation remains a protocol fault.
Job control suspends the readiness clock rather than racing it: pausing a
generation disarms its readiness deadline, because a stopped child cannot
declare anything, and resuming an undeclared generation restarts the wait. A
deadline which survives into a paused state disarms instead of faulting, since
an operator command must never end supervision.
Reaps follow the same rule for the same reason. A spawn which fails after the
fork still leaves a process the broker must reap, so its `Child` event arrives
once the supervisor has already handled the failure and moved into backoff, a
terminal failure, or a newer generation. Any generation the supervisor has
already issued is therefore stale bookkeeping and is absorbed, while a reap for
a generation it never issued remains a protocol fault.
Signal acknowledgements can trail a reap in the same way. A group stop sends
TERM and then CONT as separate broker requests, so the broker can reap a service
which dies on TERM before it reads CONT, and then acknowledge CONT as failed for
the generation it has already removed. The supervisor therefore drains every
owed acknowledgement within one bounded broker-event deadline, then begins
logger shutdown in the same pass, so the loop never waits without an event or
deadline to wake it; broker shutdown keeps rejecting any acknowledgement left
unread. A failed acknowledgement settles an operator signal as failed even
without an OS error, so a signal racing the service's exit is never reported as
delivered.
The supervisor event loop remains the sole lifecycle owner. The control accept
loop treats peer-attributable and resource-attributable accept failures as
transient: aborted or reset peers, interrupted accepts, and descriptor or
kernel-buffer exhaustion are absorbed, with exhaustion paced by a short bounded
backoff so the loop cannot spin while descriptors stay unavailable. Only an
unusable listener, a closed client semaphore, or a departed supervisor ends the
loop, because a fatal accept classification stops the whole supervisor and lets
the broker kill a healthy service. Explicit `Exit`
sends a generation-bound detach request to the broker; only a successful detach
transitions the supervisor to `Exited`, after which the empty broker is shut
down and reaped while the service is deliberately reparented to the OS; the
released group also leaves the retiring set, so shutdown never awaits it.

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
is published as `LifetimeClosed`; descriptor misuse is `LifetimeFailed`. On
completion the broker retires the group without a signal and disarms the group
guard, so shutdown waits for any member that closed the descriptor early.

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
- a null-signal process-group membership probe which rejects the broadcast
  group, reports `EPERM` as present, and never claims identity;
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
