{{/*
Expand the name of the chart.
*/}}
{{- define "nook-control.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name — <release>-<chart>, or fullnameOverride verbatim.
*/}}
{{- define "nook-control.fullname" -}}
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

{{- define "nook-control.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels applied to every object.
*/}}
{{- define "nook-control.labels" -}}
helm.sh/chart: {{ include "nook-control.chart" . }}
{{ include "nook-control.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: nookos
{{- end -}}

{{/*
Selector labels shared across the release (no per-component key here).
*/}}
{{- define "nook-control.selectorLabels" -}}
app.kubernetes.io/name: {{ include "nook-control.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Per-component names and selector labels. Call with a dict:
  (dict "root" . "component" "control")
*/}}
{{- define "nook-control.componentName" -}}
{{- printf "%s-%s" (include "nook-control.fullname" .root) .component | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "nook-control.componentSelectorLabels" -}}
{{ include "nook-control.selectorLabels" .root }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/*
The ServiceAccount name to use.
*/}}
{{- define "nook-control.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "nook-control.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
The control-plane image reference. Tag defaults to the chart's appVersion.
*/}}
{{- define "nook-control.controlImage" -}}
{{- $img := .Values.controlPlane.image -}}
{{- printf "%s/%s:%s" $img.registry $img.repository (default .Chart.AppVersion $img.tag) -}}
{{- end -}}

{{/*
The web image reference. Tag defaults to the chart's appVersion.
*/}}
{{- define "nook-control.webImage" -}}
{{- $img := .Values.web.image -}}
{{- printf "%s/%s:%s" $img.registry $img.repository (default .Chart.AppVersion $img.tag) -}}
{{- end -}}

{{/*
Guardrail: existingSecret is required — the chart references secrets, never
creates or embeds them.
*/}}
{{- define "nook-control.requireSecret" -}}
{{- if not .Values.existingSecret -}}
{{- fail "\n\nvalues.existingSecret is required: create a Kubernetes Secret holding DATABASE_URL and SESSION_SECRET (and any optional OIDC/S3 secrets), then set --set existingSecret=<name>. The chart never stores secret material itself." -}}
{{- end -}}
{{- end -}}

{{/*
Guardrail: an enabled agent listener needs BOTH its TLS Secret and its public
URL, or it is half-configured — a listener with no cert cannot start and one
with no advertised address cannot be dialled. mTLS is opt-in; refuse rather
than render half of it.
*/}}
{{- define "nook-control.requireAgent" -}}
{{- if .Values.agent.enabled -}}
{{- if not .Values.agent.tlsSecret -}}
{{- fail "\n\nagent.enabled=true needs agent.tlsSecret: create a Kubernetes TLS Secret holding the agent listener's certificate and key (see the chart README, \"Agent mTLS listener\"), then set --set agent.tlsSecret=<name>. The listener terminates TLS in-process, so the chart will not render it without a cert." -}}
{{- end -}}
{{- if not .Values.agent.publicUrl -}}
{{- fail "\n\nagent.enabled=true needs agent.publicUrl: the externally reachable address of the agent LoadBalancer (e.g. agent.nook.example.com:8081). The control plane bakes it into join tokens so a node dials the right place." -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
The agent-cert mount directory and the cert/key file paths inside it. The whole
tlsSecret mounts here; the process reads the cert and key from these paths.
*/}}
{{- define "nook-control.agentCertDir" -}}/etc/nook/agent{{- end -}}
{{- define "nook-control.agentCertPath" -}}{{ include "nook-control.agentCertDir" . }}/{{ .Values.agent.tlsCertKey }}{{- end -}}
{{- define "nook-control.agentKeyPath" -}}{{ include "nook-control.agentCertDir" . }}/{{ .Values.agent.tlsKeyKey }}{{- end -}}
