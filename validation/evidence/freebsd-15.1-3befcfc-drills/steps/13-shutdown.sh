# Shutdown drill. Stopping the rc.d service stops only the directory manager:
# service supervisors keep running and keep writing. Halting every service
# then stops each supervisor, its service group, and its log adapter, leaving
# no socket, process, or open log writer; only the released advisory lock
# files and the owner-only .definitions snapshots stay.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
before=$(ctl status --no-header | awk '{ print $2, $4, $5 }')
start=$(date +%s)
sudo service immortaldir stop
echo "service stop took $(( $(date +%s) - start ))s"
sudo service immortaldir status || echo "manager is not running"
if pgrep -x immortaldir; then echo "LEFTOVER immortaldir"; exit 1; fi
if pgrep -fl 'daemon.*immortaldir'; then echo "LEFTOVER daemon(8)"; exit 1; fi
sleep 4
after=$(ctl status --no-header | awk '{ print $2, $4, $5 }')
test "$before" = "$after" && echo "supervisors and services survived the manager stop: $after"
snapshot
lines_before=$(sudo cat /var/log/immortal-drill/api.log | wc -l)
start=$(date +%s)
ctl halt --all
echo "halt --all took $(( $(date +%s) - start ))s"
sleep 1
if pgrep -fl 'heartbeat|immortal --config|immortallog'; then echo "LEFTOVER process after halt"; exit 1; fi
echo "no supervisor, service, or log adapter process remains"
echo "-- runtime root after halt"
sudo find "$RUN" -mindepth 1 | sort
if sudo find "$RUN" -name immortal.sock | grep -q .; then echo "LEFTOVER socket"; exit 1; fi
lines_halted=$(sudo cat /var/log/immortal-drill/api.log | wc -l)
sleep 5
lines_later=$(sudo cat /var/log/immortal-drill/api.log | wc -l)
echo "api log lines: before halt $lines_before, at halt $lines_halted, 5s later $lines_later"
test "$lines_halted" = "$lines_later"
echo "-- open files on the drill logs"
sudo fstat /var/log/immortal-drill/api.log /var/log/immortal-drill/worker.log
sudo service immortaldir start
wait_state api ready 60
wait_state worker ready 60
snapshot
