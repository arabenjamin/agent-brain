#!/usr/bin/env bash
# Look for secrets with gitleaks, run from its image so nothing is installed on
# the host. A false alarm's fingerprint goes in .gitleaksignore.
#
#   scripts/secret_scan.sh            # the whole git history
#   scripts/secret_scan.sh --staged   # what is about to be committed
set -euo pipefail
cd "$(dirname "$0")/.."

docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/repo:ro" \
  zricethezav/gitleaks:v8.28.0 git /repo --redact --no-banner \
  ${1:+--pre-commit "$1"}
