# Validation and comparative evidence

Immortal is intended to become a focused, portable daemon supervisor for
Linux, macOS, and FreeBSD. It is not an init system and does not compete with
systemd features such as mounts, timers, device management, or Linux cgroup
policy. Claims about correctness, resilience, or performance require the
evidence defined here; checklist completion or a green workflow definition is
not evidence by itself.

## Evidence rules

Every requirement has one stable identifier and one of four states:

- **Proven**: the referenced automated contract and every applicable native job
  pass for the exact commit.
- **Observed**: retained real-host or comparative evidence exists but is not an
  automated release gate.
- **Pending**: the implementation or required evidence is incomplete.
- **Out of scope**: the behavior is deliberately excluded and its operational
  consequence is documented.

An automated contract must state its deadline and cleanup ownership. Manual
evidence must record the commit, platform, kernel, CPU, toolchain, command,
configuration, outcome, and artifact checksum. A rerun which replaces an
earlier artifact is not an independent sample.

## Correctness and resilience matrix

| ID | Requirement | Evidence | Platforms | State |
|---|---|---|---|---|
| COR-001 | A service generation is identified independently of a reusable PID. | Supervisor, control, and broker generation contracts | All | Proven |
| COR-002 | Stop, restart, and halt clean the owned process group before generation reuse. | Fork-backed process and controlled lifecycle contracts | All | Proven |
| COR-003 | Exec failure is distinct from successful execution and never publishes Ready. | Broker and executor contracts | All | Proven |
| COR-004 | Configuration, control, broker, and status inputs are bounded and fail closed. | Unit mutation corpora, fuzz workflows, and protocol contracts | All | Proven |
| COR-005 | Unexpected broker death cannot create a duplicate replacement or leave a live member in an owned process group. | `broker_death_contract` plus `fork` group-guard contracts | All | Proven |
| COR-006 | Deliberate process-group or session escape is never misrepresented as portable containment. | Documented limitation and `fork` escape fixture | All | Proven |
| RES-001 | Supervisor loss makes the broker stop and clean every owned group. | Broker supervisor-loss contract | All | Proven |
| RES-002 | Lost or coalesced child notifications cannot leave an owned zombie. | Delayed reap sweep and native lifecycle contracts | All | Proven |
| RES-003 | Logger failure, backpressure, and shutdown preserve the configured lossless contract. | Logger restart, file-adapter, drain, and foreground contracts | All | Proven |
| RES-004 | Interrupted reconciliation retains last-known-good state and retries independent failures. | Reconcile unit and operational contracts | All | Proven |
| RES-005 | Signal storms, control-client saturation, descriptor exhaustion, and interrupted system calls remain bounded. | `resource_fault_contract`, bounded control-listener and `fork` EINTR contracts, plus the adversarial resource campaign | All | Pending |
| SEC-001 | Runtime discovery and control authenticate ownership without following unsafe filesystem entries. | Runtime and control contracts | All | Proven |
| SEC-002 | Dependencies pass audit, license, source, and duplicate-version policy. | `cargo audit` and `cargo deny --all-features check` | All | Proven |

`Proven` describes repository contracts, not production readiness. A candidate
must still pass the real-host gates below for its exact commit.

COR-005 is deliberately bounded by COR-006. A process which creates a new
session or joins another process group has escaped the portable ownership unit;
the project must expose that limitation rather than claiming cgroup- or
subreaper-equivalent containment. The released `fork` 0.9.1 crate passes its
running, stopped, empty-startup, descriptor-isolation, and cleanup contracts on
native Linux, macOS, and FreeBSD, including an explicit session-escape fixture.
The combined Immortal candidate passes its broker-death and complete workspace
contracts on the same three native platforms. Immortal locks the exact crates.io
release and checksum; no Git or local patch overrides the reviewed source.

RES-005 has deterministic contracts for a bounded stop/continue storm,
descriptor exhaustion before broker creation, active control-client limits,
and interrupted waits. Those contracts pass natively on all three platforms.
RES-005 remains Pending until the retained resource campaign confirms bounded
descriptor, process, memory, scheduling, and cleanup behavior under sustained
load.

### Current candidate evidence

The 2026-07-14 review binds the process-containment claims to exact revisions:

