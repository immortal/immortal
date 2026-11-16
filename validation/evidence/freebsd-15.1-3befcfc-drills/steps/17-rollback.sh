# Rollback drill 3befcfc -> 43a7a6e, following INSTALL.md "Rollback": stop
# only the new directory manager, halt every service against the new runtime
# root, preserve the strict definitions, remove only runtime entries proven
# inactive, then restore the previous manager and start it.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
test "$(installed_revision)" = "$CAND"
sudo service immortaldir stop
ctl halt --all
sleep 1
if pgrep -fl 'heartbeat|immortal --config|immortallog|immortaldir'; then echo "LEFTOVER after halt"; exit 1; fi
echo "-- definitions preserved"
ls -l "$DEFS"
sha256 "$DEFS"/*.yml
echo "-- runtime entries before removal"
sudo find "$RUN" -mindepth 1 | sort
remove_inactive_runtime
echo "-- runtime entries after removal"
sudo find "$RUN" -mindepth 1 | sort
echo "(empty)"
install_release "$ROLLBACK"
test "$(installed_revision)" = "$PREV"
restored=$(date +%s)
sudo service immortaldir start
wait_state api ready 60
wait_state worker ready 60
sleep 3
snapshot
for pid in $(ctl status --no-header | awk '{ print $4 }'); do
    exe=$(exe_of "$pid")
    echo "supervisor $pid exe=$exe"
    test "$exe" = /usr/local/sbin/immortal
    test "$(ps -o etimes= -p "$pid")" -le "$(( $(date +%s) - restored ))"
done
test "$(sha256 -q /usr/local/sbin/immortal)" = "$(sha256 -q "$ROLLBACK/immortal")"
echo "every supervisor runs the restored 43a7a6e binary $(sha256 -q /usr/local/sbin/immortal)"
