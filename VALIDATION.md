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

Requirement identifiers are grouped by prefix: COR (correctness), RES
(resilience), and SEC (security).

| ID | Requirement | Evidence | Platforms | State |
|---|---|---|---|---|
| COR-001 | A service generation is identified independently of a reusable PID. | Supervisor, control, and broker generation contracts | All | Proven |
| COR-002 | Stop, restart, and halt clean the owned process group before generation reuse. | Fork-backed process and controlled lifecycle contracts | All | Proven |
| COR-003 | Exec failure is distinct from successful execution and never publishes Ready. | Broker and executor contracts | All | Proven |
| COR-004 | Configuration, control, broker, and status inputs are bounded and fail closed. | Unit mutation corpora, fuzz workflows, and protocol contracts | All | Proven |
| COR-005 | Unexpected broker death cannot create a duplicate replacement or leave a live member in an owned process group. | `broker_death_contract` plus `fork` group-guard contracts | All | Proven |
| COR-006 | Deliberate process-group or session escape is never misrepresented as portable containment. | Documented limitation and `fork` escape fixture | All | Proven |
| COR-007 | An orphaned descendant adopted after process-group or session escape is reaped as hygiene and never becomes a workload event or aborts the broker. | `broker_subreaper_orphan_contract` and `fork` subreaper acquisition | All | Proven |
| COR-008 | Every group the broker kills, owns, or releases drains before the broker exits; armed-group signals are delivered by the helper while it holds the anchor (helper loss fails without direct delivery); unarmed groups are signalled only through a verified unreaped member leader's pin, and any other group is awaited, never signalled; helper exchanges and foreground-reap kills share phase deadlines, preserving the five-second ordered-shutdown signal/settle budget; an unacknowledged foreground kill closes the guard fail-closed rather than disarming it; a group it cannot prove empty fails closed. | `broker_signal_budget_contract` (Linux stopped-helper sweeps, foreground reaps, and a TERM-ignoring sibling after broker exit), `broker_retiring_group_contract`, `broker_lifetime_release_contract`, residual-child checks in `broker_subreaper_orphan_contract` and `broker_supervisor_loss_contract`, and `a_nonleader_membership_snapshot_never_authorizes_direct_group_delivery`, `another_nonleader_membership_snapshot_is_not_a_pin`, `armed_group_signals_never_use_direct_delivery`, `helper_gone_kill_retires_without_direct_delivery`, and the `fork` 0.11.0 helper-mediated signal, stopped-helper/anchor, and group probe contracts | All | Proven |
| COR-009 | Supervisor startup owns waitable children before forking, and broker deadline cleanup never signals after policy or exact-child ownership loss. | `waitable_policy_contract`, fork `waitable_children` and `sigchld_ignored`; fixture ownership/release regression in `fixture_ownership_contract` | Shared production code; preload schedules GNU/Linux only | Proven |
| RES-001 | Supervisor loss makes the broker stop and clean every owned group. | Broker supervisor-loss contract | All | Proven |
| RES-002 | Lost or coalesced child notifications cannot leave an owned zombie. | Delayed reap sweep and native lifecycle contracts | All | Proven |
| RES-003 | Logger failure, backpressure, and shutdown preserve the configured lossless contract. | Logger restart, file-adapter, drain, and foreground contracts | All | Proven |
| RES-004 | Interrupted reconciliation retains last-known-good state and retries independent failures. | Reconcile unit and operational contracts | All | Proven |
| RES-005 | Signal storms, control-client saturation, descriptor exhaustion, and interrupted system calls remain bounded. | `resource_fault_contract`, `resource_soak_contract`, `resource_binary_soak_contract`, bounded control-listener and `fork` EINTR contracts, plus the adversarial resource campaign | All | Pending |
| SEC-001 | Runtime discovery and control authenticate ownership without following unsafe filesystem entries. | Runtime and control contracts | All | Proven |
| SEC-002 | Dependencies pass audit, license, source, and duplicate-version policy. | `cargo audit` and `cargo deny --all-features check` | All | Proven |

