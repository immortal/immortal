# Rollback drill 3befcfc -> 43a7a6e, following INSTALL.md "Rollback": stop
# only the new directory manager, halt every service against the new runtime
# root, preserve the strict definitions, remove only runtime entries proven
# inactive, then restore the previous manager and start it.
set -e
. "$HOME/drill-3befcfc/lib.sh"
test "$(installed_revision)" = "$CAND"
sudo systemctl stop immortaldir
ctl halt --all
sleep 1
if pgrep -fl 'heartbeat|immortal --config|immortallog|immortaldir'; then echo "LEFTOVER after halt"; exit 1; fi
echo "-- definitions preserved"
ls -l /etc/immortal
sha256sum /etc/immortal/*.yml
echo "-- runtime entries before removal"
sudo find /run/immortal -mindepth 1 | sort
for s in api worker; do
    dir=/run/immortal/$s
    if sudo test -e "$dir/immortal.sock"; then echo "ACTIVE socket in $dir"; exit 1; fi
    # The advisory lock must be free: no supervisor can still own this entry.
    sudo flock -n "$dir/supervisor.lock" true
    echo "$dir: no socket, lock free"
    sudo rm "$dir/supervisor.lock"
    sudo rmdir "$dir"
done
for f in api.applied.yml api.launch.yml worker.applied.yml worker.launch.yml tracker.state; do
    sudo rm "/run/immortal/.definitions/$f"
done
sudo rmdir /run/immortal/.definitions
echo "-- runtime entries after removal"
sudo find /run/immortal -mindepth 1 | sort
echo "(empty)"
install_release "$ROLLBACK"
test "$(installed_revision)" = "$PREV"
sudo systemctl start immortaldir
wait_state api ready 60
wait_state worker ready 60
sleep 3
snapshot
prev_hash=$(sha256sum "$ROLLBACK/immortal" | cut -d ' ' -f 1)
for pid in $(ctl status --no-header | awk '{ print $4 }'); do
    test "$(sudo sha256sum "/proc/$pid/exe" | cut -d ' ' -f 1)" = "$prev_hash"
done
echo "every supervisor runs the restored 43a7a6e binary $prev_hash"
