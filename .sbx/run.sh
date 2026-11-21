#!/usr/bin/env bash
# Compose the local Cursor workload with project mixins.
# Kit Spec set (kind: set) cannot reference local paths, so this script
# is the project-local equivalent of that composition.
#
# Usage (from anywhere):
#   .sbx/run.sh                  # workspace = current directory
#
# github-ssh requires a host SSH agent with a GitHub-accepted key loaded
# (ssh-add -l). Create fails without one because ssh-agent@1 is required.
set -euo pipefail

SBX_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SBX_NAME="simulation-rl78-core-dev"

if sbx ls -q | grep -qw "${SBX_NAME}"; then
  exec sbx rm $SBX_NAME;
fi

exec sbx run "${SBX_DIR}/cursor" \
  --kit "${SBX_DIR}/rust" \
  --kit "${SBX_DIR}/cc" \
  --kit "${SBX_DIR}/serena" \
  --kit "${SBX_DIR}/gh" \
  --kit "${SBX_DIR}/github-ssh" \
  --name "${SBX_NAME}" 
