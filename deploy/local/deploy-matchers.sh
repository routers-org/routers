#!/usr/bin/env bash
# Deploy one matcher pod per shard listed in manifest.txt.
# Usage:
#   ./deploy-matchers.sh                    # deploy all manifest shards
#   ./deploy-matchers.sh r3g                # deploy only shards with this prefix
#   ./deploy-matchers.sh "" 5              # deploy first 5 shards (smoke test)
#   ./deploy-matchers.sh r3g 5             # deploy first 5 r3g shards
#   SHARD_CACHE=/other/path ./deploy-matchers.sh
set -euo pipefail

NAMESPACE=routers-dev
SHARD_CACHE="${SHARD_CACHE:-/Users/benji/Documents/personal/routers/target/shard_cache}"
MANIFEST="$SHARD_CACHE/manifest.txt"
PREFIX="${1:-}"
LIMIT="${2:-0}"   # 0 = no limit

if [[ ! -f "$MANIFEST" ]]; then
  echo "manifest.txt not found at $MANIFEST" >&2
  exit 1
fi

# Pre-compute the list so count is available after the pipe subshell exits.
mapfile -t SHARDS < <(
  while IFS= read -r filename; do
    shard="${filename%.shard.rt}"
    [[ -n "$PREFIX" && "${shard#"$PREFIX"}" == "$shard" ]] && continue
    echo "$shard"
  done < "$MANIFEST" \
  | { [[ "$LIMIT" -gt 0 ]] && head -n "$LIMIT" || cat; }
)

count=${#SHARDS[@]}
{
  for shard in "${SHARDS[@]}"; do
    cat <<EOF
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: matcher-${shard}
  namespace: ${NAMESPACE}
  labels:
    app: matcher
    shard: "${shard}"
spec:
  replicas: 1
  selector:
    matchLabels:
      app: matcher
      shard: "${shard}"
  template:
    metadata:
      labels:
        app: matcher
        shard: "${shard}"
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "9092"
        prometheus.io/path: "/metrics"
    spec:
      containers:
        - name: matcher
          image: routers-matcher:latest
          imagePullPolicy: Never
          ports:
            - name: metrics
              containerPort: 9092
          env:
            - name: NATS_URL
              value: "nats://nats.routers-dev.svc.cluster.local:4222"
            - name: SHARD_DIR
              value: "/shards"
            - name: SHARD_PRECISION
              value: "5"
            - name: METRICS_ADDR
              value: "0.0.0.0:9092"
            - name: OWNED_SHARD
              value: "${shard}"
            - name: RUST_LOG
              value: "debug"
          volumeMounts:
            - name: shards
              mountPath: /shards
              readOnly: true
          resources:
            requests:
              cpu: 250m
              memory: 256Mi
            limits:
              cpu: 1000m
              memory: 1536Mi
      volumes:
        - name: shards
          hostPath:
            path: ${SHARD_CACHE}
            type: Directory
EOF
  done
} | kubectl apply -f -

echo "Applied $count matcher deployment(s)${PREFIX:+ (prefix: $PREFIX)}${LIMIT:+ (limit: $LIMIT)}."
