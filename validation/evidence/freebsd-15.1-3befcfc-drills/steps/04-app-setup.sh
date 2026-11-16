# Install the drill application the supervisors run: one heartbeat program,
# its working directory, and its log directory. This is application state,
# not Immortal state.
set -e
sudo install -d -m 0755 /usr/local/libexec/immortal-drill /var/db/immortal-drill /var/db/immortal-drill/api /var/log/immortal-drill
sudo install -m 0755 "$HOME/drill-3befcfc-freebsd/defs/heartbeat" /usr/local/libexec/immortal-drill/heartbeat
ls -ld /usr/local/libexec/immortal-drill /var/db/immortal-drill/api /var/log/immortal-drill
ls -l /usr/local/libexec/immortal-drill/heartbeat
