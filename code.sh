#!/usr/bin/env bash

set -euo pipefail

# launch sandbox if not launched
./.sbx/run.sh

# run vscode on remote ssh
code --folder-uri "vscode-remote://ssh-remote+simulation-rl78-core-dev.sbx$PWD"
