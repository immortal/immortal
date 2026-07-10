#!/usr/bin/env sh
set -eu

# Apply optional identity and SSH signing settings forwarded by scripts/dev-up.

if [ "${GIT_USER_NAME:-}" != "" ]; then
    git config --global --replace-all user.name "$GIT_USER_NAME"
fi

if [ "${GIT_USER_EMAIL:-}" != "" ]; then
    git config --global --replace-all user.email "$GIT_USER_EMAIL"
fi

signing_key="${GIT_SIGNING_KEY:-}"
if [ "$signing_key" = "" ]; then
    signing_key="$(git config --global --get user.signingkey 2>/dev/null || true)"
fi

if [ "$signing_key" != "" ]; then
    signer="$(command -v ssh-keygen || printf '%s' ssh-keygen)"
    git config --global --replace-all gpg.format ssh
    git config --global --replace-all gpg.ssh.program "$signer"
    git config --global --replace-all user.signingkey "$signing_key"
    git config --global --replace-all commit.gpgsign true

    allowed_signers="$HOME/.config/git/allowed_signers"
    mkdir -p "$(dirname "$allowed_signers")"
    identity="${GIT_USER_EMAIL:-$(git config --global --get user.email 2>/dev/null || printf '%s' '*')}"
    printf '%s %s\n' "$identity" "$signing_key" >"$allowed_signers"
    git config --global --replace-all gpg.ssh.allowedSignersFile "$allowed_signers"

    # Repository configuration wins if DevPod later injects its signing helper.
    if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git config --replace-all gpg.ssh.program "$signer"
    fi
fi
