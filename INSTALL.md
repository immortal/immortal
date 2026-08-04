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
| `cwd`, `env`, `user` | `working_directory`, `environment`, `user` | `env` remains an input alias for `environment`; select `environment_mode: inherit` or `clear` deliberately. |
| `wait` | `start_delay_seconds` and restart backoff | The old delay applied before every launch; v2 separates the first start from crash-loop delay. |
| `retries` | `restart` policy and limits | Review success codes, terminal behavior, and retry, elapsed, or burst limits. |
| `require` | `requires` plus readiness | Dependencies are validated as a graph and gate on Ready. |
| `require_cmd` | `start_condition` | Use exact argv, a deadline, and independent backoff instead of an implicit unbounded shell command. |
| `post_exit` | `post_exit` command hook | Use exact argv and a deadline; read result context from the documented environment. |
| `log`, `stderr`, `logger` | `log` plus `logger` | `log` retains local files; one top-level logger argv receives both streams. |
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
the complete [descriptor-tracking example and lifecycle](README.md#descriptor-tracking).

A representative legacy definition:

```yaml
---
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
---
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
log:
  file: /var/log/api.log
  age: 1d
  keep: 7
  size: 1MiB
```

`max_retries: 3` permits the initial start plus three restarts. Its default
terminal behavior is a persistent `Failed` supervisor; add
`exit_when_done: true` when the supervisor should instead clean up and exit
with a failure status after exhausting the limit.

`immortal --check-config` validates and emits canonical v2 only; it does not
guess at an unversioned definition or modify its input. It accepts `env` as an
alias but emits `environment`. For an incremental logging migration inside a
strict `version: 2` document, `num` is a deprecated alias for `keep`, bare `age`
values mean seconds, bare `size` values mean MiB, and top-level `stderr` is a
deprecated alias for a selected stderr file:

```yaml
---
version: 2
command: [/usr/local/bin/api, --foreground]
log:
  file: /var/log/api.log
  age: 86400
  num: 7
  size: 1
stderr:
  file: /var/log/api.err
```

The canonical output makes the split and units explicit:

```yaml
---
version: 2
command: [/usr/local/bin/api, --foreground]
log:
  stdout:
    file: /var/log/api.log
    age: 1d
    keep: 7
    size: 1MiB
  stderr:
    file: /var/log/api.err
```

Use `logger: [/usr/bin/logger, -t, api]` independently or alongside `log`.
Unlike nested local routes, the single logger always receives both stdout and
stderr. Review each rewritten definition before placing it in the candidate
directory.

The direct-command CLI accepts `-e DIR` and `--env-dir DIR`. It snapshots each
regular file's first line before daemonization, applies those values after the
inherited environment, and fails on unreadable, changing, malformed, or
unbounded regular input. The option cannot be combined with `--config`; use the
v2 environment map in definitions.

The Rust CLI accepts `-l FILE`/`--logfile FILE` for a combined local file and
long-only `--logger PROGRAM [ARGUMENTS]... -- SERVICE [ARGUMENTS]...` for one
exact external logger argv. They may coexist, preserve the Go logfile defaults
of 1 MiB and seven archives, and conflict with `--config`:

```sh
immortal -f -n api \
  --logfile /var/log/api.log \
  --logger /usr/bin/logger -t api \
  -- /usr/local/bin/api --foreground
```

The historical `--log-file`, `-logger`, and `--follow-pid` spellings remain
rejected. The historical `-name service` form becomes `-n service` or
`--name service`; the exact `-name` token is rejected rather than being misread
as `-n ame`. The old `-n` foreground shorthand becomes `-f` or `--foreground`.
A direct command requires either that name or an exact `--control-dir`.

The Rust prototype's `.immortal-archive.<time>.<pid>.<sequence>` names are not
migrated. New rotations use
`<file>.@<unix-nanoseconds>.<immortallog-pid>.<sequence>`; old prototype files
remain untouched and do not count toward retention. After selecting a live log
path, inspect only its new archive namespace with:

```sh
immortallog archives /var/log/api.log
immortallog archives -o json /var/log/api.log
```

The table converts rotation time to UTC. JSON also retains the exact Unix
nanoseconds as a string. The compact `@` marker does not imply TAI64N or
multilog-compatible `.s`/`.u` processing states.

Without `--control-dir`, config filenames provide the service identity
(`api.yml` becomes `api`) and named direct commands use the supplied name.
Both resolve below an automatically created, owner-only `$HOME/.immortal`.
Unsafe or hidden names are rejected rather than sanitized, and there is no
PID-based fallback. There is no PID-following replacement; use a foreground
command or the descriptor-tracking contract described above.

Before the first Rust launch, restrict an existing Go-era user root if needed:

```sh
chmod 700 "$HOME/.immortal"
```

Immortal creates a missing root with that mode and refuses an existing root
owned by another account, writable or readable by group/other, or itself a
symbolic link. Symlinked home-directory ancestors remain supported when the
resolved home belongs to the effective user and is not group/world writable.
The final control-socket pathname is also rejected before creating runtime
state when it exceeds the portable 103-byte Unix-socket limit.

Before migrating local definitions, the repository example provides a bounded
rehearsal which must print canonical configuration followed by a `START` plan:

```sh
immortal --config examples/services/sleep.yml --check-config
immortaldir --once --dry-run examples/services
```

Treat runtime directories as disposable observation state, not configuration.
For `immortaldir`, use `/run/immortal` on Linux and `/var/run/immortal` on
FreeBSD or macOS; the service manager recreates these ephemeral roots after
boot. Do not use the symlinked `/var/run` spelling on Linux because runtime
roots must be canonical. Never copy sockets, locks, PID files, or
`.definitions` snapshots from the Go installation. Do not run old and new
directory managers against the same definitions or runtime root.

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
definitions directory must already exist, and must be owned by `root` or the
user `immortaldir` runs as with no group or world write permission — mode
`0755` or stricter. Create it with an explicit mode rather than relying on the
umask; a directory left group writable is rejected at startup. Adjust the
`REQUIRE` line locally if
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
