#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PYTHONPATH="${ROOT}/python${PYTHONPATH:+:$PYTHONPATH}"
exec python3 -m topology_dsl create "$@"
