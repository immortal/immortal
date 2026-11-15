# Boot drill, part two: after the reboot, immortaldir must start from the
# enabled unit, recreate the ephemeral /run/immortal, and bring both services
# back to Ready by itself. The previous boot's journal shows how systemd
# stopped the manager and the supervisors it left running (KillMode=process).
set -e
. "$HOME/drill-3befcfc/lib.sh"
before=$(sudo cat /var/lib/immortal-drill/boot_id.before)
now=$(cat /proc/sys/kernel/random/boot_id)
echo "boot id before $before, now $now"
test "$before" != "$now"
uptime
echo "reboot requested at $(sudo cat /var/lib/immortal-drill/reboot.requested)"
systemctl is-enabled immortaldir
systemctl is-active immortaldir
ls -ld /run/immortal
wait_state api ready 120
wait_state worker ready 120
sleep 3
snapshot
echo "-- this boot: immortaldir unit"
sudo journalctl -b 0 -u immortaldir --no-pager -o short-iso
echo "-- previous boot: shutdown of the manager and its leftover supervisors"
sudo journalctl -b -1 --no-pager -o short-iso | grep -i -E 'immortal|heartbeat|remain|SIGTERM|SIGKILL|timed out|Killing|systemd-shutdown|Reached target .*(Shutdown|Final|Reboot)|Journal stopped' | tail -40
echo "-- previous boot: last journal line"
sudo journalctl -b -1 --no-pager -o short-iso -n 1
echo "-- heartbeats around the reboot"
for s in api worker; do
    echo "$s:"
    sudo awk -v t="$(sudo cat /var/lib/immortal-drill/reboot.requested)" '$2 >= substr(t, 1, 19) || NR == 0' "/var/log/immortal-drill/$s.log" | head -6
done
