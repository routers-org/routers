# routers-realtime helm chart

Helm chart for the routers realtime pipeline: per-shard matcher + orchestrator
Deployments plus the singleton historian, prometheus scrape secret, and
grafana dashboard ConfigMap.

Replaces the hand-maintained manifests under `deploy/local/` and the
`deploy-matchers.sh` shell loop.

## Install

The shard cache directory must be supplied — its default is a generic
node path (`/var/lib/routers-shards`) that won't exist on a dev box.

For a local dev cluster (OrbStack / Docker Desktop) where shards are
generated into `target/shard_cache`:

```sh
helm upgrade --install routers-realtime ./deploy/chart \
  -n routers-dev --create-namespace \
  -f ./deploy/chart/values-local-dev.yaml \
  --set shardCache.hostPath=$PWD/target/shard_cache
```

For a multi-node cluster, switch to a PVC:

```sh
helm upgrade --install routers-realtime ./deploy/chart \
  -n routers-dev --create-namespace \
  --set shardCache.mode=pvc \
  --set shardCache.pvc.claimName=routers-shards
```

(The chart does not create the PVC — bring your own.)

## Change the shard set

`values.yaml` carries the list under `shards:`. Override at install time:

```sh
helm upgrade routers-realtime ./deploy/chart \
  --set 'shards={r3gq,r3gr,r3gw}'
```

Or render-only to inspect:

```sh
helm template routers-realtime ./deploy/chart > /tmp/rendered.yaml
```

## What this chart does NOT install

NATS, Valkey, KEDA, kube-prometheus-stack. Those are managed separately
(see `pulumi/` for production, or installed via their upstream charts in
dev).

## Migration from `deploy/local`

The following files are now templated and can be deleted once the chart
is in use:

- `matcher.yaml`
- `orchestrator.yaml`
- `historian.yaml`
- `prometheus-scrape.yaml`
- `grafana-dashboard.yaml`
- `deploy-matchers.sh`
- `teardown-matchers.sh`

Keep `port-forward.sh` — it's an operational tool, not a manifest.
