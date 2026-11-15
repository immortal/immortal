# Compare the dry-run preview with a real one-shot pass when a requirement is
# invalid: api.yml names a missing working directory and worker requires api.
# Records whether --dry-run reports the dependency isolation the real pass
# applies (README: "--dry-run performs the same bounded scan and planning").
set -u
bin=$HOME/immortal-3befcfc/target/release
work=$(mktemp -d "$HOME/drill-3befcfc/dep.XXXXXX")
mkdir -m 0755 "$work/defs"
mkdir -m 0700 "$work/run"
sed 's#/var/lib/immortal-drill/api#/var/lib/immortal-drill/missing#' "$HOME/drill-3befcfc/defs/v2/api.yml" > "$work/defs/api.yml"
cp "$HOME/drill-3befcfc/defs/v2/worker.yml" "$work/defs/worker.yml"
sed -i 's#/var/log/immortal-drill/worker.log#'"$work"'/worker.log#' "$work/defs/worker.yml"
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
find "$work" -mindepth 1 -maxdepth 1 -name run -exec find {} -mindepth 1 \; | sort
echo "work dir kept for review: $work"
