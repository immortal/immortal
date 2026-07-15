# Developer diagnostic tools

This separate Cargo workspace contains deterministic workloads for hands-on
Immortal testing. Its binaries are maintained by CI but are not product
components, release artifacts, or installed commands. Automated contracts keep
their own hermetic fixtures.

Each helper has its own package below this directory. The first is
`immortal-log-probe`, which writes one flushed record to stdout and stderr
immediately and then at a configurable interval. The process ID and sequence
make restarts visible:

```text
stream=stdout event=tick sequence=1 pid=123 elapsed_ms=0
stream=stderr event=tick sequence=1 pid=123 elapsed_ms=0
```

## Build and run

Enter the DevPod at the repository root, then build every diagnostic tool:

```sh
scripts/dev-up
scripts/dev-ssh
just tools-build
```

Run the probe directly until it is interrupted:

```sh
tools/target/debug/immortal-log-probe
```

The supported options are:

```text
immortal-log-probe [--interval DURATION]
                   [--exit-after DURATION [--exit-code CODE]]
```

Durations are positive whole numbers using `ms`, `s`, `m`, or `h`.
`--interval` defaults to `1s`. `--exit-after` enables a final flushed
`event=exit` record, and `--exit-code` selects its status from `0` through
`255`; the default is `1`.

For a short standalone run:

```sh
tools/target/debug/immortal-log-probe \
  --interval 250ms \
  --exit-after 3s \
  --exit-code 0
```

## Exercise configured logging and restarts

Both definitions resolve the tool relative to this directory, rotate at 1 KiB,
retain seven archives, and run four failing generations: the initial process
plus three retries.

### Split stdout and stderr

The split definition writes each stream to its own timestamped file:

```sh
rm -f /tmp/immortal-log-probe.stdout.log* \
      /tmp/immortal-log-probe.stderr.log*
cargo run --quiet --locked -p immortal -- \
  -f -c tools/log-probe-split.yml
```

From another DevPod shell, inspect the streams and supervisor while it runs:

```sh
tail -f /tmp/immortal-log-probe.stdout.log \
        /tmp/immortal-log-probe.stderr.log
cargo run --quiet --locked -p immortalctl -- status log-probe-split
```

Each stream file should contain only its matching label.

### Combine stdout and stderr

The combined definition uses the concise `log.file` structure, so both streams
share one timestamped file:

```sh
rm -f /tmp/immortal-log-probe.log*
cargo run --quiet --locked -p immortal -- \
  -f -c tools/log-probe-combined.yml
```

From another DevPod shell:

```sh
tail -f /tmp/immortal-log-probe.log
cargo run --quiet --locked -p immortalctl -- status log-probe-combined
```

The combined file should contain both `stream=stdout` and `stream=stderr`
records. Their relative order is intentionally unspecified.

### Exit after successful completion

`log-probe-combined-exit-on-success.yml` also combines both streams, but the
probe exits with status `0` and the restart policy enables
`exit_when_done: true`. The foreground Immortal process therefore exits after
the first ten-second generation:

```sh
rm -f /tmp/immortal-log-probe-exit-on-success.log*
cargo run --quiet --locked -p immortal -- \
  -f -c tools/log-probe-combined-exit-on-success.yml
```

This exercises successful terminal completion. Because status `0` is successful
for `on-failure`, no retry is requested and `max_retries` is not consumed.

### Exit after retry exhaustion

`log-probe-combined-exit-after-retries.yml` exits each generation with status
`1`. It permits three retries after the initial start, then drains the combined
logger and exits Immortal with temporary-failure status:

```sh
rm -f /tmp/immortal-log-probe-exit-after-retries.log*
cargo run --quiet --locked -p immortal -- \
  -f -c tools/log-probe-combined-exit-after-retries.yml
```

The nonzero command status is expected. This definition uses a fixed one-second
backoff and three-second generations to make the exhausted-limit path quick to
exercise.

Rotated siblings use
`<file>.@<unix-nanoseconds>.<immortallog-pid>.<sequence>`. Read any live-file
namespace as a UTC table or JSON instead of decoding that identity manually:

```sh
cargo run --quiet --locked -p immortallog -- \
  archives /tmp/immortal-log-probe.stdout.log
cargo run --quiet --locked -p immortallog -- \
  archives -o json /tmp/immortal-log-probe.log
```

The `@` is a rotation marker, while the numeric timestamp is Unix nanoseconds;
it is not a TAI64N or multilog `.s`/`.u` state marker.

Retry exhaustion is intentional in the split and standard combined definitions.
After the fourth failed generation, those supervisors report `Failed` and
remain available for manual recovery or inspection because `exit_when_done`
defaults to `false`. Each new process ID proves that Immortal started a new
generation, while the archive sequence demonstrates rotation across those
generations. End either active foreground exercise from another shell:

```sh
cargo run --quiet --locked -p immortalctl -- halt log-probe-split
# Or, for the combined definition:
cargo run --quiet --locked -p immortalctl -- halt log-probe-combined
```

## Exercise direct logging options

One combined local file:

```sh
cargo run --quiet --locked -p immortal -- \
  -f -n log-probe-direct -r 3 \
  --logfile /tmp/immortal-log-probe.log \
  tools/target/debug/immortal-log-probe \
  --exit-after 10s
```

A combined local file and one centralized logger may coexist. This example
uses an explicit shell only to provide a portable append sink:

```sh
cargo run --quiet --locked -p immortal -- \
  -f -n log-probe-forward -r 3 \
  --logfile /tmp/immortal-log-probe.log \
  --logger /bin/sh -c 'cat >> /tmp/immortal-log-probe.forwarded.log' \
  -- tools/target/debug/immortal-log-probe \
  --exit-after 10s
```

The local and forwarded files should contain both stream labels. Relative
ordering between concurrent stdout and stderr records is intentionally not a
logging contract.

## Maintain the tools workspace

Run its complete native checks directly:

```sh
just tools-check
```

The root `fmt`, `clippy`, `check`, `test`, and `ci` recipes also include this
workspace. Check portability separately:

```sh
cargo check --manifest-path tools/Cargo.toml \
  --workspace \
  --target x86_64-unknown-freebsd \
  --locked
```
