#!/usr/bin/env bash
set -euo pipefail

# Reproducible local-cluster orchestrator replica sweep. This intentionally
# keeps matcher and materializer Helm values unchanged and varies only the
# orchestrator StatefulSet fleet. Every run starts from empty JetStream and
# Valkey state; opt in with CONFIRM_LOCAL_RESET=1.

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
app_namespace=${APP_NAMESPACE:-routers}
infra_namespace=${INFRA_NAMESPACE:-routers-local}
release=${RELEASE:-routers-realtime}
nats_release=${NATS_RELEASE:-nats}
nats_chart_version=${NATS_CHART_VERSION:-2.14.2}
nats_port=${NATS_LOCAL_PORT:-14222}
nats_url="nats://127.0.0.1:${nats_port}"
prom_service=${PROM_SERVICE:-kube-prometheus-stack-prometheus}
node=${KUBE_NODE:-orbstack}
streams=${RAW_STREAMS:-4}
shards=${ORCHESTRATOR_SHARDS:-64}
shard_queue_capacity=${SHARD_QUEUE_CAPACITY:-8}
sample_seconds=${SAMPLE_SECONDS:-5}
drain_timeout=${DRAIN_TIMEOUT_SECONDS:-1800}
bench_rows=${BENCH_ROWS:-300000}
replay_lanes=${REPLAY_LANES:-64}
replay_rate=${REPLAY_RATE:-5000}
data_file=${DATA_FILE:-"${repo_root}/libs/routers_realtime/data/sydney-dump-2026-thesis.csv"}
replay_bin=${REPLAY_BIN:-"${repo_root}/target/release/replay"}
results_root=${RESULTS_DIR:-"${repo_root}/target/orchestrator-sweep"}
exporter_values="${repo_root}/infrastructure/dev/nats-benchmark-values.yaml"
pf_pid=""
sampler_pid=""

usage() {
  cat <<'EOF'
Usage:
  infrastructure/dev/orchestrator-sweep.sh configure-exporter
  infrastructure/dev/orchestrator-sweep.sh check [replicas]
  CONFIRM_LOCAL_RESET=1 infrastructure/dev/orchestrator-sweep.sh run [1 2 4 8]

Useful overrides:
  BENCH_ROWS=300000       Same leading CSV rows for every topology; 0 uses all.
  REPLAY_RATE=5000        Fixed fleet-wide events/s; 0 selects flood mode.
  RESULTS_DIR=path        Output root (default target/orchestrator-sweep).
  DRAIN_TIMEOUT_SECONDS=1800
  ORCHESTRATOR_SHARDS=64  Expected broker-consumer shard count.
  SHARD_QUEUE_CAPACITY=8  Per-partition route buffer.
EOF
}

