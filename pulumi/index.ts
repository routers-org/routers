import * as pulumi from "@pulumi/pulumi";
import * as gcp from "@pulumi/gcp";
import * as k8s from "@pulumi/kubernetes";

const cfg = new pulumi.Config();
const project = cfg.require("gcpProject");
const region = cfg.get("region") ?? "us-central1";
const garRepo = cfg.get("garRepository") ?? "routers";
const imageTag = cfg.get("imageTag") ?? "latest";
const shardPrecision = cfg.getNumber("shardPrecision") ?? 5;
const matcherReplicas = cfg.getNumber("matcherReplicas") ?? 2;
const orchestratorReplicas = cfg.getNumber("orchestratorReplicas") ?? 2;

// ── Artifact Registry ───────────────────────────────────────────────────────

const registry = new gcp.artifactregistry.Repository("routers-registry", {
  location: region,
  repositoryId: garRepo,
  format: "DOCKER",
  project,
});

// ── GCS bucket for shard files ──────────────────────────────────────────────

const shardBucket = new gcp.storage.Bucket("routers-shards", {
  location: region,
  project,
  uniformBucketLevelAccess: true,
  versioning: { enabled: false },
});

// ── GKE Autopilot cluster ───────────────────────────────────────────────────

const cluster = new gcp.container.Cluster("routers-cluster", {
  location: region,
  project,
  enableAutopilot: true,
  releaseChannel: { channel: "REGULAR" },
  workloadIdentityConfig: { workloadPool: `${project}.svc.id.goog` },
  deletionProtection: false,
});

const kubeconfig = pulumi.all([cluster.name, cluster.endpoint, cluster.masterAuth]).apply(
  ([name, endpoint, auth]) => {
    const context = `${project}_${region}_${name}`;
    return `apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: ${auth.clusterCaCertificate}
    server: https://${endpoint}
  name: ${context}
contexts:
- context:
    cluster: ${context}
    user: ${context}
  name: ${context}
current-context: ${context}
kind: Config
users:
- name: ${context}
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: gke-gcloud-auth-plugin
      installHint: Install gke-gcloud-auth-plugin
      provideClusterInfo: true
`;
  }
);

const provider = new k8s.Provider("gke", { kubeconfig });

// ── Workload Identity for GCS access ────────────────────────────────────────

const matcherSA = new gcp.serviceaccount.Account("routers-matcher-sa", {
  accountId: "routers-matcher",
  displayName: "Routers Matcher",
  project,
});

new gcp.storage.BucketIAMMember("matcher-gcs-reader", {
  bucket: shardBucket.name,
  role: "roles/storage.objectViewer",
  member: pulumi.interpolate`serviceAccount:${matcherSA.email}`,
});

new gcp.serviceaccount.IAMMember("matcher-workload-identity", {
  serviceAccountId: matcherSA.name,
  role: "roles/iam.workloadIdentityUser",
  member: pulumi.interpolate`serviceAccount:${project}.svc.id.goog[default/routers-matcher]`,
});

// ── Namespace ───────────────────────────────────────────────────────────────

const ns = new k8s.core.v1.Namespace("routers-ns", {
  metadata: { name: "routers" },
}, { provider });

// ── NATS JetStream ──────────────────────────────────────────────────────────

const natsChart = new k8s.helm.v4.Chart("nats", {
  chart: "nats",
  repositoryOpts: { repo: "https://nats-io.github.io/k8s/helm/charts/" },
  namespace: ns.metadata.name,
  values: {
    config: {
      jetstream: { enabled: true, memStorage: { enabled: true, size: "1Gi" } },
      cluster: { enabled: true, replicas: 3 },
    },
  },
}, { provider });

const natsUrl = pulumi.interpolate`nats://nats.${ns.metadata.name}.svc.cluster.local:4222`;

// ── Valkey (Redis-compatible) ────────────────────────────────────────────────

const valkeyPvc = new k8s.core.v1.PersistentVolumeClaim("valkey-pvc", {
  metadata: { name: "valkey-data", namespace: ns.metadata.name },
  spec: {
    accessModes: ["ReadWriteOnce"],
    resources: { requests: { storage: "10Gi" } },
  },
}, { provider });

const valkeyStatefulSet = new k8s.apps.v1.StatefulSet("valkey", {
  metadata: { name: "valkey", namespace: ns.metadata.name },
  spec: {
    selector: { matchLabels: { app: "valkey" } },
    serviceName: "valkey",
    replicas: 1,
    template: {
      metadata: { labels: { app: "valkey" } },
      spec: {
        containers: [{
          name: "valkey",
          image: "valkey/valkey:8-alpine",
          ports: [{ containerPort: 6379 }],
          volumeMounts: [{ name: "data", mountPath: "/data" }],
          resources: {
            requests: { cpu: "250m", memory: "512Mi" },
            limits: { memory: "1Gi" },
          },
        }],
        volumes: [{ name: "data", persistentVolumeClaim: { claimName: valkeyPvc.metadata.name } }],
      },
    },
  },
}, { provider });

