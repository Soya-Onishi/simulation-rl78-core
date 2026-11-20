#!/usr/bin/env bash
# Compose the local Cursor workload with rust / cc / serena mixins.
# Kit Spec set (kind: set) cannot reference local paths, so this script
# is the project-local equivalent of that composition.
#
# Usage (from anywhere):
#   .sbx/run.sh                  # workspace = current directory
#   .sbx/run.sh .                # explicit workspace
#   .sbx/run.sh --name my-dev .  # extra sbx run flags, then paths
set -euo pipefail

SBX_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

exec sbx run "${SBX_DIR}/cursor" \
  --kit "${SBX_DIR}/rust" \
  --kit "${SBX_DIR}/cc" \
  --kit "${SBX_DIR}/serena" \
  "$@"
