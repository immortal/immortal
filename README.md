# immortal

Immortal is being rebuilt in Rust as a focused Unix process supervisor for
Linux, macOS, and FreeBSD. It is not an init system and will not run as PID 1.
The operating system's init system starts `immortaldir`; Immortal then owns the
lifecycle of configured application processes.

```text
OS init (PID 1)
    `-- immortaldir
          |-- immortal: api ---- supervised process group
          |                       `-- supervised logger pipeline
          |-- immortal: worker
          `-- immortal: database

immortalctl -------- authenticated local control --------> immortal
```

The Go implementation remains available on `master` and `develop` as a
requirements and migration reference. Those branches are read-only during the
Rust rewrite. Compatibility is claimed only after the corresponding contract
suite passes.

## Design direction

The rewrite keeps the useful product shape of the Go release while correcting
the process-management problems identified in the original
[Hacker News review](https://news.ycombinator.com/item?id=14003971), the
[Lobsters discussion](https://lobste.rs/s/svv0kz/nix_cross_platform_os_agnostic),
and the design documentation for
[daemontools](https://cr.yp.to/daemontools/supervise.html),
[runit](https://smarden.org/runit/benefits), and
[s6](https://skarnet.org/software/s6/overview.html).

- Owned services remain direct children until they are reaped. PID files are
  output-only metadata, never process identity.
- Each service generation owns a process group. Lifecycle cleanup targets the
  group; compatibility signal commands target the main process unless stated
  otherwise.
- Self-daemonizing applications use an explicit descriptor-based compatibility
  mode with application control hooks. PID adoption is not considered safe.
- Tokio is used for timers, signals, pipes, and Unix sockets, but process
  supervisors use a current-thread runtime so `fork` never runs from a
  multithreaded runtime.
- Logger commands are separately supervised processes. Immortal preserves the
  pipes so a service and its logger can restart independently.
- The privileged HTTP server is replaced by a bounded, versioned local control
  protocol. A future web gateway must be a separate unprivileged component.
- Filesystem notifications only trigger reconciliation. A complete desired
  versus current-state scan remains authoritative.
- Restarting forever with capped backoff is the default. Explicit attempt,
  burst/window, or elapsed-time limits can place a crash loop in `Failed`.
- Readiness and portable start conditions handle dependencies without copying
  systemd targets into Immortal.

### Canonical fork-library gate

The current `fork` 0.8 API provides the required fork, daemon/session, PID, and
terminated-child wait primitives. `immortal-core::process` now converts its
safe re-exported wait inspection results into typed `Exited` and `Signalled`
events and refuses to misreport a stopped status as termination. The lifecycle
executor deliberately remains gated until the canonical `immortal/fork`
library also provides safe wrappers for:

- typed `Stopped` and `Continued` events alongside terminal waits, including
  nonblocking `WUNTRACED`/`WCONTINUED` collection where the platform supports it;
- typed signal delivery to one PID and to an owned process group;
- process-group/session setup errors which preserve the underlying OS error;
- checked daemon startup IPC and descriptor allow-list primitives.

These capabilities will be added and tested in `fork`, then consumed only
through `immortal-core::process`. Immortal will not add direct `libc` calls or a
second process library to work around the boundary.

Dependency planning is deterministic and portable. Enabled services are
topologically sorted into start waves; services in one wave may start
concurrently, while the next wave waits for their requirements to become
Ready. Missing or disabled requirements and cycles reject the plan. A
requirement gates start only—later dependency failure does not cascade a stop.

See [DESIGN.md](DESIGN.md) for module ownership and implementation rules.

## Workspace

- `immortal-core`: reusable configuration, process, supervision, logging,
  control-protocol, reconciliation, and platform behavior.
- `immortal`: daemonize and supervise one service.
- `immortalctl`: inspect supervisors, change desired state, and deliver signals.
- `immortaldir`: reconcile a directory of service definitions.
- `immortallog`: minimal, replaceable stdin-to-file compatibility adapter with
  bounded-memory streaming, rotation, retention, timestamps, and optional raw
  byte pass-through. External logger commands remain the preferred interface.

Each CLI follows the one-way flow:

```text
commands -> dispatch -> actions -> start -> main
```

## Compatibility policy

The unversioned Go YAML format is schema v1. A strict schema v2 will use argv
arrays and explicit lifecycle policy. Go CLI spellings remain accepted aliases.
Intentional behavior changes are documented and tested instead of being hidden.

| Contract | Rust policy |
|---|---|
| Go CLI flags and signal aliases | Preserve as aliases and contract fixtures |
| Unversioned `.yml` definitions | Parse as v1 and emit migration warnings |
| Runtime paths and service names | Preserve where safe; validate ownership |
| PID output files | Preserve as output-only metadata |
| `pid.follow` / `-f` | Migrate to descriptor tracking plus explicit hooks |
| HTTP-over-Unix-socket control | Replace with a bounded versioned protocol |
| Go status JSON | Preserve equivalent information in `immortalctl --output json` |
| Exact internal Go architecture | Do not preserve |

Two open requests are explicit requirements:

- [Issue #71](https://github.com/immortal/immortal/issues/71): support
  retry-until-success with `restart = on-failure`, configurable successful exit
  codes, and optional supervisor exit when no restart is required.
- [Issue #68](https://github.com/immortal/immortal/issues/68): support portable
  readiness and start conditions. OS-specific boot ordering remains the init
  system's responsibility.

### Configuration schemas

Schema v1 is the released, unversioned Go format. It remains readable with
migration warnings. Its `cmd`, `logger`, `require_cmd`, and `post_exit` strings
have legacy whitespace/shell behavior; normalized output makes those choices
explicit. `retries: -1` means unbounded restarts, `0` means the initial start
only, and a positive value is the number of restarts after the initial start.
Legacy log `size` is interpreted in MiB. Environment scalars are converted to
strings as the Go parser did. A `pid.follow` path selects descriptor-tracking
compatibility mode but is never trusted as process identity.

The released Go example files are retained as migration fixtures. Examples
which used `pid.follow` without an explicit lifecycle hook are intentionally
rejected: the Rust implementation will not guess how to stop or reload a
self-daemonized process. Supported examples must round-trip through emitted
schema v2 without semantic changes.

Schema v2 is strict: unknown fields fail validation, commands are argv arrays,
durations state their unit, and every nested policy is typed. The complete
currently implemented shape is:

```yaml
version: 2
enabled: true
command: [/usr/local/bin/api, --foreground]
working_directory: /srv/api
environment:
  RUST_LOG: info
