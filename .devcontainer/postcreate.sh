#!/usr/bin/env bash
set -euo pipefail

# One-time DevPod provisioning: shell/editor dependencies, mise-managed tools,
# Rust components, and personal dotfiles.

export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

sh "$REPO_ROOT/.devcontainer/configure-git.sh"

# Named volumes can initially belong to root, so fix ownership before writing.
sudo mkdir -p \
    "$HOME/.local/bin" "$HOME/.local/share" "$HOME/.cache" "$HOME/.config"
sudo chown -R "$(id -u):$(id -g)" \
    "$HOME/.local" "$HOME/.cache" "$HOME/.config" \
    /home/vscode/.cargo /home/vscode/.rustup 2>/dev/null || true

# Match the reusable shell and Neovim environment from pg_exporter.
sudo apt-get update
sudo apt-get install -y \
    build-essential ca-certificates curl delta dnsutils fd-find fzf git gnupg iputils-ping jq \
    libbz2-dev libcap2-bin libffi-dev liblzma-dev libnss3-tools libreadline-dev libsqlite3-dev \
    libssl-dev luarocks make netcat-openbsd openssh-client pkg-config rsync \
    tig tmux unzip wget xz-utils yq zip zlib1g-dev

command -v zsh >/dev/null 2>&1 && sudo chsh -s "$(command -v zsh)" vscode || true

# Install mise tools with retries so an optional download cannot brick DevPod.
if ! command -v mise >/dev/null 2>&1; then
    curl -fsSL https://mise.run | sh
fi
mise trust --yes
if ! mise install; then
    echo "mise install failed; retrying once..." >&2
    if ! mise install; then
        echo "mise install still failing; installing essential tools individually." >&2
        mise install just || true
        mise install || true
    fi
fi
mise prune --yes || true
mise reshim || true

export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
if ! command -v just >/dev/null 2>&1; then
    echo "ERROR: 'just' is not available after mise install." >&2
    exit 1
fi

# Rust comes from the devcontainer image rather than mise.
IMAGE_RUSTUP="$(command -v rustup || echo /usr/local/cargo/bin/rustup)"
case "$IMAGE_RUSTUP" in
*/.local/share/mise/*) IMAGE_RUSTUP=/usr/local/cargo/bin/rustup ;;
esac
"$IMAGE_RUSTUP" component add rustfmt clippy rust-analyzer
"$IMAGE_RUSTUP" target add x86_64-unknown-freebsd

# Expose mise tools in interactive, login, and non-interactive shells.
sudo tee /etc/profile.d/mise.sh >/dev/null <<'EOF'
export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
EOF
grep -qxF 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' ~/.bashrc 2>/dev/null ||
    echo 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' >>~/.bashrc
grep -qxF 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' ~/.zshenv 2>/dev/null ||
    echo 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' >>~/.zshenv

cargo fetch || true

# Apply personal dotfiles. This provides the slick prompt, zinit, mise shell
# activation, Neovim configuration, and related command-line setup.
apply_dotfiles() {
    set +e
    dotfiles_repo="${DEVPOD_DOTFILES:-https://github.com/nbari/dotfiles-devpod.git}"
    [ "$dotfiles_repo" != "" ] || {
        echo "No dotfiles repository configured; skipping."
        return 0
    }

    if ! command -v chezmoi >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/chezmoi" ]; then
        for attempt in 1 2 3; do
            sh -c "$(curl -fsSL get.chezmoi.io)" -- -b "$HOME/.local/bin" && break
            echo "chezmoi install attempt ${attempt} failed; retrying..." >&2
            sleep 3
        done
    fi

    chezmoi_bin="$(command -v chezmoi || echo "$HOME/.local/bin/chezmoi")"
    if [ ! -x "$chezmoi_bin" ]; then
        echo "chezmoi is unavailable; skipping dotfiles." >&2
        return 0
    fi

    "$chezmoi_bin" init --apply --force "$dotfiles_repo" ||
        echo "chezmoi apply failed; rerun: chezmoi init --apply --force ${dotfiles_repo}" >&2
}
(apply_dotfiles)

# Dotfiles can update Git configuration, so restore forwarded identity last.
sh "$REPO_ROOT/.devcontainer/configure-git.sh"

echo "✓ Development environment ready: Rust, mise tools, Neovim, and slick prompt."
echo "  Run: just ci"
