# Boot drill, part one: record the running state and boot identity, then
# schedule a reboot a few seconds after this step's log is complete.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
sysrc immortaldir_enable
sudo service immortaldir status
sysctl -n kern.boottime | sudo tee /var/db/immortal-drill/boottime.before
uptime
snapshot
date -u +%Y-%m-%dT%H:%M:%SZ | sudo tee /var/db/immortal-drill/reboot.requested
date '+%b %e %H:%M:%S' | sudo tee /var/db/immortal-drill/reboot.syslogtime
nohup sh -c 'sleep 5; sudo shutdown -r now' > /dev/null 2>&1 < /dev/null &
echo "reboot scheduled"