cleanup() {
  if [[ -n "$sampler_pid" ]]; then
    kill "$sampler_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true
  fi
  if [[ -n "$pf_pid" ]]; then
    kill "$pf_pid" 2>/dev/null || true
    wait "$pf_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

require_tools() {
  local tool
  for tool in kubectl helm nats jq awk; do
    command -v "$tool" >/dev/null || {
      echo "missing required tool: $tool" >&2
      exit 1
    }
  done
  [[ -x "$replay_bin" ]] || {
    echo "replay binary is missing: $replay_bin (build it with cargo build --release -p routers_realtime --bin replay)" >&2
    exit 1
  }
  [[ -f "$data_file" ]] || {
    echo "replay data is missing: $data_file" >&2
    exit 1
  }
}

check_replay_features() {
  local help
  help=$($replay_bin --help)
  [[ "$help" == *"--rate"* && "$help" == *"--summary-json"* ]] || {
    echo "replay binary lacks --rate/--summary-json; rebuild the release binary from the settled source" >&2
    exit 1
  }
}

start_nats_forward() {
  if [[ -n "$pf_pid" ]]; then
    return
  fi
  kubectl -n "$infra_namespace" port-forward svc/nats "${nats_port}:4222" \
    >"${TMPDIR:-/tmp}/routers-nats-port-forward.log" 2>&1 &
  pf_pid=$!
  for _ in $(seq 1 60); do
    if nats --server "$nats_url" stream ls --json >/dev/null 2>&1; then
      return
    fi
    sleep 0.25
  done
  echo "NATS port-forward did not become ready" >&2
  exit 1
}

prom_path() {
  printf '/api/v1/namespaces/%s/services/http:%s:9090/proxy' "$infra_namespace" "$prom_service"
}

prom_query() {
  local query=$1 encoded
  encoded=$(jq -rn --arg query "$query" '$query|@uri')
  kubectl get --raw "$(prom_path)/api/v1/query?query=${encoded}"
}

prom_scalar() {
  prom_query "$1" | jq -r '([.data.result[].value[1] | tonumber] | add) // 0'
}

valkey_primary_pods() {
  kubectl -n "$infra_namespace" get pods \
    -l 'app.kubernetes.io/name=valkey,app.kubernetes.io/component=primary' \
    -o json | jq -r '.items[] | select(.status.phase=="Running") | .metadata.name' | sort
}

check_valkey_fleet() {
  local configured pods endpoint id
  configured=$(kubectl -n "$app_namespace" get statefulset orchestrator -o json | jq -r \
    '.spec.template.spec.containers[] | select(.name=="orchestrator") | .env[] | select(.name=="VALKEY") | .value')
  mapfile -t pods < <(valkey_primary_pods)
  [[ ${#pods[@]} -gt 0 ]] || {
    echo "no running Valkey primary pods discovered" >&2
    return 1
  }
  IFS=',' read -r -a endpoints <<<"$configured"
  [[ ${#pods[@]} == ${#endpoints[@]} ]] || {
    echo "Valkey fleet mismatch: ${#endpoints[@]} configured endpoint(s), ${#pods[@]} primary pod(s)" >&2
    return 1
  }
  for endpoint in "${endpoints[@]}"; do
    id=${endpoint%%=*}
    printf '%s\n' "${pods[@]}" | grep -q "^${id}-primary-" || {
      echo "configured Valkey endpoint $id has no matching primary pod" >&2
      return 1
    }
  done
  echo "Valkey primaries: ${pods[*]}"
}

valkey_on_pod() {
  local pod=$1
  shift
  kubectl -n "$infra_namespace" exec "$pod" -c valkey -- \
    /opt/bitnami/valkey/bin/redis-cli --raw "$@"
}

assert_valkey_empty() {
  local pod size
  while IFS= read -r pod; do
    size=$(valkey_on_pod "$pod" DBSIZE | tr -d '\r')
    [[ "$size" == 0 ]] || {
      echo "Valkey reset incomplete on $pod: DBSIZE=$size" >&2
      return 1
    }
  done < <(valkey_primary_pods)
}

clear_valkey_pod() {
  local pod=$1 removed
  # Bitnami disables FLUSHALL/FLUSHDB. Workloads are stopped before this runs,
  # so an exhaustive db0 key snapshot can be unlinked in bounded batches.
  removed=$(valkey_on_pod "$pod" EVAL '
    local keys = redis.call("KEYS", "*")
    for first = 1, #keys, 500 do
      redis.call("UNLINK", unpack(keys, first, math.min(first + 499, #keys)))
    end
    return #keys
  ' 0 | tr -d '\r')
  [[ "$removed" =~ ^[0-9]+$ ]] || {
    echo "Valkey clear failed on $pod: $removed" >&2
    return 1
  }
  echo "Valkey cleared on $pod: $removed key(s)"
}

capture_range() {
  local output=$1 query=$2 start=$3 end=$4 encoded
  encoded=$(jq -rn --arg query "$query" '$query|@uri')
  kubectl get --raw "$(prom_path)/api/v1/query_range?query=${encoded}&start=${start}&end=${end}&step=${sample_seconds}" >"$output"
}

stream_state() {
  nats --server "$nats_url" stream info "$1" --json --state | jq '.state'
}

stream_messages() {
  stream_state "$1" 2>/dev/null | jq -r '.messages // 0'
}

raw_consumer_ack_total() {
  local index response count total=0
  for index in $(seq 0 $((streams - 1))); do
    response=$(nats --server "$nats_url" request \
      "\$JS.API.CONSUMER.LIST.EVENTS-RAW-${index}" '{}' --raw --count 1)
    count=$(jq -r '.consumers | length' <<<"$response")
    [[ "$count" == $((shards / streams)) ]] || {
      echo "EVENTS-RAW-${index} consumer API returned $count consumers" >&2
      return 1
    }
    total=$((total + $(jq '[.consumers[].ack_floor.consumer_seq] | add // 0' <<<"$response")))
  done
  printf '%s\n' "$total"
}

orchestrator_instance_regex() {
  kubectl -n "$app_namespace" get pods -l app=orchestrator -o json \
    | jq -r '[.items[].metadata.name] | sort | join("|")'
}

configure_exporter() {
  echo "Applying lightweight benchmark exporter profile (stream JSZ, no consumer JSZ)"
  helm upgrade "$nats_release" nats/nats -n "$infra_namespace" \
    --version "$nats_chart_version" -f "$exporter_values"
  kubectl -n "$infra_namespace" rollout status statefulset/nats --timeout=180s
}

check_exporter() {
  local args
  args=$(kubectl -n "$infra_namespace" get pod nats-0 \
    -o jsonpath='{.spec.containers[?(@.name=="prom-exporter")].args}')
  if [[ "$args" == *"-jsz=all"* || "$args" != *"-jsz=streams"* ]]; then
    echo "unsafe exporter profile: expected -jsz=streams and no -jsz=all" >&2
    echo "run: infrastructure/dev/orchestrator-sweep.sh configure-exporter" >&2
    return 1
  fi
}

check_topology() {
  local replicas=$1 raw_consumers expected_per_stream total_subjects result_consumers job_consumers
  [[ $((1024 % replicas)) -eq 0 ]] || {
    echo "replicas must divide 1024: $replicas" >&2
    return 1
  }
  [[ $((shards % streams)) -eq 0 ]] || {
    echo "shards must divide evenly across raw streams: shards=$shards streams=$streams" >&2
    return 1
  }
  [[ $((shards % replicas)) -eq 0 ]] || {
    echo "replicas would split a consumer shard: shards=$shards replicas=$replicas" >&2
    return 1
  }

  local ready fleet_values shard_values pods
  ready=$(kubectl -n "$app_namespace" get statefulset orchestrator -o jsonpath='{.status.readyReplicas}')
  [[ "${ready:-0}" == "$replicas" ]] || {
    echo "orchestrator readiness mismatch: wanted $replicas, got ${ready:-0}" >&2
    return 1
  }
  pods=$(kubectl -n "$app_namespace" get pods -l app=orchestrator -o json | jq -r '.items[].metadata.name' | sort)
  [[ $(wc -w <<<"$pods" | tr -d ' ') == "$replicas" ]] || {
    echo "orchestrator pod count mismatch" >&2
    return 1
  }
  fleet_values=$(kubectl -n "$app_namespace" get pods -l app=orchestrator -o json | jq -r \
    '.items[].spec.containers[] | select(.name=="orchestrator") | .env[] | select(.name=="FLEET") | .value' | sort -u)
  [[ "$fleet_values" == "$replicas" ]] || {
    echo "FLEET env mismatch: expected $replicas, got $fleet_values" >&2
    return 1
  }
  shard_values=$(kubectl -n "$app_namespace" get pods -l app=orchestrator -o json | jq -r \
    '.items[].spec.containers[] | select(.name=="orchestrator") | .env[] | select(.name=="SHARDS") | .value' | sort -u)
  [[ "$shard_values" == "$shards" ]] || {
    echo "SHARDS env mismatch: expected $shards, got $shard_values" >&2
    return 1
  }
  if [[ -n "${EXPECTED_ORCHESTRATOR_CPU_REQUEST:-}" ]]; then
    local cpu_requests
    cpu_requests=$(kubectl -n "$app_namespace" get pods -l app=orchestrator -o json | jq -r \
      '.items[].spec.containers[] | select(.name=="orchestrator") | .resources.requests.cpu' | sort -u)
    [[ "$cpu_requests" == "$EXPECTED_ORCHESTRATOR_CPU_REQUEST" ]] || {
      echo "orchestrator CPU request mismatch: expected $EXPECTED_ORCHESTRATOR_CPU_REQUEST, got $cpu_requests" >&2
      return 1
    }
  fi
  if [[ -n "${EXPECTED_ORCHESTRATOR_MEMORY_REQUEST:-}" ]]; then
    local memory_requests
    memory_requests=$(kubectl -n "$app_namespace" get pods -l app=orchestrator -o json | jq -r \
      '.items[].spec.containers[] | select(.name=="orchestrator") | .resources.requests.memory' | sort -u)
    [[ "$memory_requests" == "$EXPECTED_ORCHESTRATOR_MEMORY_REQUEST" ]] || {
      echo "orchestrator memory request mismatch: expected $EXPECTED_ORCHESTRATOR_MEMORY_REQUEST, got $memory_requests" >&2
      return 1
    }
  fi

  expected_per_stream=$((shards / streams))
  total_subjects=0
  for index in $(seq 0 $((streams - 1))); do
    local info subjects
    info=$(nats --server "$nats_url" stream info "EVENTS-RAW-${index}" --json)
    raw_consumers=$(jq -r '.state.consumer_count' <<<"$info")
    subjects=$(jq -r '.config.subjects | length' <<<"$info")
    [[ "$raw_consumers" == "$expected_per_stream" ]] || {
      echo "EVENTS-RAW-${index} consumers: expected $expected_per_stream, got $raw_consumers" >&2
      return 1
    }
    total_subjects=$((total_subjects + subjects))
  done
  [[ "$total_subjects" == 1024 ]] || {
    echo "raw subject coverage mismatch: expected 1024, got $total_subjects" >&2
    return 1
  }

  result_consumers=$(stream_state SOLVE-RESULTS | jq -r '.consumer_count')
  job_consumers=$(stream_state SOLVE-JOBS-sydney | jq -r '.consumer_count')
  [[ "$result_consumers" == "$shards" ]] || {
    echo "SOLVE-RESULTS consumers: expected $shards, got $result_consumers" >&2
    return 1
  }
  [[ "$job_consumers" == 1 ]] || {
    echo "SOLVE-JOBS-sydney consumers: expected 1, got $job_consumers" >&2
    return 1
  }
  echo "topology OK: ${replicas}x$((1024 / replicas)) partitions, $shards consumer shards"
}

delete_streams() {
  local stream existing
  existing=$(nats --server "$nats_url" stream ls --json)
  for stream in EVENTS-RAW-0 EVENTS-RAW-1 EVENTS-RAW-2 EVENTS-RAW-3 \
    SOLVE-JOBS-sydney SOLVE-RESULTS EVENTS-MATCHED; do
    if jq -e --arg stream "$stream" 'index($stream) != null' <<<"$existing" >/dev/null; then
      nats --server "$nats_url" stream rm "$stream" --force >/dev/null
    fi
  done
  existing=$(nats --server "$nats_url" stream ls --json)
  jq -e 'length == 0' <<<"$existing" >/dev/null || {
    echo "JetStream reset incomplete; streams remain: $existing" >&2
    return 1
  }
}

reset_and_deploy() {
  local replicas=$1
  echo "Resetting local benchmark state for ${replicas} orchestrator replica(s)"
  check_valkey_fleet
  kubectl -n "$app_namespace" scale statefulset/orchestrator deployment/matcher-sydney deployment/materializer --replicas=0
  kubectl -n "$app_namespace" wait --for=delete pod -l app=orchestrator --timeout=180s || true
  kubectl -n "$app_namespace" wait --for=delete pod -l app=matcher --timeout=180s || true
  kubectl -n "$app_namespace" wait --for=delete pod -l app=materializer --timeout=180s || true
  delete_streams
  local pod
  while IFS= read -r pod; do
    clear_valkey_pod "$pod"
  done < <(valkey_primary_pods)
  assert_valkey_empty

  # --reuse-values preserves matcher/materializer resources, replicas, solve
  # slots, tracing, catalog and stream count. Only the orchestrator fleet moves.
  helm upgrade "$release" "${repo_root}/infrastructure/chart" -n "$app_namespace" \
    --reuse-values \
    --set "orchestrator.replicas=${replicas}" \
    --set "orchestrator.shards=${shards}" \
    --set "orchestrator.shardQueueCapacity=${shard_queue_capacity}"
  kubectl -n "$app_namespace" rollout status deployment/matcher-sydney --timeout=600s
  kubectl -n "$app_namespace" rollout status deployment/materializer --timeout=180s
  kubectl -n "$app_namespace" rollout status statefulset/orchestrator --timeout=600s

  # Let OTLP export once and let stale 30-second rates from the old fleet age out.
  sleep 35
  for _ in $(seq 1 60); do
    if check_topology "$replicas"; then
      assert_valkey_empty
      return
    fi
    sleep 5
  done
  echo "sharded topology did not converge within 5 minutes" >&2
  return 1
}

prepare_input() {
  local output=$1
  if [[ "$bench_rows" == 0 ]]; then
    printf '%s\n' "$data_file"
    return
  fi
  if [[ ! -f "$output" ]]; then
    awk -v rows="$bench_rows" 'NR <= rows + 1' "$data_file" >"$output"
  fi
  printf '%s\n' "$output"
}

sample_cpu() {
  local output=$1
  printf 'timestamp,kind,pod,usage_core_nanoseconds\n' >"$output"
  while true; do
    local now summary
    now=$(date +%s)
    summary=$(kubectl get --raw "/api/v1/nodes/${node}/proxy/stats/summary")
    jq -r --argjson now "$now" --arg appns "$app_namespace" --arg infrans "$infra_namespace" '
      .pods[]
      | select((.podRef.namespace == $appns and
            ((.podRef.name | startswith("orchestrator-"))
              or (.podRef.name | startswith("matcher-"))
              or (.podRef.name | startswith("materializer-"))))
          or (.podRef.namespace == $infrans and .podRef.name == "nats-0"))
      | . as $pod
      | .containers[]
      | select((($pod.podRef.namespace == $appns) and
            (.name == "orchestrator" or .name == "matcher" or .name == "materializer"))
          or (($pod.podRef.namespace == $infrans) and .name == "nats"))
      | [$now,
          .name,
          $pod.podRef.name,
          .cpu.usageCoreNanoSeconds]
      | @csv
    ' <<<"$summary" >>"$output"
    sleep "$sample_seconds"
  done
}

summarize_cpu() {
  local input=$1 output=$2 start=${3:-0} end=${4:-9999999999}
  printf 'kind,pod,average_cores\n' >"$output"
  awk -F, -v start="$start" -v end="$end" '
    NR == 1 { next }
    {
      gsub(/"/, "", $0)
      if ($1 < start || $1 > end) { next }
      key=$2 "," $3
      if (!(key in first_t)) { first_t[key]=$1; first_v[key]=$4 }
      last_t[key]=$1; last_v[key]=$4
    }
    END {
      for (key in first_t) {
        seconds=last_t[key]-first_t[key]
        cores=(seconds > 0) ? (last_v[key]-first_v[key])/seconds/1000000000 : 0
        print key "," cores
      }
    }
  ' "$input" | sort >>"$output"
}

capture_streams() {
  local output=$1
  jq -n '{streams: []}' >"$output"
  local stream tmp
  for stream in EVENTS-RAW-0 EVENTS-RAW-1 EVENTS-RAW-2 EVENTS-RAW-3 \
    SOLVE-JOBS-sydney SOLVE-RESULTS EVENTS-MATCHED; do
    tmp=$(mktemp)
    nats --server "$nats_url" stream info "$stream" --json >"$tmp"
    jq --slurpfile item "$tmp" '.streams += $item' "$output" >"${output}.tmp"
    mv "${output}.tmp" "$output"
    rm -f "$tmp"
  done
}

wait_for_drain() {
  local published=$1 deadline=$(( $(date +%s) + drain_timeout )) stable=0
  while (( $(date +%s) < deadline )); do
    local jobs results outputs pending active handlers acked
    jobs=$(stream_messages SOLVE-JOBS-sydney)
    results=$(stream_messages SOLVE-RESULTS)
    outputs=$(stream_messages EVENTS-MATCHED)
    pending=$(prom_scalar 'sum(pending_observations)')
    active=$(prom_scalar 'sum(active_jobs)')
    handlers=$(prom_scalar 'sum(matcher_handlers_in_flight)')
    # exported_instance contains only the stable StatefulSet ordinal, not a pod
    # incarnation. Broker ack floors are therefore the unambiguous absolute
    # count for these freshly-created consumers after a rollout.
    acked=$(raw_consumer_ack_total)
    echo "drain jobs=$jobs retained_results=$results retained_outputs=$outputs pending=$pending active=$active handlers=$handlers acked=$acked/$published"
    # Results and matched-output streams use limits retention, so their message
    # counts are retained history rather than backlog. Raw acknowledgement is
    # the exact end-to-end orchestrator completion signal.
    if [[ "$jobs" == 0 ]] \
      && awk -v p="$pending" -v a="$active" -v h="$handlers" -v done="$acked" -v want="$published" \
        'BEGIN { exit !((p+a+h)==0 && done>=want) }'; then
      stable=$((stable + 1))
      if (( stable >= 3 )); then
        return 0
      fi
    else
      stable=0
    fi
    sleep 10
  done
  echo "drain timed out after ${drain_timeout}s" >&2
  return 1
}

capture_metrics() {
  local dir=$1 start=$2 end=$3 instance_regex=$4
  while IFS='|' read -r name query; do
    capture_range "${dir}/${name}.json" "$query" "$start" "$end"
  done <<EOF
raw_publish_rate|sum(rate(nats_stream_last_seq{stream_name=~"EVENTS-RAW-.*"}[30s]))
raw_claim_rate|sum(rate(offered_observations_total{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"}[30s]))
raw_ack_rate|sum(rate(raw_acks_total{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"}[30s]))
job_publish_rate|sum(rate(jobs_claimed_total{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"}[30s]))
matcher_claim_rate|sum(rate(queue_wait_seconds_count[30s]))
matcher_solve_rate|sum(rate(solve_seconds_count[30s]))
matcher_useful_rate|sum(rate(solve_seconds_count{outcome="solved"}[30s]))
matcher_solve_mean_seconds|sum(rate(solve_seconds_sum[30s])) / sum(rate(solve_seconds_count[30s]))
matcher_solve_p50_seconds|histogram_quantile(0.50, sum by (le) (rate(solve_seconds_bucket[30s])))
matcher_solve_p95_seconds|histogram_quantile(0.95, sum by (le) (rate(solve_seconds_bucket[30s])))
result_receive_rate|sum(rate(results_received_total{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"}[30s]))
completion_rate|sum(rate(completions_total{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"}[30s]))
materialize_rate|sum(rate(materialized_outputs_total[30s]))
raw_queue_p95|histogram_quantile(0.95, sum by (le) (rate(raw_queue_wait_seconds_bucket[30s])))
raw_queue_p99|histogram_quantile(0.99, sum by (le) (rate(raw_queue_wait_seconds_bucket[30s])))
pending_observations|sum(pending_observations{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"})
oldest_pending_age|max(oldest_pending_age_seconds{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"})
active_jobs|sum(active_jobs{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"})
jobs_outstanding|sum(jobs_outstanding{exported_job="routers-orchestrator",exported_instance=~"${instance_regex}"})
matcher_handlers|sum(matcher_handlers_in_flight)
matcher_solves|sum(matcher_solves_in_flight)
nats_cpu_percent|nats_varz_cpu
orchestrator_cpu_cores_prom|sum(rate(container_cpu_usage_seconds_total{namespace="${app_namespace}",container="orchestrator"}[30s]))
matcher_cpu_cores_prom|sum(rate(container_cpu_usage_seconds_total{namespace="${app_namespace}",container="matcher"}[30s]))
materializer_cpu_cores_prom|sum(rate(container_cpu_usage_seconds_total{namespace="${app_namespace}",container="materializer"}[30s]))
nats_cpu_cores_prom|sum(rate(container_cpu_usage_seconds_total{namespace="${infra_namespace}",container="nats",pod="nats-0"}[30s]))
freshness_missed_rate|sum(rate(freshness_target_missed_total[30s]))
freshness_lateness_p50|histogram_quantile(0.50, sum by (le) (rate(freshness_target_lateness_seconds_bucket[30s])))
freshness_lateness_p95|histogram_quantile(0.95, sum by (le) (rate(freshness_target_lateness_seconds_bucket[30s])))
freshness_lateness_p99|histogram_quantile(0.99, sum by (le) (rate(freshness_target_lateness_seconds_bucket[30s])))
solve_jobs_messages|sum(nats_stream_total_messages{stream_name=~"SOLVE-JOBS-.*"})
solve_results_messages|sum(nats_stream_total_messages{stream_name="SOLVE-RESULTS"})
EOF
}

summarize_metrics() {
  local dir=$1 output=$2
  printf 'metric,mean,peak,last,samples\n' >"$output"
  local file name row
  for file in "$dir"/*.json; do
    name=$(basename "$file" .json)
    row=$(jq -r '
      [.data.result[].values[][1] | tonumber] as $values
      | if ($values | length) == 0 then "nan,nan,nan,0"
        else [($values | add / length), ($values | max), $values[-1], ($values | length)] | @csv
        end
    ' "$file")
    printf '%s,%s\n' "$name" "$row" >>"$output"
  done
}

run_one() {
  local replicas=$1
  local width=$((1024 / replicas)) stamp dir input publish_start publish_end drain_end published accepted broker_acks raw_ack_total instance_regex drain_status
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  dir="${results_root}/${stamp}-${replicas}x${width}"
  mkdir -p "$dir/metrics-publish" "$dir/metrics-run"

  reset_and_deploy "$replicas"
  capture_streams "$dir/streams-before.json"
  helm -n "$app_namespace" get values "$release" -a >"$dir/helm-values.yaml"
  kubectl -n "$app_namespace" get pods -o json >"$dir/pods.json"
  input=$(prepare_input "${results_root}/input-${bench_rows}.csv")
  instance_regex=$(orchestrator_instance_regex)
  broker_acks=$(raw_consumer_ack_total)
  [[ "$broker_acks" == 0 ]] || {
    echo "fresh raw consumers have a non-zero ack floor: $broker_acks" >&2
    return 1
  }

  sample_cpu "$dir/cpu.csv" &
  sampler_pid=$!
  publish_start=$(date +%s)
  replay_args=(
    --file "$input"
    --nats "$nats_url"
    --lanes "$replay_lanes"
    --streams "$streams"
    --summary-json "$dir/replay-summary.json"
  )
  if [[ "$replay_rate" == 0 ]]; then
    replay_args+=(--speed 0)
  else
    replay_args+=(--rate "$replay_rate")
  fi
  set +e
  RUST_LOG=info "$replay_bin" "${replay_args[@]}" 2>&1 | tee "$dir/replay.log"
  local replay_status=${PIPESTATUS[0]}
  set -e
  publish_end=$(date +%s)
  if (( replay_status != 0 )); then
    echo "replay failed with status $replay_status" >&2
    return "$replay_status"
  fi
  published=$(jq -er '.raw_published' "$dir/replay-summary.json")
  accepted=$(jq -er '.raw_published - .raw_duplicates' "$dir/replay-summary.json")

  drain_status="drained"
  if ! wait_for_drain "$accepted" | tee "$dir/drain.log"; then
    drain_status="timeout"
  fi
  drain_end=$(date +%s)
  broker_acks=$(raw_consumer_ack_total)
  raw_ack_total=$(prom_scalar "sum(raw_acks_total{exported_job=\"routers-orchestrator\",exported_instance=~\"${instance_regex}\"})")
  kill "$sampler_pid" 2>/dev/null || true
  wait "$sampler_pid" 2>/dev/null || true
  sampler_pid=""

  capture_streams "$dir/streams-after.json"
  capture_metrics "$dir/metrics-publish" "$publish_start" "$publish_end" "$instance_regex"
  capture_metrics "$dir/metrics-run" "$publish_start" "$drain_end" "$instance_regex"
  summarize_metrics "$dir/metrics-publish" "$dir/metrics-publish-summary.csv"
  summarize_metrics "$dir/metrics-run" "$dir/metrics-run-summary.csv"
  summarize_cpu "$dir/cpu.csv" "$dir/cpu-publish-summary.csv" "$publish_start" "$publish_end"
  summarize_cpu "$dir/cpu.csv" "$dir/cpu-run-summary.csv" "$publish_start" "$drain_end"
  jq -n \
    --argjson replicas "$replicas" --argjson partitions_per_replica "$width" \
    --argjson shards "$shards" --argjson published "$published" --argjson accepted "$accepted" \
    --arg orchestrator_instance_regex "$instance_regex" \
    --argjson broker_raw_acks "$broker_acks" --argjson raw_acks_total "$raw_ack_total" \
    --argjson publish_start "$publish_start" --argjson publish_end "$publish_end" \
    --argjson drain_end "$drain_end" --arg drain_status "$drain_status" \
    '{replicas:$replicas, partitions_per_replica:$partitions_per_replica,
      total_partitions:1024, consumer_shards:$shards, published:$published, accepted:$accepted,
      orchestrator_instance_regex:$orchestrator_instance_regex,
      broker_raw_acks:$broker_raw_acks, raw_acks_total:$raw_acks_total,
      publish_start:$publish_start, publish_end:$publish_end, drain_end:$drain_end,
      publish_seconds:($publish_end-$publish_start), drain_seconds:($drain_end-$publish_end),
      status:$drain_status}' >"$dir/run.json"
  echo "completed ${replicas}x${width}: $dir"
}

command=${1:-}
case "$command" in
  configure-exporter)
    configure_exporter
    ;;
  check)
    require_tools
    start_nats_forward
    check_exporter
    check_topology "${2:-$(kubectl -n "$app_namespace" get statefulset orchestrator -o jsonpath='{.spec.replicas}')}"
    ;;
  run)
    shift
    [[ "${CONFIRM_LOCAL_RESET:-0}" == 1 ]] || {
      echo "run deletes local benchmark streams and flushes local Valkey; set CONFIRM_LOCAL_RESET=1" >&2
      exit 2
    }
    require_tools
    check_replay_features
    start_nats_forward
    check_exporter
    if (( $# == 0 )); then
      replicas=(1 2 4 8)
    else
      replicas=("$@")
    fi
    for replica_count in "${replicas[@]}"; do
      case "$replica_count" in 1|2|4|8) ;; *) echo "unsupported sweep replica count: $replica_count" >&2; exit 2 ;; esac
      run_one "$replica_count"
    done
    ;;
  *)
    usage
    exit 2
    ;;
esac