`Proven` describes repository contracts, not production readiness. A candidate
must still pass the real-host gates below for its exact commit.

Contract scratch space must survive a host reboot when used for release evidence:
set `TMPDIR` under `/var/tmp`. Fixtures that deliberately need a short root for
Unix-socket length limits use `/var/tmp` directly, including their parent-side
rescue lookup; stderr captures honor `TMPDIR`. No fixture should force writes to
a reboot-volatile `/tmp` regardless of the configured scratch directory.

COR-005 is deliberately bounded by COR-006. A process which creates a new
session or joins another process group has escaped the portable ownership unit;
the project must expose that limitation rather than claiming cgroup- or
subreaper-equivalent containment. COR-007 is the complementary hygiene
guarantee: where the platform provides the role the broker adopts and reaps such
escaped descendants once they are orphaned, so their zombies never leak to init,
but it neither signals nor tracks them as owned work. The released `fork` 0.10.0
crate passes its running, stopped, empty-startup, descriptor-isolation,
subreaper-acquisition, and cleanup contracts on native Linux, macOS, and
FreeBSD, including an explicit session-escape fixture; Linux and FreeBSD acquire
the child-subreaper role while macOS retains init reparenting. The released
`fork` 0.11.0 crate, built from fork commit
`bf240d69e96b6f5f3ee58861307cdbb623f02b07`, passed its native Linux, macOS,
FreeBSD, and musl matrix, coverage, and minor-release API check in
[fork CI run 39908664585](https://github.com/immortal/fork/actions/runs/39908664585).
The combined Immortal candidate passes its broker-death and complete workspace
contracts on the same three native platforms.

COR-008 closes the gap the FreeBSD 24-hour campaign exposed at candidate
`dded7ad`: a member of a group the broker had killed could still be exiting when
ordered shutdown completed, and then reparented to the supervisor. Its contracts
fail on the tree before the retiring set and pass with it. Immortal now locks
the exact crates.io `fork` 0.11.0 release and checksum
`9b8b9dc1312e1668be5ffa350e3c0adc9b501b2eeebb1b31206477240cc96554`; no Git or
local patch overrides the reviewed source. Immortal commit
`07971c002e05f385d69ad6f2c17605639f3a6f36`, locked to that release, passed its
native Linux, macOS, and FreeBSD lifecycle jobs, lint, and FreeBSD compile check
in [Rust CI run 39927145070](https://github.com/immortal/immortal/actions/runs/39927145070),
with audit and dependency policy in
[security run 39927145120](https://github.com/immortal/immortal/actions/runs/39927145120)
and both bounded parser fuzz targets in
[fuzz run 39927145349](https://github.com/immortal/immortal/actions/runs/39927145349).

COR-009 includes the inherited-exec ignored-SIGCHLD regression, a broker exiting
at its cleanup deadline, explicit post-creation policy/collection violations,
and policy-reset failure. The fixture regression also exercises early-return
unwinding with an exited broker and surviving member, and modeled group reuse.
`broker_lifetime_release_contract::failed_completion_releases_delayed_member`
additionally rejects launcher exit 73 before successful completion while delaying
the member's first release poll. Release ownership is installed before startup;
member guards are installed before fallible completion checks. Lifetime, retiring
and supervisor-loss fixtures retain release directories through descendant
collection and recorded-group absence (or retain them on a cleanup timeout), and
shell handshakes have independent 20-second deadlines. This closes the transient
release-marker gap that an already-installed-member-guard test did not exercise.
Linux interposition records/suppresses unsafe deliveries; it does not claim
actual PID recycling. Native macOS/FreeBSD execution of this candidate remains
required; cross-checking alone does not establish that runtime evidence.

RES-005 has deterministic contracts for a bounded stop/continue storm,
descriptor exhaustion before broker creation, active control-client limits,
and interrupted waits. Those contracts pass natively on all three platforms.
Two restart-storm soaks additionally exercise sustained fault load at different
boundaries. `resource_soak_contract` drives a real forked broker through a
bounded spawn, signal, and reap storm while sampling the broker's own resident
memory, open descriptors, and surviving children at rest.
`resource_binary_soak_contract` launches the full `immortal --foreground` binary
against a throwaway restart-always service whose child exits at once, so the
supervisor drives its own continuous fork, exec, and reap storm; it samples the
supervisor process's footprint throughout and then confirms a graceful signal
produces a clean exit with no leftover control socket. Both fail on growth past a
generous absolute tolerance, a retained child, or an unclean teardown, and record
schema-checked resource trends when an `IMMORTAL_SOAK_*` or `IMMORTAL_BINSOAK_*`
override scales them into a long run. Both pass natively on Linux, macOS, and
FreeBSD.

At Immortal commit `43a7a6eaf05838efadb58cfbd6ef73cd6c3756c9` the 24-hour
resource campaign, 12 hours per boundary, passed on the dedicated Linux and
FreeBSD hosts described under the current candidate evidence. Every record
passed with clean cleanup: 65 at-rest broker samples per host, and 167,753
(Linux) and 170,475 (FreeBSD) samples of the supervisor under its restart storm.
The broker never retained a child and held a constant 10 (Linux) or 13 (FreeBSD)
descriptors, while its resident memory stayed between 2,820 and 2,836 KiB and
between 5,216 and 5,220 KiB. The supervisor held exactly its one service child
and a constant 12 or 15 descriptors, while its resident memory stayed between
11,676 and 11,740 KiB and between 11,440 and 11,464 KiB. RES-005 remains Pending
until the same campaign passes on a real macOS host.

### Current candidate evidence

The 2026-07-14 and 2026-07-18 reviews bind the process-containment claims to
exact revisions:

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
- The child-subreaper wiring (COR-007, `broker_subreaper_orphan_contract`) locks
  the candidate to the crates.io `fork` 0.10.0 release, checksum
  `aafa2fb0e6b3b0886eb36b909c13a5e3dc9d7e0cf436376e43f095177050c029`. Immortal
  commit `69cfa3c15222872924bfb639a0f5dd7357c53561` passed its complete native
  Linux, macOS, and FreeBSD matrix, lifecycle benchmarks, version checks, and
  FreeBSD cross-check in
  [Rust CI run 29658374195](https://github.com/immortal/immortal/actions/runs/29658374195).
  Its audit and dependency-policy jobs passed in
  [security run 29658374231](https://github.com/immortal/immortal/actions/runs/29658374231),
  and both bounded parser fuzz targets passed in
  [fuzz run 29658374252](https://github.com/immortal/immortal/actions/runs/29658374252).
  The workspace suite runs `broker_subreaper_orphan_contract` on all three native
  platforms, so COR-007 is Proven at that commit.
- The current candidate, Immortal commit
  `6e261f815cad5a75ad4c38ad38931bc8a2b5caad`, carries the pre-release
  correctness work: cancellation-safe broker request decoding, validation of
  materialized direct commands, isolation of unresolvable dependencies,
  readiness acceptance for any owned generation, an accept loop which absorbs
  transient failures, broker process-group isolation, a bounded restart
  history, a trusted definitions directory, and permission-denied answers for
  unauthorized control peers. It passed its complete native Linux, macOS, and
  FreeBSD matrix, lifecycle benchmarks, version checks, and FreeBSD cross-check
  in
  [Rust CI run 30938300095](https://github.com/immortal/immortal/actions/runs/30938300095).
  Its audit and dependency-policy jobs passed in
  [security run 30938297498](https://github.com/immortal/immortal/actions/runs/30938297498).
  Both bounded parser fuzz targets passed at the immediately preceding commit
  `9375a5d` in
  [fuzz run 30935891204](https://github.com/immortal/immortal/actions/runs/30935891204);
  the fuzzing workflow is path filtered and the only change since that commit is
  a test cleanup path, which no fuzz target reaches. A clean DevPod run of the
  workspace suite left no child, process group, socket, PID file, runtime
  directory, or temporary definition behind. Rows whose evidence is the
  repository suite are therefore Proven at this commit; every real-host,
  duration, and comparative gate below stays Pending.
- Immortal commit `43a7a6eaf05838efadb58cfbd6ef73cd6c3756c9`, locked to `fork`
  0.11.0 as recorded under COR-008, completed the 24-hour campaign on two
  dedicated 2-vCPU virtual machines, each building with rustc 1.97.0: Debian 13
  with Linux 6.12.113 on x86_64, and FreeBSD 15.1-RELEASE-p5 on amd64. The
  contract-soak component ran the complete workspace suite back to back for 24
  hours, with 703 clean passing samples on Linux and 742 on FreeBSD, and the
  resource campaign recorded under RES-005 followed on each host. The
  schema-validated records, metadata, and SHA-256 manifests are retained in
  [`linux-6.12-43a7a6e-validation`](validation/evidence/linux-6.12-43a7a6e-validation),
  [`linux-6.12-43a7a6e-resource`](validation/evidence/linux-6.12-43a7a6e-resource),
  [`freebsd-15.1-43a7a6e-validation`](validation/evidence/freebsd-15.1-43a7a6e-validation),
  and
  [`freebsd-15.1-43a7a6e-resource`](validation/evidence/freebsd-15.1-43a7a6e-resource).
  The supervisor-boundary records, about 100 MB each, are retained
  xz-compressed; decompressing `binary-results.tsv.xz` restores the exact file
  that its manifest covers. The seven-day FreeBSD canary that followed failed
  at contract-soak sample 262: the `operational_contract` cross-restart poll
  read the deletion tracker while the manager atomically replaced it, and the
  snapshot reader correctly rejected the mid-read replacement. No production
  reader runs beside that writer. The next candidate retries that test-only
  race and adds a regression which fails without the retry, and every
  real-host gate restarts at it.

Apart from the Linux and FreeBSD 24-hour campaigns recorded above, this evidence
proves the repository contracts. It does not replace the macOS 24-hour campaign,
seven-day canary, comparative runs, or release drills.

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
regression ceilings are accepted per platform from at least ten retained
benchmark runs spread across at least three separate days at one commit. An
accepted ceiling is no lower than 125% of the maximum observed across those
runs, and a ceiling accepted on one platform never carries to another. Fewer
runs, a single day, or a mixed set of commits does not establish a ceiling.

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

- `just ci` on a dedicated VM or in DevPod, and the locked FreeBSD cross-check;
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
Collect them natively on the target host or a dedicated VM, not in a container:
the evidence metadata records the running kernel, so a container run reports
its virtual machine's platform rather than the host's.

The repository runner repeatedly executes every workspace contract and emits
schema-checked evidence plus a SHA-256 manifest. It is one input to a real-host
campaign; by itself it does not inject every fault, collect resource trends, or
satisfy the release gate. Run this contract-soak component from a clean
candidate worktree with a report path outside the repository:

```sh
just validation-campaign 86400 /tmp/immortal-validation-24h
```

Use `604800` seconds for the contract-soak component of the FreeBSD canary.
Validate any copied result before review:

```sh
just validation-evidence /tmp/immortal-validation-24h/results.tsv
```

A focused single-supervisor resource soak complements that broad runner. It
divides its duration between two boundaries: the broker soak drives one forked
broker through a sustained restart storm, and the binary soak drives a full
`immortal --foreground` supervisor through a self-driven restart storm. Both
sample the supervised process's own resident memory, descriptors, and children,
so their evidence isolates supervisor resource trends from the compiler and
harness. Run it from a clean candidate worktree with a report path outside the
repository:

```sh
just resource-soak 86400 /tmp/immortal-resource-soak-24h
```

It writes and self-validates schema-checked evidence for both boundaries with a
SHA-256 manifest and fails on any unbounded trend or leftover. Use `604800`
seconds for the FreeBSD canary, and review the retained trend before changing
RES-005.

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
