# Install the previous candidate 43a7a6e exactly as INSTALL.md describes, with
# the shipped systemd unit and the two strict-v2 drill services, and keep a
# copy of its binaries for the rollback drill.
set -e
. "$HOME/drill-3befcfc/lib.sh"
install_release "$PREV_BIN"
test "$(installed_revision)" = "$PREV"
sudo install -d -m 0755 "$ROLLBACK"
sudo install -m 0755 "$PREV_BIN/immortal" "$PREV_BIN/immortaldir" "$PREV_BIN/immortalctl" "$PREV_BIN/immortallog" "$ROLLBACK/"
sudo install -d -m 0755 /etc/immortal
sudo install -m 0644 "$HOME/drill-3befcfc/defs/v2/api.yml" "$HOME/drill-3befcfc/defs/v2/worker.yml" /etc/immortal/
ls -la /etc/immortal
sudo install -m 0644 "$HOME/immortal-43a7a6e/contrib/systemd/immortaldir.service" /etc/systemd/system/immortaldir.service
cat /etc/systemd/system/immortaldir.service
sudo systemctl daemon-reload
sudo systemctl enable --now immortaldir
wait_state api ready 60
wait_state worker ready 60
systemctl status immortaldir --no-pager --lines=20
sleep 5
snapshot
