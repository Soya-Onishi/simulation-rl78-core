#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

usage() {
  echo "Usage: $(basename "$0") <script.py> [--server URL] [--open]" >&2
  exit 2
}

if [[ $# -eq 0 ]]; then
  echo "create-sim: missing topology script" >&2
  usage
fi

script=""
skip_next=0
for arg in "$@"; do
  if [[ "$skip_next" -eq 1 ]]; then
    skip_next=0
    continue
  fi
  case "$arg" in
    --server)
      skip_next=1
      ;;
    --open) ;;
    -*)
      echo "create-sim: unknown argument $arg" >&2
      usage
      ;;
    *.py)
      if [[ -n "$script" ]]; then
        echo "create-sim: unexpected extra argument: $arg" >&2
        usage
      fi
      script="$arg"
      ;;
    *)
      echo "create-sim: topology script must be a .py path: $arg" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$script" ]]; then
  echo "create-sim: missing topology script" >&2
  usage
fi

if [[ ! -f "$script" ]]; then
  echo "create-sim: script not found: $script" >&2
  exit 2
fi

PYTHONPATH="${ROOT}/python${PYTHONPATH:+:$PYTHONPATH}" \
  exec python3 -m topology_dsl create "$@"
