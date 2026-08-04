# Public contract traceability

This audit maps every currently supported public input and output surface to
tests which exercise its syntax or codec and its observable behavior. A parser
round trip alone is not behavioral evidence: process, filesystem, lifecycle,
and authorization claims also name a contract executable or focused failure
test. The release evidence and platform state remain governed by
[`VALIDATION.md`](VALIDATION.md).

## Command-line interfaces

| Executable | Public surface | Syntax and typed-dispatch tests | Behavioral and failure tests |
|---|---|---|---|
| `immortal` | `-f`/`--foreground`, `-n`/`--name`, `-l`/`--logfile`, exact variadic `--logger` with `--` separator, `--retries`, `--check-config`, `--child-pid`, `-c`/`--config`, `--control-dir`, automatic config-stem identity, `-e`/`--env-dir`, `--supervisor-pid`, `--user`, `--working-dir`, `--wait`, command argv, help and version | `immortal::cli::commands` tests including `exact_logger_argv_and_child_argv_are_separated_by_double_dash` and conflict/terminator failures; dispatch tests including `captures_supported_direct_options`; `carries_an_exact_control_directory_for_commands_and_configs`; `preserves_direct_argv`; `selects_config_check` | `foreground_contract` direct local-plus-centralized logging, automatic-root/name/stem, symlinked-home, and unsafe-root contracts; `controlled_contract`; trusted-home, portable socket-path, bounded environment-directory, configuration, PID-file, account, daemon-startup, and process tests in `immortal-core` |
| `immortalctl` | automatic or exact runtime discovery; `--runtime-scope`; table/JSON, color, and header selection; lifecycle timeout and no-wait; status, start, stop, restart, once, exit, halt, signal; main/group scope; service, `--all`, and released signal flags | `immortalctl::cli::commands` tests; `immortalctl::cli::dispatch` tests | `immortalctl::cli::actions` discovery, transport, rendering, generation-binding, timeout, and completion tests; `routing_contract`; `controlled_contract` |
| `immortaldir` | definitions directory, runtime directory, scan interval, concurrent-start limit, supervisor binary, once, dry-run, environment overrides, help and version | `immortaldir::cli::commands` and `immortaldir::cli::dispatch` tests | `immortaldir::cli::actions` tests; `routing_contract`; `operational_contract`; reconciliation and watch tests in `immortal-core` |
| Broker process-group isolation | terminal interrupt delivered to the supervisor's whole process group | — | `foreground_contract` `prove_terminal_interrupt_runs_ordered_shutdown`, proving the trapped service `SIGTERM` still runs instead of a guard kill |
| Stale generation events | reaps and readiness for generations the supervisor already issued but no longer runs | `a_reap_after_spawn_failure_is_stale_rather_than_a_fault`; `a_reap_after_terminal_failure_is_still_stale`; `a_reap_for_an_unissued_generation_is_a_fault` | `broker_dispatch_absorbs_a_reap_after_spawn_failure`; `broker_dispatch_rejects_a_reap_for_an_unissued_generation` |
| `immortaldir` definitions trust | real directory, stable identity, no group or world write, owner `root` or the effective user | `rejects_a_symlinked_definitions_directory`; `rejects_a_writable_definitions_directory`; `accepts_an_owner_only_definitions_directory` | `canonicalizes_a_real_definitions_directory`; `routing_contract`; `operational_contract` |
| `immortallog` | destination file, maximum bytes, maximum age, retained archive count, total archive bytes, timestamp, passthrough; `archives` live-file namespace with table/JSON output; help and version | `immortallog::cli::commands` tests; dispatch tests including `preserves_rotation_and_stream_flags` and `captures_archive_namespace_and_output_format` | `immortallog::cli::actions` stream, UTC, table/JSON, and failure tests; write/archive `archives_contract`; logging archive ownership, ordering, rotation, retention, and `file_adapter_contract` tests in `immortal-core` |

Short and long version behavior is also executed for every supervisor-facing
binary by the native Linux, macOS, and FreeBSD CI jobs. CLI conflicts, missing
values, zero or excessive numeric bounds, removed options, and option-looking
child arguments are covered by the named command tests.

## Strict configuration version 2

The following rows cover the complete schema shown in `README.md`. Serde denies
unknown fields at every document level; mutation, depth, alias, size, UTF-8,
duplicate-key, multi-document, missing-version, and unsupported-version tests
exercise the common decoder boundary.

