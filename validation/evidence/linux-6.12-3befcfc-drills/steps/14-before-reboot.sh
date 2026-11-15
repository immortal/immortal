# Boot drill, part one: record the running state and boot identity, then
# schedule a reboot a few seconds after this step's log is complete.
set -e
. "$HOME/drill-3befcfc/lib.sh"
systemctl is-enabled immortaldir
systemctl is-active immortaldir
cat /proc/sys/kernel/random/boot_id | sudo tee /var/lib/immortal-drill/boot_id.before
uptime
ls -ld /var/log/journal || echo "journal is not persistent"
snapshot
date -u +%Y-%m-%dT%H:%M:%SZ | sudo tee /var/lib/immortal-drill/reboot.requested
nohup sh -c 'sleep 5; sudo systemctl reboot' > /dev/null 2>&1 < /dev/null &
echo "reboot scheduled"
