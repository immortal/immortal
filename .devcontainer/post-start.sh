#!/usr/bin/env bash
set -uo pipefail

# Runs on every DevPod start. Keep this best-effort so shell configuration cannot
# prevent the workspace from starting.

export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
cd /workspaces/immortal 2>/dev/null || exit 0

sh .devcontainer/configure-git.sh || true

echo "✓ Workspace ready with mise tools and the slick prompt."
echo "  Run: just ci"

exit 0
