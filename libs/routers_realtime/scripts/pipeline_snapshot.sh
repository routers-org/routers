#!/usr/bin/env bash
set -euo pipefail

# Read-only snapshot of the local full-workflow benchmark. Port-forward NATS
# before running; no streams or consumers are created or changed here.
nats_url=${1:-nats://127.0.0.1:14222}
node=${2:-orbstack}

date -u '+snapshot %Y-%m-%dT%H:%M:%SZ'
for stream in EVENTS-RAW-0 EVENTS-RAW-1 EVENTS-RAW-2 EVENTS-RAW-3 EVENTS-MATCHED; do
  nats --server "$nats_url" stream info "$stream" --json |
    jq -r '["stream", .config.name, .state.messages, .state.bytes, .state.consumer_count, .state.first_ts, .state.last_ts] | @tsv'
done

for spec in 'EVENTS-MATCHED materializer' 'EVENTS-RAW-0 orchestrator-raw-n64-s0-sh0'; do
  read -r stream consumer <<< "$spec"
  nats --server "$nats_url" consumer info "$stream" "$consumer" --json |
    jq -r --arg stream "$stream" '["consumer", $stream, .name, .num_pending, .num_ack_pending, .num_redelivered, .config.max_ack_pending] | @tsv'
done

kubectl get --raw "/api/v1/nodes/$node/proxy/stats/summary" |
  jq -r '.pods[] | select(.podRef.namespace == "routers" or (.podRef.namespace == "routers-local" and (.podRef.name | startswith("nats-") or startswith("valkey-")))) | ["cpu", .podRef.namespace, .podRef.name, (([.containers[].cpu.usageNanoCores // 0] | add) / 1000000000), ([.containers[].cpu.usageCoreNanoSeconds // 0] | add)] | @tsv'
