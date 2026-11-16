# RELEASE.md candidate validation steps 5-7 on the release artifacts: every
# binary's short and long version, the strict-v2 upgrade rehearsal, and no
# leftover process, socket, lock, PID file, runtime directory, or definition.
set -e
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export RUSTUP_AUTO_INSTALL=0
cd "$HOME/immortal-3befcfc"
commit=$(git rev-parse HEAD)
for b in immortal immortaldir immortalctl immortallog; do
    short=$(target/release/$b -V)
    long=$(target/release/$b --version)
    echo "$b -V: $short"
    echo "$b --version: $long"
    test "$short" = "$b 0.1.0"
    case $long in
        *"$commit"*) ;;
        *) echo "long version lacks $commit" >&2; exit 1 ;;
    esac
done
before_tmp=$(ls -A /tmp | sort)
sh scripts/rehearse-upgrade
echo "rehearse-upgrade exit=0"
after_tmp=$(ls -A /tmp | sort)
test "$before_tmp" = "$after_tmp" && echo "no new /tmp entries"
if pgrep -fl 'immortal' ; then echo "leftover immortal process" >&2; exit 1; fi
echo "no immortal processes"
test ! -e /var/run/immortal && echo "no /var/run/immortal"
test ! -e "$HOME/.immortal" && echo "no ~/.immortal" || ls -la "$HOME/.immortal"
test -z "$(git status --porcelain)" && echo "worktree clean"
