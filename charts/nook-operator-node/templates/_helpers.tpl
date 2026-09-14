{{/*
Expand the name of the chart.
*/}}
{{- define "nook-operator-node.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name — <release>-<chart>, or fullnameOverride verbatim.
*/}}
{{- define "nook-operator-node.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "nook-operator-node.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Selector labels shared across the release.
*/}}
{{- define "nook-operator-node.selectorLabels" -}}
app.kubernetes.io/name: {{ include "nook-operator-node.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Common labels applied to every object.
*/}}
{{- define "nook-operator-node.labels" -}}
helm.sh/chart: {{ include "nook-operator-node.chart" . }}
{{ include "nook-operator-node.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: nookos
app.kubernetes.io/component: operator-node
{{- end -}}

{{/*
The operator-node image reference. Tag defaults to the chart's appVersion.
*/}}
{{- define "nook-operator-node.image" -}}
{{- $img := .Values.image -}}
{{- printf "%s/%s:%s" $img.registry $img.repository (default .Chart.AppVersion $img.tag) -}}
{{- end -}}

{{/*
The image a job Pod runs (NOOK_JOB_IMAGE).

Derived the same way the node's own image is — same registry, same version —
because the job sandbox is published in lockstep with it by the release
workflow. `executor.image` overrides it for a private registry or a sandbox
built from `scripts/build-job-sandbox.sh`.
*/}}
{{- define "nook-operator-node.jobImage" -}}
{{- if .Values.executor.image -}}
{{- .Values.executor.image -}}
{{- else -}}
{{- $img := .Values.image -}}
{{- printf "%s/%s:%s" $img.registry .Values.executor.imageRepository (default .Chart.AppVersion $img.tag) -}}
{{- end -}}
{{- end -}}

{{/*
Guardrails: the node cannot join without a server address and a join token.
Refuse to render half a configuration.
*/}}
{{- define "nook-operator-node.requireJoin" -}}
{{- if not .Values.server -}}
{{- fail "\n\nvalues.server is required: the externally reachable address of the control plane's agent listener the node joins (e.g. agent.nook.example.com:8081). Set --set server=<addr>." -}}
{{- end -}}
{{- if not .Values.existingSecret -}}
{{- fail "\n\nvalues.existingSecret is required: create a Kubernetes Secret holding the join token, then set --set existingSecret=<name>. Join stays operator-driven; the chart never stores the token itself." -}}
{{- end -}}
{{- end -}}

{{/*
The ServiceAccount the agent runs as. Named explicitly or derived; only ever
used when the executor is in `kubernetes` mode, since `local` needs no identity
at the apiserver at all.
*/}}
{{- define "nook-operator-node.serviceAccountName" -}}
{{- default (printf "%s-executor" (include "nook-operator-node.fullname" .)) .Values.executor.serviceAccount.name -}}
{{- end -}}
