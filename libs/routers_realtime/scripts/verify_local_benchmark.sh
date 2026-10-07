#!/usr/bin/env bash
set -euo pipefail

nats_url=${1:-nats://127.0.0.1:14222}

[[ $(kubectl config current-context) == orbstack ]] || {
  echo 'Refusing to verify a context other than orbstack' >&2
  exit 1
}

deadline=$((SECONDS + 240))
while ((SECONDS < deadline)); do
  ready=true
  for stream in EVENTS-RAW-0 EVENTS-RAW-1 EVENTS-RAW-2 EVENTS-RAW-3; do
    state=$(nats --server "$nats_url" stream info "$stream" --json | jq -r '[.state.messages, .state.consumer_count] | @tsv')
    if [[ $state != $'0\t16' ]]; then
      ready=false
      break
    fi
  done
  if [[ $ready == true ]]; then
    for stream in EVENTS-MATCHED; do
      state=$(nats --server "$nats_url" stream info "$stream" --json | jq -r '.state.messages')
      if [[ $state != 0 ]]; then
        ready=false
        break
      fi
    done
  fi
  if [[ $ready == true ]]; then
    echo 'Benchmark topology ready: 64 raw consumers; all five streams empty.'
    exit 0
  fi
  sleep 1
done

echo 'Benchmark topology did not become ready within 240 seconds' >&2
exit 1
