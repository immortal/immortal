#!/bin/sh
set -eu

script_directory=$(CDPATH='' cd "$(dirname "$0")" && pwd)
repository=$(CDPATH='' cd "$script_directory/.." && pwd)
runtime_root=${IMMORTAL_EXAMPLE_RUNTIME_DIR:-"$HOME/.immortal"}
service=${IMMORTAL_EXAMPLE_SERVICE:-sleep}
custom_runtime=
if [ "${IMMORTAL_EXAMPLE_RUNTIME_DIR+x}" = x ]; then
    custom_runtime=1
fi

case "$runtime_root" in
    /*) ;;
    *)
        echo "IMMORTAL_EXAMPLE_RUNTIME_DIR must be absolute: $runtime_root" >&2
        exit 64
        ;;
esac

if [ -n "$custom_runtime" ] && [ -d "$runtime_root" ]; then
    runtime_root=$(CDPATH='' cd "$runtime_root" && pwd -P)
fi
cd "$repository"

if [ "$#" -gt 0 ]; then
    if [ -n "$custom_runtime" ]; then
        exec cargo run --quiet --locked -p immortalctl -- \
            --runtime-dir "$runtime_root" \
            "$@"
    fi
    exec cargo run --quiet --locked -p immortalctl -- \
        --runtime-scope user \
        "$@"
fi

if [ -n "$custom_runtime" ]; then
    status=$(cargo run --quiet --locked -p immortalctl -- \
        --runtime-dir "$runtime_root" \
        --output json \
        status "$service")
else
    status=$(cargo run --quiet --locked -p immortalctl -- \
        --runtime-scope user \
        --output json \
        status "$service")
fi
printf '%s\n' "$status"

SUP=$(printf '%s\n' "$status" |
    sed -n 's/.*"supervisor_pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p')
MAIN=$(printf '%s\n' "$status" |
    sed -n 's/.*"main_pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p')

case "$SUP" in
    '' | *[!0-9]*)
        echo "status did not publish a valid supervisor PID" >&2
        exit 69
        ;;
esac
case "$MAIN" in
    '' | *[!0-9]*)
        echo "status did not publish a live main PID; retry when the service is Ready" >&2
        exit 69
        ;;
esac

case "$(uname -s)" in
    Linux) elapsed_field=etimes ;;
    *) elapsed_field=etime ;;
esac
ps_fields="euser,egroup,pid,ppid,pgid,sid,stat,$elapsed_field,args"

printf '\nSUP=%s MAIN=%s\n' "$SUP" "$MAIN"
printf '\nSupervisor and main process:\n'
if ! ps -o "$ps_fields" -p "$SUP,$MAIN"; then
    echo "the reported generation changed before it could be inspected" >&2
    exit 75
fi

printf '\nSupervisor process tree:\n'
if command -v pstree >/dev/null 2>&1; then
    pstree -ap "$SUP"
else
    echo "pstree is unavailable; continuing with ps process-group output" >&2
fi

PGID=$(ps -o pgid= -p "$MAIN" | tr -d '[:space:]')
case "$PGID" in
    '' | *[!0-9]*)
        echo "the main process exited before its process group could be read" >&2
        exit 75
        ;;
esac

printf '\nService process group %s:\n' "$PGID"
if ! processes=$(ps -eo "$ps_fields"); then
    echo "unable to enumerate processes" >&2
    exit 71
fi
if ! printf '%s\n' "$processes" |
    awk -v pgid="$PGID" '
        NR == 1 {
            print
            next
        }
        $5 == pgid {
            print
            found = 1
        }
        END {
            if (!found) {
                exit 1
            }
        }
    '
then
    echo "the service process group changed before it could be inspected" >&2
    exit 75
fi
