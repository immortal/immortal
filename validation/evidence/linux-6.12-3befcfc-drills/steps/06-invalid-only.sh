# Exit status of --dry-run and a real one-shot pass when the directory holds
# only one invalid definition and nothing depends on it.
set -u
bin=$HOME/immortal-3befcfc/target/release
work=$(mktemp -d "$HOME/drill-3befcfc/inv.XXXXXX")
mkdir -m 0755 "$work/defs"
mkdir -m 0700 "$work/run"
sed 's#/var/lib/immortal-drill/api#/var/lib/immortal-drill/missing#' "$HOME/drill-3befcfc/defs/v2/api.yml" > "$work/defs/api.yml"
chmod 0644 "$work/defs/api.yml"
"$bin/immortaldir" --once --dry-run "$work/defs"
echo "dry-run exit=$?"
"$bin/immortaldir" --once --runtime-dir "$work/run" --supervisor-binary "$bin/immortal" "$work/defs"
echo "once exit=$?"
find "$work/run" -name immortal.sock | wc -l
printf 'version: 2\ncommand: [\n' > "$work/defs/api.yml"
"$bin/immortaldir" --once --dry-run "$work/defs"
echo "dry-run malformed exit=$?"
