#!/usr/bin/env bash
# Forward all routers-dev services to localhost for local binary development.
# Each forwarder restarts automatically if kubectl port-forward drops the connection.
# Usage: ./deploy/local/port-forward.sh
# Stop with Ctrl-C.

set -uo pipefail

NS=routers-dev

cleanup() {
    echo ""
    echo "Stopping port-forwards..."
    kill -- -$$ 2>/dev/null || kill $(jobs -p) 2>/dev/null || true
}
trap cleanup EXIT INT TERM

restart_forever() {
    local name=$1; shift
    while true; do
        kubectl "$@" 2>&1 | grep -v "^Forwarding" || true
        echo "[port-forward] ${name} exited — restarting in 1s" >&2
        sleep 1
    done &
}

echo "Port-forwarding namespace ${NS} (self-healing)..."
echo ""

restart_forever rabbitmq-amqp  port-forward -n "${NS}" svc/rabbitmq       5672:5672
restart_forever rabbitmq-ui    port-forward -n "${NS}" svc/rabbitmq       15672:15672
restart_forever nats-client    port-forward -n "${NS}" pod/nats-0         4222:4222
restart_forever nats-monitor   port-forward -n "${NS}" pod/nats-0         8222:8222
restart_forever valkey         port-forward -n "${NS}" svc/valkey-primary  6379:6379
restart_forever grafana        port-forward -n "${NS}" svc/kube-prometheus-stack-grafana 3000:80
restart_forever prometheus     port-forward -n "${NS}" svc/prometheus-operated 9090:9090

echo "  RabbitMQ AMQP   amqp://routers:routers@127.0.0.1:5672/"
echo "  RabbitMQ UI     http://127.0.0.1:15672     (guest/guest)"
echo "  NATS            nats://127.0.0.1:4222"
echo "  NATS monitor    http://127.0.0.1:8222/jsz"
echo "  Valkey          redis://127.0.0.1:6379"
echo "  Grafana         http://127.0.0.1:3000      (admin/admin)"
echo "  Prometheus      http://127.0.0.1:9090"
echo "  Orchestrator    http://127.0.0.1:9091/metrics"
echo ""
echo "Press Ctrl-C to stop."

wait
