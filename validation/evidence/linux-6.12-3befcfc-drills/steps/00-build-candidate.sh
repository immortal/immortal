export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export RUSTUP_AUTO_INSTALL=0
cd "$HOME/immortal-3befcfc"
git rev-parse HEAD
test -z "$(git status --porcelain)"
rustc --version
cargo --version
cargo build --workspace --release --locked
ls -l target/release/immortal target/release/immortaldir target/release/immortalctl target/release/immortallog
sha256sum target/release/immortal target/release/immortaldir target/release/immortalctl target/release/immortallog
