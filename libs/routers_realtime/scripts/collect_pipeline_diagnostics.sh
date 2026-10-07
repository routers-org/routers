#!/usr/bin/env bash
# Emit synchronized, read-only NATS and Prometheus snapshots as JSON lines.
# Requires local forwards to NATS :8222 and Prometheus :9090.
set -euo pipefail

interval_seconds=${1:-5}
sample_limit=${2:-0}
nats_monitor=${NATS_MONITOR_URL:-http://127.0.0.1:18222}
prometheus=${PROMETHEUS_URL:-http://127.0.0.1:19090}

if [[ ! $interval_seconds =~ ^[1-9][0-9]*$ || ! $sample_limit =~ ^[0-9]+$ ]]; then
  echo 'usage: collect_pipeline_diagnostics.sh [interval_seconds=5] [sample_limit=0 (unlimited)]' >&2
  exit 2
fi
if [[ $(kubectl config current-context) != orbstack ]]; then
  echo 'refusing to sample: Kubernetes context must be orbstack' >&2
  exit 2
fi

metric_selector='{__name__=~"materializer_stage_seconds_(sum|count)|materializer_active|materializer_keyed_queued|commit_stage_seconds_(sum|count)|output_bytes_count|materialized_outputs_total|redis_cpu_.*_seconds_total|redis_commands_duration_seconds_total|nats_varz_cpu|nats_varz_jetstream_stats_api_inflight|node_cpu_seconds_total|node_disk_(io_time_seconds_total|read_bytes_total|written_bytes_total)"}'

for ((sample = 1; sample_limit == 0 || sample <= sample_limit; sample++)); do
  started_at=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
  jsz=$(curl --fail --silent --show-error --max-time 10 "$nats_monitor/jsz?consumers=true" | jq -c '{streams:[.account_details[].stream_detail[]? | {name, messages:.state.messages, consumers:[.consumer_detail[]? | {name, pending:.num_pending, ack_pending:.num_ack_pending, redelivered:.num_redelivered, waiting:.num_waiting}]}]}')
  varz=$(curl --fail --silent --show-error --max-time 10 "$nats_monitor/varz" | jq -c '{cpu, mem, slow_consumers, in_msgs, out_msgs, in_bytes, out_bytes, jetstream_storage:.jetstream.stats.storage, jetstream_memory:.jetstream.stats.memory, jetstream_api:.jetstream.stats.api}')
  connz=$(curl --fail --silent --show-error --max-time 10 "$nats_monitor/connz?limit=2048" | jq -c '{num_connections, total, connections:[.connections[]? | {cid, name, ip, pending_bytes, in_msgs, out_msgs, in_bytes, out_bytes, subscriptions}]}')
  metrics=$(curl --fail --silent --show-error --max-time 10 --get "$prometheus/api/v1/query" --data-urlencode "query=$metric_selector" | jq -ec 'select(.status == "success") | [.data.result[] | {metric, value}]')
  kubelet=$(kubectl --request-timeout=10s get --raw /api/v1/nodes/orbstack/proxy/stats/summary | jq -c '{node_cpu:{time:.node.cpu.time, nano_cores:.node.cpu.usageNanoCores, core_nanoseconds:.node.cpu.usageCoreNanoSeconds, psi:.node.cpu.psi}, pods:[.pods[] | select(.podRef.namespace == "routers" or .podRef.namespace == "routers-local") | {namespace:.podRef.namespace, name:.podRef.name, containers:[.containers[] | {name, cpu_time:.cpu.time, nano_cores:.cpu.usageNanoCores, core_nanoseconds:.cpu.usageCoreNanoSeconds}]}]}')
  finished_at=$(date -u +'%Y-%m-%dT%H:%M:%SZ')

  jq -nc \
    --arg started_at "$started_at" --arg finished_at "$finished_at" \
    --argjson sample "$sample" --argjson jsz "$jsz" \
    --argjson varz "$varz" --argjson connz "$connz" \
    --argjson metrics "$metrics" --argjson kubelet "$kubelet" \
    '{sample: $sample, started_at: $started_at, finished_at: $finished_at,
      raw: ([ $jsz.streams[] | select(.name | startswith("EVENTS-RAW-")) ]),
      matched: ([ $jsz.streams[] | select(.name == "EVENTS-MATCHED") ]),
      nats: $varz, connections: $connz, prometheus: $metrics,
      kubelet: $kubelet}'

  if ((sample_limit == 0 || sample < sample_limit)); then
    sleep "$interval_seconds"
  fi
done
