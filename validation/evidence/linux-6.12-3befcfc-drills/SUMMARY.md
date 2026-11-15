# Linux release drills for candidate 3befcfc

Host: dedicated VM `immortal-linux`, Debian 13, Linux 6.12.113 x86_64, 2 vCPU,
systemd as PID 1, rustc 1.97.0. Date: 2026-11-05 (UTC). Every step is a
traced shell script under `steps/` whose commands, output, and exit status are
in the matching `.log`. Candidate `3befcfc4b01b3c66a18624229795d0e1301e89a6`;
previous candidate `43a7a6eaf05838efadb58cfbd6ef73cd6c3756c9` stands in for
the installed version that upgrade replaces and rollback restores. Both were
built with `cargo build --workspace --release --locked` from clean checkouts.

| Step | Drill | Result |
|---|---|---|
| 00 | Build candidate release artifacts | pass |
| 02 | RELEASE.md 5-7: short/long versions, `scripts/rehearse-upgrade`, no leftovers | pass |
| 03 | Strict-v2 migration: Go definitions rejected (exit 78), converted v2 accepted, dry-run plan, deprecated logging aliases | pass (attempt 1: drill setup lacked the working directory) |
| 05, 06 | Dry-run preview versus a real one-shot pass with an invalid definition | finding A |
| 07 | Build previous candidate | pass |
| 10 | Install 43a7a6e with INSTALL.md commands and the shipped systemd unit | attempt 1 failed: finding B |
| 11 | Install `immortallog` beside `immortal`; stuck supervisor recovers without restart | pass |
| 12 | Upgrade to 3befcfc: manager-only restart keeps every supervisor; new client controls old supervisors; full cutover onto the candidate binary | pass |
| 13 | Shutdown: unit stop keeps supervisors; `halt --all` leaves no process, socket, or open log | pass |
| 14-16 | Boot: reboot, enabled unit recreates `/run/immortal` and restores both services; shutdown journal | pass; observation C |
| 17 | Rollback to 43a7a6e per INSTALL.md: halt, preserve definitions, remove only proven-inactive runtime entries, restore | pass |
| 18 | Uninstall every drill file; VM restored | pass |

## Findings

A. `immortaldir --once --dry-run DIR`, which INSTALL.md gives for validating
migrated definitions, exits 0 when a definition is invalid or malformed; the
problem appears only on stderr. A real `--once` pass also exits 0 for an
isolated invalid definition. The dry-run plan also prints `START` for a
service whose requirement is invalid, while the real pass skips it with
`requires unavailable service` and exits 79. README states that dry-run
"performs the same bounded scan and planning". No wrong process starts.
Decision (maintainer, 2026-11-05): correct the documentation for this
candidate; make the exit status and dependency preview match in the next
version.

B. INSTALL.md installs `immortallog` into `/usr/local/bin`, but a supervisor
without `log_adapter` runs the default adapter from its own directory
(`crates/immortal-core/src/executor/prepare.rs`), here
`/usr/local/sbin/immortallog`. Following INSTALL.md exactly, a service with
`log.file` never starts: its logger stays in backoff and status shows
`initializing` with no reason. Installing `immortallog` beside `immortal`
fixed it within three seconds without a restart. Documentation fix: install
`immortallog` next to `immortal` and state the default adapter lookup.

## Observations

C. With `KillMode=process`, systemd stops only the manager at shutdown; the
supervisors stay in the stopped unit's cgroup and stop on the final shutdown
SIGTERM. They exited at once (the whole shutdown took under a second), but
services therefore stop after the network and other units rather than in unit
order.

D. While a logger cannot start, neither `immortalctl status` nor the journal
names the cause; `immortaldir` reports only `lifecycle deadline exceeded`.

E. `<defunct>` processes in the snapshots are the fork 0.11.0 process-group
anchors, which stay unreaped for the life of each guarded group by design.