environment_mode: inherit # inherit | clear
user: www
start_delay_seconds: 0

restart:
  policy: on-failure       # always | on-failure | never
  success_exit_codes: [0]
  exit_when_done: false
  limits:
    max_retries: null      # restarts after the initial start
    max_elapsed_seconds: null
    burst:                 # null disables the rolling limit
      starts: 5
      window_seconds: 60
  backoff:
    initial_seconds: 1
    max_seconds: 60
    multiplier: 2
    jitter_percent: 20
    reset_after_seconds: 60

readiness:
  mode: immediate         # immediate | notify-fd
  timeout_seconds: 30
requires: [database]
start_condition:
  command: [/usr/bin/test, -e, /var/run/network-ready]
  timeout_seconds: 10
  backoff:
    initial_seconds: 1
    max_seconds: 30
    multiplier: 2
    jitter_percent: 20
post_exit:
  command: [/usr/local/libexec/api-cleanup]
  timeout_seconds: 30

logging:
  combine_stderr: false
  stdout:
    file:
      file: /var/log/api.log
      max_age_seconds: 86400
      keep: 7
      max_bytes: 10485760
      max_total_bytes: 73400320
      timestamp: true
    logger: [/usr/bin/logger, -t, api]
  stderr:
    file: {}
    logger: null

pid_files:
  supervisor: /var/run/api.supervisor.pid
  main: /var/run/api.pid
