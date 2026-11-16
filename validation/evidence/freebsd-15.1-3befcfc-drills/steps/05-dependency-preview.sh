# Compare the dry-run preview with a real one-shot pass when a requirement is
# invalid: api.yml names a missing working directory and worker requires api.
set -u
bin=$HOME/immortal-3befcfc/target/release
kit=$HOME/drill-3befcfc-freebsd
work=$(mktemp -d "$kit/dep.XXXXXX")
mkdir -m 0755 "$work/defs"
mkdir -m 0700 "$work/run"
sed 's#/var/db/immortal-drill/api#/var/db/immortal-drill/missing#' "$kit/defs/v2/api.yml" > "$work/defs/api.yml"
sed 's#/var/log/immortal-drill/worker.log#'"$work"'/worker.log#' "$kit/defs/v2/worker.yml" > "$work/defs/worker.yml"
chmod 0644 "$work/defs"/*.yml
echo "== dry-run"
"$bin/immortaldir" --once --dry-run "$work/defs"
echo "dry-run exit=$?"
echo "== real one-shot pass"
"$bin/immortaldir" --once --runtime-dir "$work/run" --supervisor-binary "$bin/immortal" "$work/defs"
echo "once exit=$?"
echo "== runtime root after the pass"
find "$work/run" -mindepth 1 | sort
started=$(find "$work/run" -name immortal.sock | wc -l)
echo "supervisor sockets: $started"
"$bin/immortalctl" --runtime-dir "$work/run" status 2>&1 || true
if [ "$started" -ne 0 ]; then
    "$bin/immortalctl" --runtime-dir "$work/run" halt --all
fi
if pgrep -fl "$work"; then echo "LEFTOVER process"; fi
find "$work/run" -mindepth 1 | sort
echo "work dir kept for review: $work"
