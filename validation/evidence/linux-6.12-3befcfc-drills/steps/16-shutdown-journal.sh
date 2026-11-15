# Boot drill, part three: how the previous boot's shutdown stopped the
# directory manager and the supervisors KillMode=process left in its cgroup.
# The host name contains "immortal", so it is removed before matching.
set -e
J() { sudo journalctl -b -1 --no-pager -o short-iso "$@" | sed 's/ immortal-linux\.127\.network / /'; }
echo "-- immortaldir unit, previous boot, from the reboot request on"
J -u immortaldir --since '2026-11-05 08:45:20'
echo "-- remaining processes, signals, and timeouts during shutdown"
J --since '2026-11-05 08:45:20' | grep -i -E 'immortal|heartbeat|remain|Sending SIG|timed out|Killing process|still around|Final kill' || echo "none"
echo "-- shutdown milestones"
J --since '2026-11-05 08:45:20' | grep -E 'Stopping immortaldir|Reached target (shutdown|final|reboot)|Journal stopped' || true
