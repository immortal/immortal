# Upgrade drill 43a7a6e -> 3befcfc. First the manager-only path: install the
# candidate binaries and restart immortaldir through rc.d; daemon(8) forwards
# TERM to the manager only, so every supervisor and service process must
# survive, and the new immortalctl must control the old supervisors. Then the
# full cutover, mirroring INSTALL.md rollback: stop the manager, halt every
# service, start the manager so each service restarts on the candidate.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
echo "== before upgrade"
snapshot
before=$(ctl status --no-header | awk '{ print $2, $4, $5 }')
install_release "$CAND_BIN"
test "$(installed_revision)" = "$CAND"
sudo service immortaldir restart
sleep 3
sudo service immortaldir status
after=$(ctl status --no-header | awk '{ print $2, $4, $5 }')
echo "service supervisor main before: $before"
echo "service supervisor main after:  $after"
test "$before" = "$after" && echo "manager restart kept every supervisor and main process"
echo "== old supervisors under the new manager and client"
snapshot
ctl restart worker
wait_state worker ready 30
ctl status worker
echo "== full cutover"
sudo service immortaldir stop
ctl halt --all
sleep 1
if pgrep -fl 'heartbeat|immortal --config'; then echo "LEFTOVER after halt --all"; exit 1; fi
echo "no supervisor or service process after halt --all"
sudo find "$RUN" -mindepth 1 | sort
cutover=$(date +%s)
sudo service immortaldir start
wait_state api ready 60
wait_state worker ready 60
sleep 5
echo "== after cutover"
snapshot
# FreeBSD has no /proc/PID/exe. Each supervisor started after the candidate
# was installed and its image path still names the installed file, so it
# runs the candidate binary whose hash is printed here.
for pid in $(ctl status --no-header | awk '{ print $4 }'); do
    exe=$(exe_of "$pid")
    echo "supervisor $pid exe=$exe"
    test "$exe" = /usr/local/sbin/immortal
    test "$(ps -o etimes= -p "$pid")" -le "$(( $(date +%s) - cutover ))"
done
test "$(sha256 -q /usr/local/sbin/immortal)" = "$(sha256 -q "$CAND_BIN/immortal")"
echo "every supervisor runs the candidate binary $(sha256 -q /usr/local/sbin/immortal)"
