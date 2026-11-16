# Install the previous candidate 43a7a6e exactly as INSTALL.md "FreeBSD rc.d"
# describes, with the shipped rc.d script and the two strict-v2 drill
# services, and keep a copy of its binaries for the rollback drill.
set -e
. "$HOME/drill-3befcfc-freebsd/lib.sh"
install_release "$PREV_BIN"
test "$(installed_revision)" = "$PREV"
sudo install -d -m 0755 /usr/local/lib/immortal-rollback "$ROLLBACK"
sudo install -m 0755 "$PREV_BIN/immortal" "$PREV_BIN/immortaldir" "$PREV_BIN/immortalctl" "$PREV_BIN/immortallog" "$ROLLBACK/"
sudo install -d -m 0755 "$DEFS"
sudo install -m 0644 "$HOME/drill-3befcfc-freebsd/defs/v2/api.yml" "$HOME/drill-3befcfc-freebsd/defs/v2/worker.yml" "$DEFS/"
ls -la "$DEFS"
sudo install -o root -g wheel -m 0555 "$HOME/immortal-43a7a6e/contrib/freebsd/immortaldir" /usr/local/etc/rc.d/immortaldir
ls -l /usr/local/etc/rc.d/immortaldir
cat /usr/local/etc/rc.d/immortaldir
sudo sysrc immortaldir_enable=YES
sudo sysrc immortaldir_definitions="$DEFS"
sudo sysrc immortaldir_runtime_dir="$RUN"
sudo sysrc immortaldir_supervisor=/usr/local/sbin/immortal
sudo service immortaldir start
sudo service immortaldir status
wait_state api ready 60
wait_state worker ready 60
sleep 5
snapshot
