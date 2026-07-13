# Installing an Immortal release candidate

Immortal is not yet a production release. These instructions install a reviewed
candidate for native testing on Linux, macOS, or FreeBSD; they do not bypass the
release gates in [RELEASE.md](RELEASE.md).

## Build and install

Build the complete version-locked workspace in DevPod:

```sh
scripts/dev-up
scripts/dev-ssh just ci
scripts/dev-ssh cargo build --workspace --release --locked
```

Install all four executables from the same commit. `immortal`, `immortaldir`,
and usually `immortallog` belong in the system administrator path;
`immortalctl` may also be installed in a normal user path.

```sh
install -m 0755 target/release/immortal /usr/local/sbin/immortal
install -m 0755 target/release/immortaldir /usr/local/sbin/immortaldir
install -m 0755 target/release/immortalctl /usr/local/bin/immortalctl
install -m 0755 target/release/immortallog /usr/local/bin/immortallog
```

Confirm that every long version reports the same package version and source
revision before starting services.

## Definition migration

The Rust implementation accepts only strict `version: 2` definitions. It does
not read the unversioned Go format. Keep the old installation stopped but
available for rollback, convert definitions into a separate directory, and
validate that directory without mutating runtime state:

```sh
immortaldir --once --dry-run /usr/local/etc/immortal
```

The format change is not a field-renaming exercise. The Rust supervisor uses
owned children, process groups, monotonic generations, bounded hooks, and exact
argv. A legacy reader would otherwise imply behavioral compatibility with
contracts which were deliberately replaced.

| Go definition | Version 2 definition | Migration decision |
|---|---|---|
| `cmd` scalar | `command` argv | Split arguments explicitly; add `/bin/sh -c` only when shell evaluation is intentional. |
| `cwd`, `env`, `user` | `working_directory`, `environment`, `user` | Select `environment_mode: inherit` or `clear` deliberately. |
| `wait` | `start_delay_seconds` and restart backoff | The old delay applied before every launch; v2 separates the first start from crash-loop delay. |
| `retries` | `restart` policy and limits | Review success codes, terminal behavior, and retry, elapsed, or burst limits. |
| `require` | `requires` plus readiness | Dependencies are validated as a graph and gate on Ready. |
| `require_cmd` | `start_condition` | Use exact argv, a deadline, and independent backoff instead of an implicit unbounded shell command. |
| `post_exit` | `post_exit` command hook | Use exact argv and a deadline; read result context from the documented environment. |
| `log`, `stderr`, `logger` | structured `logging` routes | Rotation sizes are bytes; file adapters and external loggers are supervised processes. |
| `pid.parent`, `pid.child` | `pid_files.supervisor`, `pid_files.main` | These files are atomic observation output, never process identity. |
| `pid.follow` | foreground or descriptor tracking | There is intentionally no PID-adoption translation. |

The Go `pid.follow` path read a numeric PID written by the application and then
treated that number as the live service. A stale or replaced file and PID reuse
can redirect observation or a signal to an unrelated process, and a
self-daemonized process is no longer an owned child which Immortal can reliably
wait for and reap. Version 2 therefore keeps normal services as broker-owned
foreground children. Control requests name a monotonic generation, while the
broker resolves the actual child and process group.

Applications which cannot remain in the foreground must use the explicit
descriptor-tracking contract. Immortal gives the launched generation
`IMMORTAL_LIFETIME_FD`; application forks inherit that capability, and its final
close—not a PID-file value—ends the logical generation. Because no background
PID is adopted, bounded stop and reload hooks provide application control. See
the complete [descriptor-tracking example and lifecycle](README.md#configuration-schema).

A representative legacy definition:

```yaml
cmd: /usr/local/bin/api --foreground
cwd: /srv/api
env:
  APP_ENV: production
wait: 2
retries: 3
log:
  file: /var/log/api.log
  age: 86400
  num: 7
  size: 1
```

becomes an explicit version 2 definition:

```yaml
version: 2
command: [/usr/local/bin/api, --foreground]
working_directory: /srv/api
environment:
  APP_ENV: production
environment_mode: inherit
start_delay_seconds: 2
restart:
  policy: always
  limits:
    max_retries: 3
logging:
  combine_stderr: true
  stdout:
    file:
      file: /var/log/api.log
      max_age_seconds: 86400
      keep: 7
      max_bytes: 1048576
```

`immortal --check-config` validates and emits canonical v2 only; it does not
guess at legacy intent or modify its input. Review each rewritten definition
before placing it in the candidate directory.

The Rust CLI also does not accept the historical direct-command `--env-dir`,
`--follow-pid`, `--log-file`, `--logger`, or `--name` flags. Express environment
and logging policy in the v2 definition. Use the definition filename with
`immortaldir`, or an explicit `--control-dir` for a direct command, as the
service's managed identity. There is no PID-following replacement; use a
foreground command or the descriptor-tracking contract described above.

Before migrating local definitions, the repository example provides a bounded
rehearsal which must print canonical configuration followed by a `START` plan:

```sh
immortal --config examples/services/sleep.yml --check-config
immortaldir --once --dry-run examples/services
```

Treat the runtime directory as disposable observation state, not configuration.
Never copy sockets, locks, PID files, or `.definitions` snapshots from the Go
installation. Do not run old and new directory managers against the same
definitions or runtime root.

## FreeBSD rc.d

Install [the example rc.d script](contrib/freebsd/immortaldir) as
`/usr/local/etc/rc.d/immortaldir`, owned by root and executable. It uses
FreeBSD `daemon(8)` with a supervisor PID file and restart delay so `rc.subr`
can stop the manager rather than its child. TERM is forwarded to `immortaldir`,
which shuts down and reaps only its launcher broker; independently daemonized
service supervisors continue running.

Enable and configure it in `rc.conf`:

```sh
sysrc immortaldir_enable=YES
sysrc immortaldir_definitions=/usr/local/etc/immortal
sysrc immortaldir_runtime_dir=/var/run/immortal
sysrc immortaldir_supervisor=/usr/local/sbin/immortal
service immortaldir start
service immortaldir status
```

The pre-start hook recreates the runtime root with mode `0755` after boot. The
definitions directory must already exist. Adjust the `REQUIRE` line locally if
the definitions filesystem has additional mount ordering requirements.

## Linux systemd

Install [the example unit](contrib/systemd/immortaldir.service), then review all
paths before enabling it. `KillMode=process` is deliberate: restarting the
directory manager must not kill service supervisors in the same cgroup. The
unit creates `/run/immortal` before startup but does not remove it on manager
shutdown.

## macOS launchd

Create `/var/db/immortal/run` and `/usr/local/etc/immortal` as root-owned,
non-group-writable directories before installing
[the example LaunchDaemon](contrib/launchd/run.immortal.immortaldir.plist).
The persistent `/var/db` runtime root avoids relying on `/var/run` surviving a
reboot. `AbandonProcessGroup` is deliberate because service supervisors are
independent of the directory-manager job. Validate these semantics on the
target macOS release before using the example outside release-candidate tests.

## Rollback

Stop only the new directory manager first. Use `immortalctl halt --all` against
the new runtime root when the managed services must also stop. Preserve the
strict definitions for diagnosis, remove only runtime entries proven inactive,
and then restore the previous manager and its separate configuration. Never use
PID-file contents as authority for cleanup.
