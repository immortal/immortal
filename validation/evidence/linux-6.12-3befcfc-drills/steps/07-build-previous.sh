# Build the previous candidate, 43a7a6e, as the installation that the upgrade
# drill replaces and the rollback drill restores.
set -e
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export RUSTUP_AUTO_INSTALL=0
cd "$HOME/immortal-43a7a6e"
git rev-parse HEAD
test "$(git rev-parse HEAD)" = 43a7a6eaf05838efadb58cfbd6ef73cd6c3756c9
test -z "$(git status --porcelain)"
cargo build --workspace --release --locked
for b in immortal immortaldir immortalctl immortallog; do
    target/release/$b --version
done
sha256sum target/release/immortal target/release/immortaldir target/release/immortalctl target/release/immortallog
