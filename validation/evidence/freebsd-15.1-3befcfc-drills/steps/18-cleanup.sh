# Return the dedicated VM to its pre-drill state: keep the drill service logs
# as evidence, stop and disable the manager, halt every service, and remove
# exactly the files the drills installed. Nothing Immortal may remain.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
E=$HOME/immortal-evidence/drills-3befcfc-freebsd
mkdir -p "$E/service-logs"
sudo cp /var/log/immortal-drill/api.log /var/log/immortal-drill/worker.log "$E/service-logs/"
sudo chown "$(id -u):$(id -g)" "$E/service-logs/api.log" "$E/service-logs/worker.log"
wc -l "$E/service-logs/api.log" "$E/service-logs/worker.log"
sudo service immortaldir stop
ctl halt --all
sleep 1
if pgrep -fl 'heartbeat|immortal'; then echo "LEFTOVER process"; exit 1; fi
for v in immortaldir_enable immortaldir_definitions immortaldir_runtime_dir immortaldir_supervisor; do
    sudo sysrc -x "$v"
done
sudo rm /usr/local/etc/rc.d/immortaldir
remove_inactive_runtime
sudo rmdir "$RUN"
sudo rm -f /var/run/immortaldir.pid
sudo rm "$DEFS/api.yml" "$DEFS/worker.yml"
sudo rmdir "$DEFS"
sudo rm $BINS
for b in immortal immortaldir immortalctl immortallog; do sudo rm "$ROLLBACK/$b"; done
sudo rmdir "$ROLLBACK" /usr/local/lib/immortal-rollback
sudo rm /usr/local/libexec/immortal-drill/heartbeat
sudo rmdir /usr/local/libexec/immortal-drill
sudo rm /var/db/immortal-drill/boottime.before /var/db/immortal-drill/reboot.requested /var/db/immortal-drill/reboot.syslogtime
sudo rmdir /var/db/immortal-drill/api /var/db/immortal-drill
sudo rm /var/log/immortal-drill/api.log /var/log/immortal-drill/worker.log
sudo rmdir /var/log/immortal-drill
echo "-- verification"
ls -A /usr/local/sbin /usr/local/bin /usr/local/lib /usr/local/libexec /usr/local/etc/rc.d
for p in "$DEFS" "$RUN" /usr/local/etc/rc.d/immortaldir /var/run/immortaldir.pid /var/db/immortal-drill /var/log/immortal-drill /usr/local/lib/immortal-rollback; do
    if sudo test -e "$p"; then echo "LEFTOVER $p"; exit 1; fi
done
sysrc -a | grep -i immortal | grep -v '^hostname:' | grep . && { echo "LEFTOVER rc.conf setting"; exit 1; }
if pgrep -fl immortal; then echo "LEFTOVER process"; exit 1; fi
echo "VM restored: no Immortal process, rc.d script, rc.conf setting, binary, definition, runtime, or drill file"
