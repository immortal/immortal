# INSTALL.md placed immortallog in /usr/local/bin, but a supervisor without
# log_adapter runs the default adapter from its own directory
# (/usr/local/sbin/immortallog), so the api logger stayed in backoff and the
# service never started. Apply the corrected layout in place and check that
# the stuck supervisor recovers by itself, without a restart.
set -e
. "$HOME/drill-3befcfc/lib.sh"
echo "-- before"
snapshot
sudo install -m 0755 "$PREV_BIN/immortallog" /usr/local/sbin/immortallog
sudo rm /usr/local/bin/immortallog
test "$(installed_revision)" = "$PREV"
wait_state api ready 120
wait_state worker ready 120
sleep 5
echo "-- after"
snapshot
