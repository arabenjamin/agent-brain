#!/usr/bin/env bash
# Print the brain's API key for host scripts (OUTSIDE_REVIEW_PLAN Phase 0):
# MCP_API_KEY from the environment, else from the repo's .env. Prints nothing
# when neither has it, so a caller sends no Authorization header.
#
#   KEY=$(scripts/brain_key.sh)
set -euo pipefail
if [ -n "${MCP_API_KEY:-}" ]; then
    printf '%s' "$MCP_API_KEY"
    exit 0
fi
env_file="$(cd "$(dirname "$0")/.." && pwd)/.env"
[ -f "$env_file" ] || exit 0
grep -E '^MCP_API_KEY=' "$env_file" | tail -1 | cut -d= -f2- | sed -E "s/^(['\"])(.*)\1$/\2/" | tr -d '\n'
