# Return the dedicated VM to its pre-drill state: keep the drill service logs
# as evidence, stop and disable the manager, halt every service, and remove
# exactly the files the drills installed. Nothing Immortal may remain.
set -e
. "$HOME/drill-3befcfc/lib.sh"
E=$HOME/immortal-evidence/drills-3befcfc-linux
mkdir -p "$E/service-logs"
sudo cp /var/log/immortal-drill/api.log /var/log/immortal-drill/worker.log "$E/service-logs/"
sudo chown "$(id -u):$(id -g)" "$E/service-logs/api.log" "$E/service-logs/worker.log"
wc -l "$E/service-logs/api.log" "$E/service-logs/worker.log"
sudo systemctl disable --now immortaldir
ctl halt --all
sleep 1
if pgrep -fl 'heartbeat|immortal'; then echo "LEFTOVER process"; exit 1; fi
sudo rm /etc/systemd/system/immortaldir.service
sudo systemctl daemon-reload
sudo systemctl reset-failed immortaldir 2>/dev/null || true
for s in api worker; do
    sudo flock -n "/run/immortal/$s/supervisor.lock" true
    sudo rm "/run/immortal/$s/supervisor.lock"
    sudo rmdir "/run/immortal/$s"
done
for f in api.applied.yml api.launch.yml worker.applied.yml worker.launch.yml tracker.state; do
    sudo rm "/run/immortal/.definitions/$f"
done
sudo rmdir /run/immortal/.definitions /run/immortal
sudo rm /etc/immortal/api.yml /etc/immortal/worker.yml
sudo rmdir /etc/immortal
sudo rm /usr/local/sbin/immortal /usr/local/sbin/immortaldir /usr/local/sbin/immortallog /usr/local/bin/immortalctl
for b in immortal immortaldir immortalctl immortallog; do sudo rm "$ROLLBACK/$b"; done
sudo rmdir "$ROLLBACK" /usr/local/lib/immortal-rollback
sudo rm /usr/local/libexec/immortal-drill/heartbeat
sudo rmdir /usr/local/libexec/immortal-drill
sudo rm /var/lib/immortal-drill/boot_id.before /var/lib/immortal-drill/reboot.requested
sudo rmdir /var/lib/immortal-drill/api /var/lib/immortal-drill
sudo rm /var/log/immortal-drill/api.log /var/log/immortal-drill/worker.log
sudo rmdir /var/log/immortal-drill
echo "-- verification"
ls -A /usr/local/sbin /usr/local/bin /usr/local/lib /usr/local/libexec
for p in /etc/immortal /run/immortal /etc/systemd/system/immortaldir.service /var/lib/immortal-drill /var/log/immortal-drill /usr/local/lib/immortal-rollback; do
    if sudo test -e "$p"; then echo "LEFTOVER $p"; exit 1; fi
done
systemctl list-unit-files immortaldir.service --no-legend || true
if pgrep -fl immortal; then echo "LEFTOVER process"; exit 1; fi
echo "VM restored: no Immortal process, unit, binary, definition, runtime, or drill file"
