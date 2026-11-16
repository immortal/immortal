export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export RUSTUP_AUTO_INSTALL=0
cd "$HOME/immortal-3befcfc"
git rev-parse HEAD
test "$(git rev-parse HEAD)" = 3befcfc4b01b3c66a18624229795d0e1301e89a6 || exit 1
test -z "$(git status --porcelain)" || exit 1
rustc --version
cargo --version
cargo build --workspace --release --locked || exit 1
ls -l target/release/immortal target/release/immortaldir target/release/immortalctl target/release/immortallog
sha256 target/release/immortal target/release/immortaldir target/release/immortalctl target/release/immortallog
