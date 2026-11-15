# Strict-v2 migration drill (INSTALL.md "Definition migration"): the candidate
# rejects unversioned Go definitions, accepts their hand-converted version 2
# form, plans it without mutating runtime state, and canonicalizes the
# documented deprecated logging aliases exactly as INSTALL.md shows.
set -u
bin=$HOME/immortal-3befcfc/target/release
defs=$HOME/drill-3befcfc/defs
chmod 0755 "$defs" "$defs/legacy" "$defs/v2" "$defs/alias"
chmod 0644 "$defs"/*/*.yml
fail=0
for f in "$defs"/legacy/*.yml; do
    echo "== legacy $(basename "$f")"
    cat "$f"
    if "$bin/immortal" --config "$f" --check-config; then
        echo "UNEXPECTED: legacy definition accepted"; fail=1
    else
        echo "rejected as required (exit $?)"
    fi
done
for f in "$defs"/v2/*.yml; do
    echo "== v2 $(basename "$f")"
    "$bin/immortal" --config "$f" --check-config || { echo "UNEXPECTED: v2 rejected"; fail=1; }
done
echo "== dry-run plan"
plan=$("$bin/immortaldir" --once --dry-run "$defs/v2") || fail=1
printf '%s\n' "$plan"
[ "$plan" = "$(printf 'START\tapi\nSTART\tworker')" ] || { echo "UNEXPECTED plan"; fail=1; }
echo "== deprecated logging aliases"
got=$("$bin/immortal" --config "$defs/alias/api.yml" --check-config) || fail=1
printf '%s\n' "$got"
want=$(cat <<'YAML'
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
YAML
)
[ "$got" = "$want" ] && echo "matches INSTALL.md canonical output" || { echo "DIFFERS from INSTALL.md canonical output"; fail=1; }
test ! -e "$HOME/.immortal" && echo "no runtime state created" || { ls -la "$HOME/.immortal"; fail=1; }
exit "$fail"