- The signed `fork` 0.9.1 tag resolves to commit
  `08d50bf05cd0a63d1567b4f475eb701ff51a2907`, which passed its complete native
  Linux, macOS, and FreeBSD matrix in
  [`fork` run 29318936207](https://github.com/immortal/fork/actions/runs/29318936207).
- Immortal commit `b1d17997a5fbd782c69b7e4145a92db405bc53ce`, locked to the
  crates.io `fork` 0.9.1 release and checksum, passed its complete native Linux,
  macOS, and FreeBSD matrix, lifecycle benchmarks, version checks, and FreeBSD
  cross-check in
  [Rust CI run 29319666973](https://github.com/immortal/immortal/actions/runs/29319666973).
  Its audit and dependency-policy jobs passed in
  [security run 29319666972](https://github.com/immortal/immortal/actions/runs/29319666972).
  The exact checkpoint passed both bounded parser fuzz targets in
  [run 29320496890](https://github.com/immortal/immortal/actions/runs/29320496890).
- As retained historical regression evidence, an independent amd64 FreeBSD
  15.1-RELEASE-p1 host ran the workspace suite twice at Immortal commit
  `e735120c4fca7686d3f8b758b9e81fe54232ac87`, with clean outcomes in 43 and 42
  seconds. The schema-validated records and SHA-256 manifest are retained in
  [`validation/evidence/freebsd-15.1-e735120`](validation/evidence/freebsd-15.1-e735120).

This evidence proves the repository contracts above. It does not replace the
24-hour campaigns, seven-day canary, comparative runs, or release drills.

The complete public CLI, strict configuration, control, status, and private
broker field inventory is mapped to its success and failure coverage in
[`TRACEABILITY.md`](TRACEABILITY.md). That audit establishes test ownership; it
does not change any Pending real-host or duration gate in this document.

## Comparative reference set

The common comparison set is Immortal, runit, daemontools, and s6. systemd is
measured only on Linux. Unsupported platform combinations are recorded as
unsupported; competitors are not locally patched to manufacture parity.

Only common daemon-supervision behavior is timed:

- foreground execution and successful exec acknowledgement;
- readiness where the supervisor provides an equivalent contract;
- crash detection and restart;
- status, signal, stop, restart, and shutdown;
- lossless pipe-based logging;
- one, 100, and 1,000 independently supervised definitions where supported.

Feature review uses **proven**, **missing**, **out of scope**, or
**platform-specific**. It does not assign a winner by counting features.
Portable liveness monitoring, resource limits, synchronous reload completion,
broker-death containment, and high-cardinality operation require explicit
review before any new configuration field is proposed.

## Performance evidence

The existing dependency-free harnesses provide internal configuration, codec,
spawn/reap, and signal/reap measurements. Comparative work must additionally
record:

- idle RSS, CPU, descriptors, and processes per service;
- exec and readiness latency;
- crash-to-ready recovery latency;
- control-operation latency;
- logger throughput, backpressure, and exact byte count;
- reconciliation latency at one, 100, and 1,000 definitions;
- shutdown latency and residual descendants;
- descriptor, process, and memory trends across prolonged restart cycles.

Run competitors in randomized order on the same otherwise idle host. Record at
least 30 independent comparative runs across three days. Internal Immortal
regression ceilings retain the ten-run, three-day rule in `README.md`; an
accepted ceiling is no lower than 125% of the observed maximum and remains
platform-specific.

Machine-readable result records are UTF-8 tab-separated rows with this header:

```text
schema_version	commit	platform	kernel	cpu	toolchain	supervisor	supervisor_version	scenario	sample	metric	value	unit	outcome	cleanup
```

Schema version `1` requires every column. Numeric values use base-ten ASCII and
an invariant decimal point when fractional. `outcome` is `pass` or `fail`;
`cleanup` is `clean` or a bounded diagnostic. Raw artifacts are retained with a
SHA-256 manifest. Reviewed summaries link those artifacts rather than copying
selected results into documentation.

## Validation tiers

### Pull requests

- DevPod `just ci` and the locked FreeBSD cross-check;
- native Linux, macOS, and FreeBSD lifecycle jobs;
- short bounded configuration and control fuzzing;
- security and dependency policy.

### Nightly and manual campaigns

- extended fuzzing and deterministic mutation corpora;
- repeated lifecycle and fault campaigns;
- descriptor, child, runtime-entry, and memory trend collection;
- retained benchmark artifacts with complete metadata.

### Release candidate

A release candidate must pass all existing requirements in `RELEASE.md` plus:

1. a 24-hour fault and soak campaign on real Linux, macOS, and FreeBSD hosts;
2. a seven-day FreeBSD canary after the 24-hour campaigns;
3. zero wrong-target signals, duplicate live generations, owned zombies,
   unmanaged owned groups, corrupt checkpoints, or unbounded waits;
4. exact cleanup of owned descriptors, children, groups, sockets, locks, PID
   files, runtime directories, and temporary definitions;
5. reviewed performance and memory trends without unexplained regression;
6. successful install, boot, shutdown, strict-v2 migration, upgrade, and
   rollback drills.

These campaigns run on maintainer-controlled hosts, not as fake long sleeps in
GitHub Actions. Their scripts may automate collection, but a human must review
the complete evidence before changing a requirement to Proven or Observed.

The repository runner repeatedly executes every workspace contract and emits
schema-checked evidence plus a SHA-256 manifest. It is one input to a real-host
campaign; by itself it does not inject every fault, collect resource trends, or
satisfy the release gate. Run this contract-soak component from a clean
candidate worktree with a report path outside the repository:

```sh
scripts/dev-ssh just validation-campaign 86400 /tmp/immortal-validation-24h
```

Use `604800` seconds for the contract-soak component of the FreeBSD canary.
Validate any copied result before review:

```sh
scripts/dev-ssh just validation-evidence /tmp/immortal-validation-24h/results.tsv
```

## Finding priority

- **P0**: ownership, cleanup, authorization, resource-bound, or recovery
  invariant violation. It blocks other feature work and release.
- **P1**: a missing portable supervision primitive which operators cannot
  compose safely outside Immortal. It requires a focused design and contracts.
- **P2**: convenience or observability work without correctness impact.
- **Out of scope**: PID 1, Linux cgroup parity, timers, mounts, device
  management, and general systemd unit compatibility.

No universal “better than systemd, runit, daemontools, or s6” statement follows
from this document. A published comparison must name the category, platform,
workload, versions, commit, and retained evidence which support it.