| Configuration group | Fields and values | Parse and validation evidence | Operational evidence |
|---|---|---|---|
| Service | `version`, `enabled`, `command`, `working_directory`, canonical `environment` with `env` input alias, `environment_mode`, `user`, `start_delay_seconds`, `requires` | `supported_configuration_round_trips`; `env_alias_normalizes_scalars_and_emits_canonical_environment`; alias, schema, and path-resolution rejection tests; `validates_commands_dependencies_and_descriptor_tracking`; start-condition parser tests; `dependency_plan_groups_independent_services`; `dependency_plan_isolates_missing_disabled_and_cyclic_services`; `dependency_plan_isolates_cycles_and_self_requirements` | `foreground_contract`; `operational_contract` dependency-isolation contract proving an unresolvable `requires` skips only its dependents while unrelated starts and pending stops still complete; account, environment, dependency-plan, and delayed-start contracts |
| Restart | `policy`, `success_exit_codes`, `exit_when_done`; `limits.max_retries`, `max_elapsed_seconds`, `burst.starts`, `burst.window_seconds`; backoff initial, maximum, multiplier, jitter, and stable-reset values | `parses_strict_v2_restart_and_readiness_policy`; logger backoff parser tests; invalid-value validation corpus | supervisor retry and exhausted-failure tests; foreground exit-after-retries contract; issue 71, elapsed/burst, stable-reset, condition-isolation, logger-retry, and executor contracts |
| Readiness and hooks | readiness mode and timeout; start-condition command, timeout, and backoff; post-exit command and timeout | readiness and start-condition parser tests; hook validation and path-resolution tests | readiness unit/broker/foreground contracts; condition retry/timeout/shutdown contracts; complete post-exit contract family |
| Logging | combined `log.file`; strict `log.stdout`/`log.stderr`; file `age`, `keep`, `size`, and `timestamp`; one combined `logger` argv; `log_adapter`; `logger_restart`; deprecated `num`, top-level `stderr`, and representable `logging` inputs | `logging_schema_normalizes_combined_and_selected_routes`; duration/size unit and canonical-emission tests; direct-default, alias, conflict, bound, path, unknown-field, and unrepresentable-migration tests | logger restart/drain/file-adapter contracts; selected and split local-file fan-in to one logger; foreground direct local-plus-centralized delivery, permission, backpressure, and drain-timeout contracts; executor staged-tier test; rotation, sync, retention, broken-pipe, and partial-line tests |
| Process metadata and mode | supervisor/main PID files; foreground or descriptor-tracking mode; stop/reload command and timeout; lifetime timeout | PID and descriptor configuration/path tests; partial, misplaced, unknown, zero, and excessive descriptor-field rejection | PID atomicity/replacement tests; foreground PID lifecycle; descriptor lifetime, control, hook, shutdown, and broker-loss contracts |

Defaults are exercised by minimal definitions and explicit default assertions;
canonical emission is reparsed and compared as a complete `ServiceConfig`.
Fuzzing targets the same public configuration byte boundary.

## Control and status protocol

| Frame | Public fields | Codec and bound evidence | Semantic and transport evidence |
|---|---|---|---|
| Request | protocol version; operation; service; expected generation (`any`, no child, exact); signal scope; optional signal | `every_lifecycle_operation_round_trips`; `all_signal_names_round_trip`; unknown version/operation/scope/signal, unsafe name, inconsistent signal, generation, truncation, trailing data, and size tests | `decide_request` state-machine tests; authenticated transport tests; `controlled_contract`; generation-bound `immortalctl` action tests |
| Response | response code; optional generation; bounded message; optional typed status | response-code and typed-status round trips; malformed, unknown, truncated, oversized, and excessive-argument tests | transport round trip, disconnect, timeout, bad-client isolation, and controlled lifecycle contracts |
| Status | supervisor/main PID; desired and observed state; readiness; up/down duration; starts; failures; last result; backoff; logger health; command argv | `typed_status_payload_round_trips_every_field`; status enum/code tests | pre-child status, state-machine, controlled lifecycle, discovery, and table/JSON rendering tests |
| Local socket | runtime path ownership; peer UID/GID/PID; active-client limit; read/write/accept deadlines; transient-versus-fatal accept classification | listener path, mode, peer-policy, cleanup, timeout, client-limit, and `accept_classification_separates_peer_and_resource_faults_from_listener_faults` tests | runtime ownership/discovery tests; server bad-client isolation; `server_loop_survives_aborted_peers`; `server_loop_still_stops_when_the_supervisor_is_gone`; `resource_fault_contract` control accept-exhaustion recovery; black-box authenticated control contracts |

The private broker protocol is not a public compatibility surface, but its
version, generation and task identities, argv/environment/path bytes,
credentials, descriptor plans, logging plans, requests, events, adopted-orphan
reaping, truncation, bounds, and unknown values are covered by `broker_protocol`
unit tests and the broker, subreaper-orphan, process, resource-fault, and
supervisor-loss contract executables.

## Maintenance rule

A new CLI option, configuration field, control/status field, or public enum
value must update this audit in the same change and add both successful and
failing coverage. A row may cite a family of tests only while every field in
that row remains exercised; otherwise split the row and leave the uncovered
contract explicitly pending in `VALIDATION.md`.
