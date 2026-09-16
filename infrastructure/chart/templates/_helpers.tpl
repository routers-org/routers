{{/* Common labels applied to every rendered resource. */}}
{{- define "routers.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end }}

{{/* Fail with a clear message if required infra endpoints are missing. */}}
{{- define "routers.requireInfra" -}}
{{- if not .Values.infra.nats.url -}}
{{- fail "infra.nats.url is required; supply a values overlay (e.g. -f values-local-dev.yaml)" -}}
{{- end -}}
{{- if not .Values.infra.valkey.url -}}
{{- fail "infra.valkey.url is required; supply a values overlay (e.g. -f values-local-dev.yaml)" -}}
{{- end -}}
{{- end }}

{{/*
Chart-level guard, included by every workload template.

The catalog is the contract between the orchestrator (which routes a vehicle's
jobs to a region's subject) and the matcher (which loads that region's shards
and answers on it). A catalog the binaries would reject, or one that would
silently misroute, must fail the render rather than the running fleet, so each
rule below names what breaks.
*/}}
{{- define "routers.validate" -}}
{{- include "routers.requireInfra" . -}}
{{- $precision := int .Values.shardPrecision -}}
{{- if ne $precision 4 -}}
{{- fail (printf "shardPrecision is %d but the binaries compile event::SHARD_PRECISION = 4; a mismatch would route every job to a cell no matcher serves." $precision) -}}
{{- end -}}
{{- $regions := .Values.catalog.regions -}}
{{- if not $regions -}}
{{- fail "catalog.regions is empty; a fleet with no regions serves nothing. Define at least one region." -}}
{{- end -}}
{{- $fleet := int .Values.orchestrator.replicas -}}
{{- if or (lt $fleet 1) (ne (mod 1024 $fleet) 0) -}}
{{- fail (printf "orchestrator.replicas is %d, which must divide 1024; any other fleet size slices the 1024 partitions unevenly, leaving gaps or overlapping ownership." $fleet) -}}
{{- end -}}
{{- $owned := dict -}}
{{- range $region := $regions -}}
{{- $id := toString $region.id -}}
{{- if not (regexMatch "^[A-Za-z0-9_-]+$" $id) -}}
{{- fail (printf "region id %q is not NATS-token-safe (needs ^[A-Za-z0-9_-]+$); the id becomes a subject and stream token, so an unsafe one makes the region's job plane unaddressable." $id) -}}
{{- end -}}
{{- if not (regexMatch "^[A-Za-z0-9_-]+$" (toString $region.graph)) -}}
{{- fail (printf "region %q graph %q is not token-safe (needs ^[A-Za-z0-9_-]+$); the graph version is a token on the solve subject." $id (toString $region.graph)) -}}
{{- end -}}
{{- if lt (int $region.lanes) 1 -}}
{{- fail (printf "region %q sets lanes = %d; a region needs at least one lane or no job can be addressed to it." $id (int $region.lanes)) -}}
{{- end -}}
{{- range $cell := $region.coverage -}}
{{- $cell = toString $cell -}}
{{- if ne (len $cell) $precision -}}
{{- fail (printf "region %q coverage cell %q is %d characters but shardPrecision is %d; the matcher would load a different extent from the one its job subject covers." $id $cell (len $cell) $precision) -}}
{{- end -}}
{{- if hasKey $owned $cell -}}
{{- fail (printf "coverage cell %q is owned by both region %q and region %q; ownership must be disjoint or a vehicle's jobs would split across two regions." $cell (index $owned $cell) $id) -}}
{{- end -}}
{{- $owned = set $owned $cell $id -}}
{{- end -}}
{{- range $cell := $region.overlap -}}
{{- $cell = toString $cell -}}
{{- if ne (len $cell) $precision -}}
{{- fail (printf "region %q overlap cell %q is %d characters but shardPrecision is %d." $id $cell (len $cell) $precision) -}}
{{- end -}}
{{- if has $cell $region.coverage -}}
{{- fail (printf "region %q lists cell %q as both coverage and overlap; an owned cell cannot also be its own certified fallback." $id $cell) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end }}

{{/*
Render one TOML value (scalar, string, array of values, or inline table) from a
decoded YAML value. Recurses through `include` for arrays and tables, so the
catalog renderer never has to know a field's shape.
  - strings   -> a quoted basic string (%q escapes for us)
  - integers  -> bare, never in exponent form (a whole float prints as an int)
  - bools     -> bare true/false
  - slices    -> [a, b, c]
  - maps      -> { k = v, ... } with keys in a stable order
*/}}
{{- define "routers.tomlValue" -}}
{{- $v := . -}}
{{- if kindIs "map" $v -}}
{{- $parts := list -}}
{{- range $k := (keys $v | sortAlpha) -}}
{{- $parts = append $parts (printf "%s = %s" $k (include "routers.tomlValue" (index $v $k))) -}}
{{- end -}}
{{- printf "{ %s }" (join ", " $parts) -}}
{{- else if kindIs "slice" $v -}}
{{- $parts := list -}}
{{- range $e := $v -}}
{{- $parts = append $parts (include "routers.tomlValue" $e) -}}
{{- end -}}
{{- printf "[%s]" (join ", " $parts) -}}
{{- else if kindIs "string" $v -}}
{{- printf "%q" $v -}}
{{- else if kindIs "float64" $v -}}
{{- if eq $v (floor $v) -}}
{{- printf "%d" (int64 $v) -}}
{{- else -}}
{{- printf "%v" $v -}}
{{- end -}}
{{- else -}}
{{- printf "%v" $v -}}
{{- end -}}
{{- end }}

{{/*
Render `.Values.catalog` as a whole catalog TOML document: the top-level scalars
(everything but `regions`) first, then one `[[regions]]` table per region. The
result is what the binaries' `Catalog::load` parses, so it is mounted verbatim
and also drives the pods' catalog checksum.
*/}}
{{- define "routers.catalog.toml" -}}
{{- $catalog := .Values.catalog -}}
{{- range $key := (keys $catalog | sortAlpha) -}}
{{- if ne $key "regions" }}
{{ $key }} = {{ include "routers.tomlValue" (index $catalog $key) }}
{{- end -}}
{{- end }}
{{- range $region := $catalog.regions }}

[[regions]]
{{- range $key := (keys $region | sortAlpha) }}
{{ $key }} = {{ include "routers.tomlValue" (index $region $key) }}
{{- end -}}
{{- end -}}
{{- end }}

{{/*
Fully qualified image reference. `image.registry` is prepended when set, so
the same service-level repository/tag pair resolves against Artifact Registry
in GKE and against the local daemon in dev with no per-service duplication.
Call with (dict "registry" $.Values.image.registry "image" $.Values.<svc>.image).
*/}}
{{- define "routers.image" -}}
{{- $registry := .registry | default "" -}}
{{- if $registry -}}
{{- printf "%s/%s:%s" (trimSuffix "/" $registry) .image.repository (.image.tag | toString) -}}
{{- else -}}
{{- printf "%s:%s" .image.repository (.image.tag | toString) -}}
{{- end -}}
{{- end }}

{{/*
Name of the ServiceAccount the pods run as. Empty means "do not set
serviceAccountName", which leaves the namespace default in place and keeps
local dev identical to before Workload Identity existed in this chart.
*/}}
{{- define "routers.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- .Values.serviceAccount.name | default .Chart.Name -}}
{{- else -}}
{{- .Values.serviceAccount.name | default "" -}}
{{- end -}}
{{- end }}

{{/*
Pod-spec fields identical in shape for the matcher and the orchestrator:
identity, pull secrets and placement. Held in one place so a new scheduling
knob is added once rather than twice.
Call with (dict "root" $ "service" $.Values.<svc>).
*/}}
{{- define "routers.podSpecCommon" -}}
{{- $sa := include "routers.serviceAccountName" .root }}
{{- if $sa }}
serviceAccountName: {{ $sa }}
{{- end }}
{{- with .root.Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .service.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .service.tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}
