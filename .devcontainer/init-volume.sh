#!/bin/bash
set -euo pipefail  # Exit on error, undefined vars, and pipeline failures
IFS=$'\n\t'       # Stricter word splitting

MOUNT_DIRS=(
    "/home/node/.cursor"
)

for MOUNT_DIR in "${MOUNT_DIRS[@]}"; do
    chown -R node:node $MOUNT_DIR
done