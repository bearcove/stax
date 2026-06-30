#!/usr/bin/env bash
# Install the stax agent skill into the local agent skill directories as
# symlinks, so they always track this checkout (pull the repo, skill updates).
#
#   ./skills/install.sh
#
# Targets Claude Code (~/.claude/skills) and Codex (~/.codex/skills). Pass extra
# skill roots as arguments to install into them too.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
src="$here/stax"

roots=("$HOME/.claude/skills" "$HOME/.codex/skills" "$@")

for root in "${roots[@]}"; do
  mkdir -p "$root"
  ln -sfn "$src" "$root/stax"
  echo "linked $root/stax -> $src"
done