new k8s.core.v1.Service("valkey-svc", {
  metadata: { name: "valkey", namespace: ns.metadata.name },
  spec: {
    selector: { app: "valkey" },
    ports: [{ port: 6379, targetPort: 6379 }],
    clusterIP: "None",
  },
}, { provider });

const valkeyUrl = pulumi.interpolate`redis://valkey.${ns.metadata.name}.svc.cluster.local:6379`;

// ── KEDA ────────────────────────────────────────────────────────────────────

new k8s.helm.v4.Chart("keda", {
  chart: "keda",
  repositoryOpts: { repo: "https://kedacore.github.io/charts" },
  namespace: "keda",
  createNamespace: true,
}, { provider });

// ── Helper: image URL ────────────────────────────────────────────────────────

const imageBase = `${region}-docker.pkg.dev/${project}/${garRepo}`;

// ── Orchestrator Deployment ──────────────────────────────────────────────────

const rabbitmqUrl = cfg.requireSecret("rabbitmqUrl");

new k8s.apps.v1.Deployment("orchestrator", {
  metadata: { name: "routers-orchestrator", namespace: ns.metadata.name },
  spec: {
    replicas: orchestratorReplicas,
    selector: { matchLabels: { app: "routers-orchestrator" } },
    template: {
      metadata: { labels: { app: "routers-orchestrator" } },
      spec: {
        containers: [{
          name: "orchestrator",
          image: `${imageBase}/routers-orchestrator:${imageTag}`,
          env: [
            { name: "RABBITMQ_URL", value: rabbitmqUrl },
            { name: "NATS_URL", value: natsUrl },
            { name: "VALKEY_URL", value: valkeyUrl },
            { name: "SHARD_PRECISION", value: String(shardPrecision) },
          ],
          resources: {
            requests: { cpu: "250m", memory: "256Mi" },
            limits: { memory: "512Mi" },
          },
        }],
      },
    },
  },
}, { provider, dependsOn: [natsChart, valkeyStatefulSet] });

// ── Matcher StatefulSet ──────────────────────────────────────────────────────

const matcherK8sSA = new k8s.core.v1.ServiceAccount("matcher-k8s-sa", {
  metadata: {
    name: "routers-matcher",
    namespace: ns.metadata.name,
    annotations: {
      "iam.gke.io/gcp-service-account": matcherSA.email,
    },
  },
}, { provider });

new k8s.apps.v1.StatefulSet("matcher", {
  metadata: { name: "routers-matcher", namespace: ns.metadata.name },
  spec: {
    selector: { matchLabels: { app: "routers-matcher" } },
    serviceName: "routers-matcher",
    replicas: matcherReplicas,
    template: {
      metadata: { labels: { app: "routers-matcher" } },
      spec: {
        serviceAccountName: matcherK8sSA.metadata.name,
        initContainers: [{
          name: "fetch-shards",
          image: "google/cloud-sdk:alpine",
          command: [
            "sh", "-c",
            `gsutil -m cp "gs://${shardBucket.name}/*.shard.rt" /shards/`,
          ],
          volumeMounts: [{ name: "shards", mountPath: "/shards" }],
          resources: { requests: { cpu: "100m", memory: "128Mi" } },
        }],
        containers: [{
          name: "matcher",
          image: `${imageBase}/routers-matcher:${imageTag}`,
          env: [
            { name: "NATS_URL", value: natsUrl },
            { name: "SHARD_DIR", value: "/shards" },
            { name: "SHARD_PRECISION", value: String(shardPrecision) },
          ],
          volumeMounts: [{ name: "shards", mountPath: "/shards" }],
          resources: {
            requests: { cpu: "500m", memory: "1Gi" },
            limits: { memory: "2Gi" },
          },
        }],
        volumes: [{ name: "shards", emptyDir: {} }],
      },
    },
  },
}, { provider, dependsOn: [natsChart, matcherK8sSA] });

// ── KEDA ScaledObject for matcher ─────────────────────────────────────────────

new k8s.apiextensions.CustomResource("matcher-scaledobject", {
  apiVersion: "keda.sh/v1alpha1",
  kind: "ScaledObject",
  metadata: { name: "matcher-scaledobject", namespace: ns.metadata.name },
  spec: {
    scaleTargetRef: { name: "routers-matcher" },
    minReplicaCount: 1,
    maxReplicaCount: 20,
    triggers: [{
      type: "nats-jetstream",
      metadata: {
        natsServerMonitoringEndpoint: pulumi.interpolate`nats.${ns.metadata.name}.svc.cluster.local:8222`,
        account: "$G",
        stream: "MATCH",
        consumer: "matchers",
        lagThreshold: "10",
      },
    }],
  },
}, { provider, dependsOn: [natsChart] });

// ── Outputs ──────────────────────────────────────────────────────────────────

export const clusterName = cluster.name;
export const shardBucketName = shardBucket.name;
export const registryUrl = pulumi.interpolate`${region}-docker.pkg.dev/${project}/${garRepo}`;
