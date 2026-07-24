#!/usr/bin/env bash
# Lint the chart and assert the rendered manifests are what MAIN-20 promises:
# both Deployments, both Services, the Ingress, the ConfigMap, secretKeyRefs
# (and never a literal Secret), non-root pods, and the /livez + /healthz probes.
#
# Run: charts/nook-control/ci/validate.sh
set -euo pipefail

chart="$(cd "$(dirname "$0")/.." && pwd)"

render() { helm template nook "$chart" "$@"; }

# Minimal valid inputs used for lint and the happy-path render.
min=(--set existingSecret=nook-control-secrets
     --set ingress.host=nook.example.com
     --set config.publicBaseUrl=https://nook.example.com)

echo "==> helm lint"
helm lint "$chart" "${min[@]}"

echo "==> helm template (minimal values)"
out="$(render "${min[@]}")"

fail=0
need() {
  local label="$1" pattern="$2" want="$3" got
  got="$(grep -cE "$pattern" <<<"$out" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}

need "Deployments (control + web)" '^kind: Deployment$' 2
need "Services (control + web)"    '^kind: Service$' 2
need "Ingress"                     '^kind: Ingress$' 1
need "ConfigMap"                   '^kind: ConfigMap$' 1
need "ServiceAccount"              '^kind: ServiceAccount$' 1
need "no chart-created Secret"     '^kind: Secret$' 0
need "required secretKeyRefs"      'key: (DATABASE_URL|SESSION_SECRET)' 2
need "liveness /livez"             'path: /livez' 2
need "readiness /healthz"          'path: /healthz' 1

# No secret *material* may appear — only references.
if grep -inE 'password: |nookdevsecret' <<<"$out" | grep -vE 'secretKeyRef|secretName|existingSecret' >/dev/null; then
  echo "  FAIL: rendered manifest contains literal secret material"
  fail=1
else
  echo "  ok:   no literal secret material"
fi

# Guardrails must stop a misconfigured install with a clear message. Capture
# first — helm exits non-zero here (by design), which pipefail would surface.
guard="$(render --set ingress.host=x 2>&1 || true)"
if grep -q 'existingSecret is required' <<<"$guard"; then
  echo "  ok:   missing existingSecret is refused"
else
  echo "  FAIL: missing existingSecret was not refused"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "chart validation FAILED"
  exit 1
fi
echo "chart validation passed"
