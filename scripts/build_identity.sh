#!/usr/bin/env bash
# Prints the exports that tell an image build what it is (OUTSIDE_REVIEW_PLAN
# Phase 3): `/health` reports them. A hand-run build needs them too, or the brain
# reports a null commit:
#
#   eval "$(scripts/build_identity.sh)" && docker compose up -d --build
#
# `dirty` means tracked files differ from the commit, so the image holds more
# than that commit says.
set -euo pipefail
cd "$(dirname "$0")/.."
commit=$(git rev-parse HEAD 2>/dev/null || true)
dirty=false
if [ -z "$commit" ]; then dirty=""; elif ! git diff --quiet HEAD -- 2>/dev/null; then dirty=true; fi
echo "export BRAIN_GIT_COMMIT='$commit' BRAIN_GIT_DIRTY='$dirty' BRAIN_BUILD_TIME='$(date -u +%Y-%m-%dT%H:%M:%SZ)'"
