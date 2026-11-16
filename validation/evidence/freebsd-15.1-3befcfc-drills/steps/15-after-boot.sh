# Boot drill, part two: after the reboot, rc.d must start immortaldir from
# rc.conf, its pre-start hook must recreate the ephemeral /var/run/immortal,
# and both services must return to Ready by themselves. The system log shows
# how the shutdown stopped the manager and what happened to the supervisors.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
before=$(sudo cat /var/db/immortal-drill/boottime.before)
now=$(sysctl -n kern.boottime)
echo "boot time before: $before"
echo "boot time now:    $now"
test "$before" != "$now"
uptime
echo "reboot requested at $(sudo cat /var/db/immortal-drill/reboot.requested)"
sysrc immortaldir_enable
sudo service immortaldir status
ls -ld "$RUN"
wait_state api ready 120
wait_state worker ready 120
sleep 3
snapshot
echo "-- system log: immortal, shutdown and boot lines since the reboot request"
sudo awk -v t="$(sudo cat /var/db/immortal-drill/reboot.syslogtime)" '
    substr($0, 1, 15) >= t || seen { seen = 1; print }' /var/log/messages |
    sed 's/ immortal-freebsd / /' |
    grep -i -E 'immortal|heartbeat|shutdown|reboot|rc\.shutdown|Stopping|Waiting for PIDS|timed out|SIGTERM|SIGKILL|kernel: ---<<BOOT>>---|FreeBSD 1' | tail -60
echo "-- heartbeats around the reboot"
for s in api worker; do
    echo "$s:"
    sudo awk -v t="$(sudo cat /var/db/immortal-drill/reboot.requested)" '$2 >= substr(t, 1, 19)' "/var/log/immortal-drill/$s.log" | head -6
done
