#!/bin/sh
set -eu

script_directory=$(CDPATH='' cd "$(dirname "$0")" && pwd)
repository=$(CDPATH='' cd "$script_directory/.." && pwd)
runtime_root=${IMMORTAL_EXAMPLE_RUNTIME_DIR:-"$HOME/.immortal"}
config=${IMMORTAL_EXAMPLE_CONFIG:-"$repository/examples/services/sleep.yml"}
config_filename=${config##*/}
service=${IMMORTAL_EXAMPLE_SERVICE:-"${config_filename%.*}"}
explicit_control=
if [ "${IMMORTAL_EXAMPLE_RUNTIME_DIR+x}" = x ] ||
    [ "${IMMORTAL_EXAMPLE_SERVICE+x}" = x ]; then
    explicit_control=1
fi

usage() {
cat <<EOF
Usage: examples/run-immortal.sh [--daemon]

Run the example in the foreground by default. Use --daemon to omit -f and
exercise checked double-fork daemonization.

Without overrides, immortal creates \$HOME/.immortal and derives the service
name from the configuration filename. Runtime or service environment overrides
select an explicit control directory instead.
EOF
}

mode=foreground
case "${1:-}" in
--daemon)
    mode=daemon
    shift
    ;;
-h | --help)
    usage
    exit 0
    ;;
'') ;;
*)
    usage >&2
    exit 64
    ;;
esac
if [ "$#" -ne 0 ]; then
usage >&2
exit 64
fi

case "$runtime_root" in
/*) ;;
*)
    echo "IMMORTAL_EXAMPLE_RUNTIME_DIR must be absolute: $runtime_root" >&2
    exit 64
    ;;
esac

if [ -n "$explicit_control" ]; then
    install -d -m 700 "$runtime_root"
    runtime_root=$(CDPATH='' cd "$runtime_root" && pwd -P)
fi
cd "$repository"

if [ -n "$explicit_control" ]; then
    printf 'Starting %s in %s mode with explicit control directory %s\n' \
        "$service" "$mode" "$runtime_root/$service"
else
    printf 'Starting %s in %s mode with automatic control directory %s\n' \
        "$service" "$mode" "$runtime_root/$service"
fi
printf 'Inspect it from another DevPod shell with examples/run-immortalctl.sh\n'

run_immortal() {
    if [ -n "$explicit_control" ]; then
        exec cargo run --quiet --locked -p immortal -- \
            "$@" \
            --control-dir "$runtime_root/$service" \
            -c "$config"
    fi
    exec cargo run --quiet --locked -p immortal -- "$@" -c "$config"
}

if [ "$mode" = foreground ]; then
    run_immortal -f
fi
run_immortal
