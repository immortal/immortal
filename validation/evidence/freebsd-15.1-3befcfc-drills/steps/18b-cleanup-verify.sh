# Re-run of the verification half of 18-cleanup after the first run removed everything (its rc.conf check matched the hostname, now fixed).
set -u
. "$HOME/drill-3befcfc-freebsd/lib.sh"
echo "-- verification"
ls -A /usr/local/sbin /usr/local/bin /usr/local/lib /usr/local/libexec /usr/local/etc/rc.d
for p in "$DEFS" "$RUN" /usr/local/etc/rc.d/immortaldir /var/run/immortaldir.pid /var/db/immortal-drill /var/log/immortal-drill /usr/local/lib/immortal-rollback; do
    if sudo test -e "$p"; then echo "LEFTOVER $p"; exit 1; fi
done
sysrc -a | grep -i immortal | grep -v '^hostname:' | grep . && { echo "LEFTOVER rc.conf setting"; exit 1; }
if pgrep -fl immortal; then echo "LEFTOVER process"; exit 1; fi
echo "VM restored: no Immortal process, rc.d script, rc.conf setting, binary, definition, runtime, or drill file"
