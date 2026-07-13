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
