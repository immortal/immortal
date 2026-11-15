# Upgrade drill 43a7a6e -> 3befcfc. First the manager-only path: install the
# candidate binaries and restart immortaldir; KillMode=process must keep every
# supervisor and service process, and the new immortalctl must control the old
# supervisors. Then the full cutover, mirroring INSTALL.md rollback: stop the
# manager, halt every service, start the manager so each service restarts on
# the candidate supervisor.
set -e
. "$HOME/drill-3befcfc/lib.sh"
echo "== before upgrade"
snapshot
before=$(ctl status --no-header | awk '{ print $2, $4, $5 }')
install_release "$CAND_BIN"
test "$(installed_revision)" = "$CAND"
sudo systemctl restart immortaldir
sleep 3
systemctl is-active immortaldir
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
sudo systemctl stop immortaldir
ctl halt --all
sleep 1
if pgrep -fl 'heartbeat|immortal --config'; then echo "LEFTOVER after halt --all"; exit 1; fi
echo "no supervisor or service process after halt --all"
sudo find /run/immortal -mindepth 1 | sort
sudo systemctl start immortaldir
wait_state api ready 60
wait_state worker ready 60
sleep 5
echo "== after cutover"
snapshot
cand_hash=$(sha256sum /usr/local/sbin/immortal | cut -d ' ' -f 1)
for pid in $(ctl status --no-header | awk '{ print $4 }'); do
    test "$(sudo sha256sum "/proc/$pid/exe" | cut -d ' ' -f 1)" = "$cand_hash"
done
echo "every supervisor runs the candidate binary $cand_hash"
