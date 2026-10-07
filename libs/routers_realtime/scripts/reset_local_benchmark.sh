#!/usr/bin/env bash
set -euo pipefail

nats_url=${1:-nats://127.0.0.1:14222}
matcher_replicas=${2:-4}

[[ $(kubectl config current-context) == orbstack ]] || {
  echo 'Refusing to reset a context other than orbstack' >&2
  exit 1
}

[[ $matcher_replicas =~ ^[1-9][0-9]*$ ]] || {
  echo 'Matcher replica count must be a positive integer' >&2
  exit 1
}
current_replicas=$(kubectl -n routers get deployment matcher-sydney -o jsonpath='{.spec.replicas}')
[[ $current_replicas == "$matcher_replicas" ]] || {
  echo "Matcher deployment has $current_replicas replicas, not $matcher_replicas" >&2
  exit 1
}

kubectl -n routers-local get pod valkey-000-primary-0 -o json |
  jq -e '.spec.volumes[] | select(.name == "valkey-data") | .emptyDir != null' >/dev/null
store_pods=(valkey-000-primary-0)
if kubectl -n routers-local get pod valkey-001-primary-0 >/dev/null 2>&1; then
  kubectl -n routers-local get pod valkey-001-primary-0 -o json |
    jq -e '.spec.volumes[] | select(.name == "valkey-data") | .emptyDir != null' >/dev/null
  store_pods+=(valkey-001-primary-0)
fi
if kubectl -n routers-local get pod valkey-view-primary-0 >/dev/null 2>&1; then
  kubectl -n routers-local get pod valkey-view-primary-0 -o json |
    jq -e '.spec.volumes[] | select(.name == "valkey-data") | .emptyDir != null' >/dev/null
  store_pods+=(valkey-view-primary-0)
fi

expected='["EVENTS-MATCHED","EVENTS-RAW-0","EVENTS-RAW-1","EVENTS-RAW-2","EVENTS-RAW-3"]'
actual=$(nats --server "$nats_url" stream ls --json | jq -c 'sort')
[[ $actual == "$expected" ]] || {
  echo "Unexpected NATS streams: $actual" >&2
  exit 1
}

kubectl -n routers scale statefulset/orchestrator --replicas=0
kubectl -n routers scale deployment/matcher-sydney --replicas=0
kubectl -n routers scale deployment/materializer --replicas=0

while [[ $(kubectl -n routers get pods -o name | wc -l | tr -d ' ') != 0 ]]; do
  sleep 1
done

for stream in EVENTS-MATCHED EVENTS-RAW-0 EVENTS-RAW-1 EVENTS-RAW-2 EVENTS-RAW-3; do
  nats --server "$nats_url" stream purge "$stream" --force >/dev/null
done

for stream_index in 0 1 2 3; do
  for local_index in {0..15}; do
    shard_index=$((16 * stream_index + local_index))
    nats --server "$nats_url" consumer rm "EVENTS-RAW-$stream_index" "orchestrator-raw-n64-s$stream_index-sh$shard_index" --force
  done
done

kubectl -n routers-local delete pod nats-0 "${store_pods[@]}"
for attempt in {1..180}; do
  if kubectl -n routers-local get pod nats-0 "${store_pods[@]}" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
ready_pods=(pod/nats-0)
for pod in "${store_pods[@]}"; do
  ready_pods+=("pod/$pod")
done
kubectl -n routers-local wait --for=condition=Ready "${ready_pods[@]}" --timeout=180s

kubectl -n routers scale statefulset/orchestrator --replicas=4
kubectl -n routers scale deployment/matcher-sydney --replicas="$matcher_replicas"
kubectl -n routers scale deployment/materializer --replicas=1
kubectl -n routers rollout status statefulset/orchestrator --timeout=180s
kubectl -n routers rollout status deployment/matcher-sydney --timeout=180s
kubectl -n routers rollout status deployment/materializer --timeout=180s

echo 'Reset complete. Restart the localhost NATS port-forward, then run verify_local_benchmark.sh before replay.'
