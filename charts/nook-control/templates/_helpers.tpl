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
The worker image reference (MAIN-153). Tag defaults to the chart's appVersion,
so the worker rolls forward with the control plane in lockstep.
*/}}
{{- define "nook-control.workerImage" -}}
{{- $img := .Values.worker.image -}}
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

{{/*
Non-empty when the control-plane pod mounts a volume a second copy of itself
could not mount at the same time (MAIN-653).

A ReadWriteOnce claim attaches to ONE node at a time, and RollingUpdate needs
both pods alive at once: the new pod sits in ContainerCreating with
`Multi-Attach error for volume`, and the old pod is not removed until the new
one is Ready. `helm upgrade --wait` then times out and marks the release
failed, `helm rollback` deadlocks the same way, and deleting the old pod does
not help because its ReplicaSet immediately makes a replacement that takes the
volume again. Empty when there is nothing to contend for — an emptyDir, or a
class granting ReadWriteMany.
*/}}
{{- define "nook-control.userContentIsExclusive" -}}
{{- if .Values.userContent.persistence.enabled -}}
{{- if not (has "ReadWriteMany" (.Values.userContent.persistence.accessModes | default list)) -}}
exclusive
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
The tunnel zone (MAIN-512), validated once for every consumer. The ConfigMap
renders whatever ingress.enabled says, so the check lives here rather than in
the Ingress — a deployment fronted by something other than this chart's Ingress
still gets its TUNNEL_DOMAIN judged.

Two refusals, both of which otherwise fail silently at runtime:
  - a stored "*.zone", which would render "*.*.zone" and match nothing;
  - a zone that is a PARENT of ingress.host, where host_dispatch strips the
    zone off the apex, reads the leading label as a tunnel, and answers the
    whole application with the "No such tunnel" page (docs/tunnel-proxy.md).
*/}}
{{- define "nook-control.tunnelHost" -}}
{{- $h := .Values.ingress.tunnelHost -}}
{{- if $h -}}
{{- if hasPrefix "*." $h -}}
{{- fail (printf "\n\ningress.tunnelHost must be the zone itself, not a wildcard: got %q. The chart prefixes \"*.\" for you, so store e.g. tunnels.example.com." $h) -}}
{{- end -}}
{{- if and .Values.ingress.host (hasSuffix (printf ".%s" $h) .Values.ingress.host) -}}
{{- fail (printf "\n\ningress.tunnelHost (%q) is a parent of ingress.host (%q): the control plane would read your apex as a tunnel host and answer the application with its \"No such tunnel\" page. Give tunnels a zone beside the apex, not above it (docs/tunnel-proxy.md)." $h .Values.ingress.host) -}}
{{- end -}}
{{- end -}}
{{- $h -}}
{{- end -}}

{{/*
The wildcard form of the tunnel zone — the Ingress rule host and the TLS entry.
One label deep, which is why the zone stored must be exactly the one tunnels
are served under.
*/}}
{{- define "nook-control.tunnelWildcard" -}}
{{- printf "*.%s" (include "nook-control.tunnelHost" .) -}}
{{- end -}}

{{/*
The dev-login exposure, or empty (MAIN-671).

`config.authDevMode` opens POST /api/v1/auth/dev-login, which signs any caller
in as any email and CREATES the user when the email is unknown; the first user
on a fresh deployment is granted operator. Paired with an Ingress that is what
the values file above actually says: whoever reaches the host first owns the
deployment. A live install shipped exactly this pair for eight days, which is
why the guard lives here as well as in the generator that wrote it — a values
file is only one of the ways these two keys get set.

It WARNS rather than fails. The pair is not always wrong (an internal-only
ingress class, a cluster behind a VPN) and helm has no --force, so refusing
would dead-end an install the chart cannot tell apart from the dangerous one.
Empty unless both are set, so callers may branch on it directly.
*/}}
{{- define "nook-control.devLoginExposure" -}}
{{- if and .Values.config.authDevMode .Values.ingress.enabled -}}
INSECURE: config.authDevMode is true and this release publishes an Ingress at
{{ .Values.ingress.host }}. Anyone who can reach

    POST https://{{ .Values.ingress.host }}/api/v1/auth/dev-login

is signed in as any email they name — the user is CREATED when the email is
unknown — and the first user on a fresh deployment is granted operator, so the
first caller owns this deployment.

Set config.authDevMode=false (with config.appEnv=production), or leave
ingress.enabled=false so the hatch is not reachable from outside the cluster.
{{- end -}}
{{- end -}}
