#!/usr/bin/env bash
# Delete all matcher-* deployments in routers-dev.
# Usage:
#   ./teardown-matchers.sh                  # delete all shard matchers
#   ./teardown-matchers.sh r3g              # delete only shards with this prefix
set -euo pipefail

NAMESPACE=routers-dev
PREFIX="${1:-}"

# Match both the legacy plain "matcher" deployment and all "matcher-{shard}" ones.
kubectl get deployments -n "$NAMESPACE" -o name \
  | grep -E "deployment.apps/matcher(-|$)" \
  | { [[ -n "$PREFIX" ]] && grep "matcher-${PREFIX}" || cat; } \
  | xargs -r kubectl delete -n "$NAMESPACE"