process_mode: foreground  # foreground | descriptor-tracking
```

All shown sections except `version` and `command` have defaults. Absent restart
limits mean retry forever. A burst value requires both nonzero fields.
`success_exit_codes` cannot be empty. `notify-fd` will publish
`IMMORTAL_READY_FD` when its executor is implemented. The child must write the
exact six-byte `READY\n` token before its configured deadline; fragmented writes
are accepted, while invalid tokens, early EOF, and timeout fail that generation.
Descriptor tracking requires explicit lifecycle hooks and deliberately does not
adopt a PID.

When reading a file, Immortal resolves working directories, PID/log paths,
hook/logger executables containing `/`, and service executables containing `/`
before daemonization. File/PID/hook/logger paths are relative to the definition
directory; the service executable is relative to its resolved working
directory. Bare executable names remain unresolved for the process executor's
future `PATH` lookup. `environment_mode: inherit` defines overrides after the
supervisor environment; `clear` defines an empty base with only configured
values.

Logging routes are process chains, not in-supervisor multiwriters. For example,
a file plus an external logger is normalized to `service -> immortallog ->
external logger`. `combine_stderr: true` attaches both child streams to the
stdout chain and conflicts with an explicit stderr route. `immortallog
--passthrough` writes the original bytes downstream even when its file copy is
timestamped, and it imposes no maximum line length. Rotation syncs the live
file, atomically renames it into an Immortal-owned archive namespace, creates a
replacement, and enforces archive-count and aggregate-byte limits both on open
and after rotation.

Exactly one service source is accepted. With `--config`, direct command options
are rejected instead of being silently merged; change the definition or emit
and edit schema v2. Without `--config`, CLI defaults are resolved first and
explicit CLI values override only those defaults. Arguments after the child
command begins are always child argv, even when they look like Immortal flags.

### Runtime and control boundary

The system runtime root remains `/var/run/immortal` by default. Each service is
discovered only at `ROOT/SERVICE/immortal.sock`. The root must be absolute,
canonical, a real directory, and not group/world writable. Service directories
must grant no group/other bits (normally `0700`); sockets must be mode `0600`;
root, service directory, and socket ownership must agree. Hidden, unsafe,
symlinked, wrongly owned, or
wrongly typed entries are reported and ignored without mutation.

The server authorizes only root or the socket owner using native Unix peer
credentials on Linux, macOS, and FreeBSD. It never removes an entry merely
because it looks stale, and listener cleanup removes only the exact socket
device/inode it created. Frames are limited to 64 KiB, reads/writes/connects and
accepts have five-second idle deadlines, and active clients are bounded. Every
mutation is preceded by a status probe and carries `NoChild` or an exact
generation; unguarded mutations and generation races are rejected.

Status responses use a typed bounded binary payload rather than an
interpolated string or privileged JSON parser. The payload carries supervisor
and main PID, desired and observed state, readiness, uptime/down time, start and
failure counters, last result, backoff, logger health, and exact argv. Command
argument count, individual argument length, and total frame size are bounded.
`immortalctl` alone converts this payload to stable JSON or a
control-character-safe table.
The authenticated socket loop isolates each client, forwards requests through
a bounded channel, and waits for the single-owner supervisor event loop to
publish the completion response. Socket tasks never mutate lifecycle state.
Shutdown aborts and joins every remaining connection task.

Lifecycle mutations wait up to `--timeout 30` seconds by default. Start waits
for Ready, stop for Down, restart for a different Ready generation, and once
for a generation to appear and then return Down. Halt/exit complete when their
owned control socket disappears. `--no-wait` explicitly returns after request
acceptance. The outer deadline bounds polling and each generation comparison
remains race-safe.

Filesystem notifications use the native recommended backend (inotify,
FSEvents, or kqueue) only as hints. Hints are nonrecursive and debounced for
250 ms before a complete scan. Saturated/coalesced event delivery is safe
because a complete scan runs at startup and every 30 seconds regardless of
notifications.

`immortaldir --dry-run` can run once or continuously through this watcher path
without touching supervisors. Non-dry-run reconciliation remains unavailable
until the fork-backed process and control-server runtime is implemented.
The continuous planner retains last-known-good definitions, treats invalid
replacements as present, and requires two complete scans to confirm deletion.
Incomplete enumeration never advances deletion confirmation. Semantic
`START`, `RESTART`, and `STOP` transitions are emitted once; unchanged and
first-missing states are `KEEP`.

### Exit status contract

Clap retains `0` for help/version and `2` for syntax errors. After typed
dispatch, every executable uses a shared BSD `sysexits(3)`-style contract:

| Code | Meaning |
|---:|---|
| 0 | success |
| 64 | typed usage/dispatch error |
| 65 | malformed data or protocol |
| 66 | missing file or service |
| 69 | unavailable capability or supervisor |
| 70 | internal invariant/software failure |
| 71 | operating-system facility failure |
| 73 | runtime/output creation failure |
| 74 | I/O failure |
| 75 | retryable generation conflict or timeout |
| 77 | permission/peer authorization failure |
| 78 | invalid service/runtime configuration |
| 79 | partial multi-service failure |

## Implementation status

An item is checked only after implementation, documentation, success and
failure tests, and required CI pass.

### General foundation

- [x] Rust workspace and one-way CLI layering.
- [x] Initial CLI parsers and parser unit tests.
- [x] DevPod CI and FreeBSD cross-check baseline.
- [x] Document focused-supervisor scope and reject PID 1 ambitions.
- [x] Record research findings and compatibility decisions.
- [x] Capture released Go CLI, YAML example, and migration contract fixtures.
- [ ] Add fork-backed Go process/path behavior contract fixtures.
- [x] Add resource-bounded schema v1/v2 configuration parsing.
- [x] Add normalized configuration resolution and migration output.
- [x] Define stable application errors and CLI exit codes.
- [x] Add safe runtime-directory ownership and permission policy.
- [x] Add process-generation identifiers independent of PIDs.
- [x] Wrap terminated-child waits as typed exited/signalled results.
- [ ] Add typed stopped/continued collection to `fork`.
- [ ] Add native Linux, macOS, and FreeBSD lifecycle jobs.
- [x] Add deterministic arbitrary/truncation/byte-mutation decoder corpora.
- [ ] Add continuous coverage-guided configuration and protocol fuzzing.
- [x] Add release baselines for configuration and control codecs.
- [ ] Add cross-platform lifecycle benchmarks and reviewed regression budgets.

### immortal

#### CLI and configuration

- [x] Accept every released Go short flag and modern long alias.
- [x] Preserve option-looking child arguments after the command begins.
- [x] Define configuration versus CLI precedence explicitly.
- [x] Implement `--check-config` / `-cc` success and failure behavior.
- [x] Emit equivalent schema v2 without modifying the input file.
- [x] Validate command, cwd, environment, PID outputs, logger, hooks, readiness,
  restart policy, and start conditions.
- [ ] Resolve configured users against the target OS account database.
- [x] Resolve absolute paths before daemonization changes cwd.
- [x] Make environment inheritance, clearing, and override order deterministic.
- [x] Reject empty commands, invalid durations, duplicate keys, conflicting
  options, oversized files, and excessive YAML nesting/aliases.
- [ ] Reject unknown users and enforce UID/GID transition policy.

#### Process lifecycle

- [ ] Run and reap one foreground direct child.
- [ ] Fork only through `immortal-core::process` and the `fork` crate.
- [ ] Daemonize before creating Tokio or any other thread.
- [ ] Implement double-fork/session detachment and safe stdio redirection.
- [ ] Preserve inherited descriptors through an explicit allow-list.
- [ ] Report daemon startup success or failure to the invoking process.
- [ ] Hold the service lock descriptor for the supervisor lifetime.
- [ ] Remove stale sockets only after acquiring the service lock.
- [ ] Create every service generation in its own process group.
- [ ] Distinguish exec failure from a successfully executed process.
- [ ] Drain all child wait events after each coalesced `SIGCHLD`.
- [ ] Clean remaining process-group members before generation reuse.
- [ ] Write and invalidate configured parent/child PID files atomically.
- [ ] Never use signal 0 or PID files to identify an owned service.

#### State and restart policy

- [ ] Implement `Initializing`, `WaitingCondition`, `Starting`, `Running`,
  `Ready`, `Paused`, `Stopping`, `Backoff`, `Completed`, `Failed`, and `Exited`.
- [x] Keep desired state separate from observed state.
- [ ] Implement Up, Down, Once, Restart, Halt, and Exit transitions.
- [x] Implement `always`, `on-failure`, and `never` restart policies.
- [x] Implement configurable successful exit codes and `exit_when_done`.
- [x] Implement exponential backoff with a stable-runtime reset.
- [x] Implement legacy retry limits plus v2 attempt, burst/window, and elapsed
  retry limits.
- [x] Keep condition failure/backoff separate from service attempts.
- [ ] Ensure manual start/restart resets Backoff and Failed.
- [ ] Return valid status in every state, including before the first child.
- [ ] Gracefully halt on supervisor `SIGTERM` and `SIGINT`.

#### Readiness, hooks, and compatibility

- [ ] Support immediate readiness after successful exec.
- [x] Define and test the bounded `IMMORTAL_READY_FD` token and timeout reader.
- [ ] Create, inherit, and monitor the readiness descriptor through `fork`.
- [x] Define and validate argv conditions with independent timeout/backoff.
- [ ] Execute pre-start conditions through the process broker.
- [ ] Run bounded post-exit hooks with exit/signal context.
- [ ] Implement descriptor-based fghack lifetime tracking.
- [ ] Require stop/reload hooks for fghack and reject raw adopted-PID signals.
- [ ] Document foreground invocations for common self-daemonizing software.

### immortalctl

#### Discovery, status, and output

- [ ] Discover system and user supervisors from documented runtime roots.
- [x] Validate ownership and ignore malformed/unknown entries.
- [x] Avoid broad stale-directory deletion.
- [x] Show all services when no command is supplied.
- [x] Support one service, `--all`, and legacy `*`.
- [x] Provide stable table and JSON output without terminal escapes in JSON.
- [x] Define, bound, transport, and render supervisor/main PID, generation,
  desired/state, readiness, uptime/down time, starts, failures, last result,
  backoff, logger health, and command.
- [ ] Populate all typed status fields from the fork-backed runtime.
- [x] Represent childless states without assuming a PID exists.
- [x] Define nonzero exits for missing targets, partial failure, authorization,
  timeout, and protocol mismatch.

#### Lifecycle commands

- [ ] `status`: inspect without mutation.
- [ ] `start` / `up`: set Up and reset configured failure.
- [ ] `stop` / `down`: TERM+CONT the group, escalate, remain supervised Down.
- [ ] `once`: start one generation and remain Down after it exits.
- [ ] `restart`: stop/reap then start a new generation in the same supervisor.
- [ ] `exit`: explicitly leave the service running and warn about orphaning.
- [ ] `halt`: stop the group, drain logging, and exit the supervisor.
- [x] Wait deterministically for typed lifecycle completion with a hard timeout.
- [x] Provide `--no-wait` for explicitly asynchronous control.

#### Signal delivery

- [ ] `-1` / `signal usr1` -> main process.
- [ ] `-2` / `signal usr2` -> main process.
- [ ] `-a` / `signal alrm` -> main process.
- [ ] `-c` / `signal cont` -> main process.
- [ ] `-h` / `signal hup` -> main process.
- [ ] `-i` / `signal int` -> main process.
- [ ] `-k` / `signal kill` -> service process group.
- [ ] `-in` / `signal ttin` -> main process.
- [ ] `-ou` / `signal ttou` -> main process.
- [ ] `-q` / `signal quit` -> main process.
- [ ] `-s` / `signal stop` -> main process.
- [ ] `-t` / `signal term` -> main process.
- [ ] `-w` / `signal winch` -> main process.
- [x] Reject multiple conflicting legacy signal flags.
- [ ] Reject absent, exited, stale-generation, and fghack targets.
- [x] Support explicit `--scope main|group`.
- [ ] Confirm STOP/TTIN/TTOU/CONT through child wait events.
- [ ] Preserve the signal exit reason for restart-policy decisions.

#### Control protocol

- [x] Replace HTTP with a fixed versioned request and bounded response codec.
- [x] Authenticate root or the supervisor UID using Unix peer credentials.
- [x] Restrict runtime-directory and socket modes.
- [x] Bound frames, clients, reads, writes, and idle time.
- [x] Forward authenticated requests to one lifecycle owner over a bounded channel.
- [ ] Run the authenticated control-server loop inside `immortal`.
- [x] Reject malformed, truncated, oversized, unknown-version, and unknown-op
  requests.
- [x] Bind mutations to an expected service generation.
- [x] Keep JSON on the client/output side, not privileged server input.

### immortaldir

#### Reconciliation

- [x] Validate and canonicalize the definitions directory.
- [x] Scan only top-level non-hidden regular `*.yml` files.
- [x] Ignore editor swap/temp files and unrelated extensions.
- [x] Reject unsafe names, duplicate names, symlinks, path traversal, oversized
  files, and excessive definition counts.
- [x] Read each definition as a stable snapshot before parsing.
- [x] Retain the last-known-good service after an invalid/partial replacement.
- [x] Compare semantic configuration rather than mtimes.
- [x] Use native inotify, FSEvents, and kqueue notifications.
- [x] Treat notifications only as full-reconciliation triggers.
- [x] Debounce editor write/rename sequences.
- [x] Perform a 30-second safety reconciliation.
- [x] Recover from dropped and coalesced filesystem notifications.
- [x] Implement mutation-free `--once --dry-run` output.
- [x] Never remove unknown runtime files or directories.
- [ ] Detect stale supervisors via lock/control state rather than PID guessing.

#### Desired state and dependencies

- [ ] Start enabled definitions missing from runtime state.
- [ ] Preserve healthy unchanged supervisors.
- [x] Plan restart only after a valid semantic configuration change.
- [ ] Apply a planned restart through the supervisor control boundary.
- [x] Confirm stable deletion across two complete authoritative scans.
- [ ] Stop and exit a service after confirmed deletion.
- [x] Retain persistent `enabled: false` in desired state.
- [ ] Apply disabled desired state to a live supervisor.
- [ ] Preserve an operator-requested Down state while its supervisor lives.
- [ ] Restart an exited supervisor whose definition remains enabled.
- [x] Prevent duplicate semantic plan actions from watcher and periodic scans.
- [ ] Retry and deduplicate operational mutations after partial failure.
- [ ] Bound concurrent starts/restarts and isolate per-service failures.
- [x] Validate `requires`, missing dependencies, and cycles.
- [ ] Start independent services concurrently.
- [ ] Gate dependent starts on Ready without later cascading stops.
- [ ] Keep waiting dependents from consuming retry limits.
- [x] Plan portable start conditions with independent timeout/backoff.
- [ ] Gate operational starts on broker-executed condition success.
- [ ] Document Linux, FreeBSD, and macOS boot/network ordering.
- [ ] Add issue #68 compatibility and regression fixtures.

### Logging

- [ ] Supervise external logger commands as independent children.
- [x] Use argv rather than implicit shell strings in v2.
- [ ] Confirm logger readiness before starting its service.
- [ ] Preserve stable pipe endpoints across service/logger restarts.
- [ ] Restart service and logger independently without losing the pipe.
- [ ] Expose logger failure/backoff in status.
- [ ] Default to lossless backpressure instead of silent dropping.
- [ ] Stop the service before draining and stopping its logging chain.
- [x] Provide a small, replaceable `immortallog` file compatibility adapter.
- [ ] Support legacy combined output, separate stderr, and file-plus-command.
- [ ] Keep byte fan-out out of the supervisor.
- [x] Sync before rotation and use atomic timestamped archives.
- [x] Recover interrupted rotation and enforce count/total-size retention.
- [x] Inject disk-write/sync failure and broken downstream pipes.
- [x] Stream huge and partial lines without unbounded line buffering.
- [ ] Test real permission failures, logger crash loops, pipe backpressure, and
  shutdown drain timeouts.

## Performance baselines

Run the dependency-free release harness inside DevPod:

```sh
scripts/dev-ssh cargo bench -p immortal-core --bench core_contracts
```

The initial Linux x86_64 DevPod medians recorded on 2026-07-11 are
informational reference points:

| Contract | Median |
|---|---:|
| Representative schema-v2 parse | 82,602 ns/op |
| Control request encode + decode | 39 ns/op |
| Typed status encode + decode | 130 ns/op |

These are not portable CI thresholds. Regression budgets will be set only after
the fork-backed spawn/wait/signal path is measurable on Linux, macOS, and
FreeBSD and normal host variance is known.

## Build and validation

Run project commands inside DevPod:

```sh
scripts/dev-up
scripts/dev-ssh just ci
scripts/dev-ssh cargo check --workspace --target x86_64-unknown-freebsd --locked
```

Process changes require native lifecycle tests on Linux, macOS, and FreeBSD.
Every process test must have a hard timeout and clean up every child and process
group on both success and failure. Dependency changes must pass `cargo audit`
and `cargo deny check` before their checklist item is complete.
