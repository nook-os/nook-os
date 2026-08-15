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
need "no agent Service by default" 'component: agent' 0
need "Ingress"                     '^kind: Ingress$' 1
need "ConfigMap"                   '^kind: ConfigMap$' 1
need "ServiceAccount"              '^kind: ServiceAccount$' 1
need "no chart-created Secret"     '^kind: Secret$' 0
need "required secretKeyRefs"      'key: (DATABASE_URL|SESSION_SECRET)' 2
# MAIN-408: the review sweep's cadence is operator-tunable from the chart. The
# switch that turns the sweep ON is a per-tenant runtime setting, not a chart
# value — deliberately, so enabling agents to review repositories is a decision
# made against a live tenant rather than baked into a deploy.
need "liveness /livez"             'path: /livez' 2
need "readiness /healthz"          'path: /healthz' 1
# MAIN-598: uploads get a home of their own. Without both of these the control
# plane writes into the image's read-only dist and every upload 500s.
need "upload PVC"                  '^kind: PersistentVolumeClaim$' 1
need "NOOK_USER_CONTENT_DIR set"   'NOOK_USER_CONTENT_DIR: "/var/lib/nook/user-content"' 1
need "upload volume mounted"       'mountPath: /var/lib/nook/user-content' 1

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

# ── Agent mTLS listener (opt-in) ─────────────────────────────────────────────
echo "==> helm template (agent.enabled)"
agentout="$(render "${min[@]}" \
  --set agent.enabled=true \
  --set agent.tlsSecret=nook-agent-tls \
  --set agent.publicUrl=agent.nook.example.com:8081)"

aneed() {
  local label="$1" pattern="$2" want="$3" got
  got="$(grep -cE "$pattern" <<<"$agentout" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}

aneed "agent Service rendered"      'component: agent' 1
aneed "three Services now"          '^kind: Service$' 3
aneed "LoadBalancer passthrough"    'type: LoadBalancer' 1
aneed "cert env is a file path"     'NOOK_AGENT_TLS_CERT' 1
aneed "public URL baked in"         'value: "agent.nook.example.com:8081"' 1
aneed "cert Secret mounted"         'secretName: nook-agent-tls' 1
if grep -A1 'NOOK_AGENT_TLS_CERT' <<<"$agentout" | grep -q '/etc/nook/agent/tls.crt'; then
  echo "  ok:   NOOK_AGENT_TLS_CERT points at the mount path"
else
  echo "  FAIL: NOOK_AGENT_TLS_CERT is not the mount path"
  fail=1
fi

# Half-configured (enabled, but no cert) must be refused, not half-rendered.
agentguard="$(render "${min[@]}" --set agent.enabled=true --set agent.publicUrl=x:8081 2>&1 || true)"
if grep -q 'agent.tlsSecret' <<<"$agentguard"; then
  echo "  ok:   agent.enabled without a cert is refused"
else
  echo "  FAIL: agent.enabled without a cert was not refused"
  fail=1
fi

# ── Uploads without a PVC (MAIN-598) ─────────────────────────────────────────
# The documented ephemeral case: no claim, but the directory is still mounted
# and still named, so the control plane never falls back to the image.
echo "==> helm template (userContent.persistence.enabled=false)"
ephemeral="$(render "${min[@]}" --set userContent.persistence.enabled=false)"
if grep -q '^kind: PersistentVolumeClaim$' <<<"$ephemeral"; then
  echo "  FAIL: a PVC was rendered with persistence disabled"
  fail=1
elif grep -q 'emptyDir: {}' <<<"$ephemeral" &&
     grep -q 'mountPath: /var/lib/nook/user-content' <<<"$ephemeral"; then
  echo "  ok:   emptyDir mounted at the upload directory"
else
  echo "  FAIL: persistence=false did not mount an emptyDir at the upload directory"
  fail=1
fi

# ── Dev-mode, log level, and mail config (MAIN-62) ───────────────────────────
echo "==> helm template (authDevMode + logLevel + mail)"
cfgout="$(render "${min[@]}" \
  --set config.appEnv=dev \
  --set config.authDevMode=true \
  --set config.logLevel=debug \
  --set config.mail.provider=postmark \
  --set config.mail.from='NookOS <no-reply@nook.example.com>' \
  --set config.mail.sendEnabled=true \
  --set config.mail.notificationsEnabled=true \
  --set config.mail.maxPerMonth=100 \
  --set config.mail.smtpHost=smtp.example.com \
  --set config.mail.smtpPort=587 \
  --set config.mail.smtpTls=starttls \
  --set config.mail.smtpUsername=nook \
  --set config.mail.postmarkApiUrl=https://api.postmarkapp.com \
  --set secretKeys.smtpPassword=SMTP_PASSWORD \
  --set secretKeys.postmarkToken=POSTMARK_TOKEN)"

cneed() {
  local label="$1" pattern="$2" want="$3" got
  got="$(grep -cE "$pattern" <<<"$cfgout" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}

cneed "AUTH_DEV_MODE rendered"       'AUTH_DEV_MODE: "true"' 1
cneed "RUST_LOG rendered"            'RUST_LOG: "debug"' 1
cneed "MAIL_PROVIDER"                'MAIL_PROVIDER: "postmark"' 1
cneed "MAIL_FROM"                    'MAIL_FROM: ' 1
cneed "MAIL_SEND_ENABLED"            'MAIL_SEND_ENABLED: "true"' 1
cneed "MAIL_NOTIFICATIONS_ENABLED"   'MAIL_NOTIFICATIONS_ENABLED: "true"' 1
cneed "MAIL_MAX_PER_MONTH"           'MAIL_MAX_PER_MONTH: "100"' 1
cneed "SMTP_HOST/PORT/TLS/USERNAME"  'SMTP_(HOST|PORT|TLS|USERNAME): ' 4
cneed "POSTMARK_API_URL"             'POSTMARK_API_URL: ' 1
cneed "SMTP_PASSWORD secretKeyRef"   'name: SMTP_PASSWORD' 1
cneed "POSTMARK_TOKEN secretKeyRef"  'name: POSTMARK_TOKEN' 1

# The dev-mode hatch must be refused in production — the chart mirrors the
# control plane, which will not boot on that combo.
devguard="$(render "${min[@]}" --set config.authDevMode=true --set config.appEnv=production 2>&1 || true)"
if grep -q 'authDevMode=true is incompatible' <<<"$devguard"; then
  echo "  ok:   authDevMode=true + appEnv=production is refused"
else
  echo "  FAIL: authDevMode=true + appEnv=production was not refused"
  fail=1
fi

# Additive: with none of the new keys set, the manifest is what it was before —
# every new env key omitted, so no MAIL_/RUST_LOG/AUTH_DEV_MODE lines appear.
baseout="$(render "${min[@]}")"
if grep -qE 'AUTH_DEV_MODE|RUST_LOG|MAIL_|SMTP_PASSWORD|POSTMARK_' <<<"$baseout"; then
  echo "  FAIL: a new env key leaked into the default render (breaks additive-ness)"
  fail=1
else
  echo "  ok:   default render omits every new key (additive)"
fi

# ── Worker + queue provider + KEDA matrix (MAIN-153) ─────────────────────────
# AC-4: render all three providers × KEDA on/off and assert the worker
# Deployment, the provider-matched env, and the matching KEDA trigger.
echo "==> helm template (worker matrix: database|redis|sqs × KEDA on/off)"

# Default (worker off) renders no worker Deployment / ScaledObject — additive.
if grep -qE 'component: worker' <<<"$baseout"; then
  echo "  FAIL: the worker Deployment leaked into the default render (not additive)"
  fail=1
else
  echo "  ok:   worker off by default (additive)"
fi

wmatrix() {
  local label="$1" out="$2" pattern="$3" want="$4" got
  got="$(grep -cE "$pattern" <<<"$out" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}

for provider in database redis sqs; do
  extra=()
  case "$provider" in
    redis) extra=(--set secretKeys.redisUrl=REDIS_URL) ;;
    sqs) extra=(--set queue.sqs.queueUrl=https://sqs.us-east-1.amazonaws.com/1/q
                --set queue.sqs.region=us-east-1) ;;
  esac

  # KEDA off: worker Deployment renders, no ScaledObject.
  off="$(render "${min[@]}" --set worker.enabled=true --set queue.provider="$provider" "${extra[@]}")"
  wmatrix "$provider: 3 Deployments (worker on)" "$off" '^kind: Deployment$' 3
  wmatrix "$provider: NOOK_QUEUE_PROVIDER"    "$off" "NOOK_QUEUE_PROVIDER: \"$provider\"" 1
  wmatrix "$provider: no ScaledObject (KEDA off)" "$off" '^kind: ScaledObject$' 0

  # KEDA on: the ScaledObject + the provider-matched trigger render.
  on="$(render "${min[@]}" --set worker.enabled=true --set queue.provider="$provider" \
        --set autoscaling.keda.enabled=true "${extra[@]}")"
  wmatrix "$provider: ScaledObject (KEDA on)" "$on" '^kind: ScaledObject$' 1
  case "$provider" in
    database)
      wmatrix "$provider: postgresql trigger" "$on" 'type: postgresql' 1
      # The trigger's definition of READY must be the queue's own (MAIN-411).
      # `crates/nook-infra/src/queue/database.rs` is authoritative: `receive`
      # claims, and `describe` counts, exactly this set. A trigger counting more
      # than the worker can claim scales up for work nobody can do.
      wmatrix "$provider: trigger counts only runnable rows" "$on" \
        'query: "SELECT count\(\*\) FROM work_queue WHERE \(locked_until IS NULL OR locked_until <= now\(\)\) AND not_before <= now\(\)"' 1
      # The parentheses specifically, because losing them is the silent form of
      # the bug: `AND` binds tighter, so an unparenthesised `OR` counts every
      # unlocked row whatever its `not_before`.
      wmatrix "$provider: trigger's OR is parenthesised" "$on" \
        'WHERE \(locked_until' 1
      ;;
    redis)    wmatrix "$provider: redis trigger"      "$on" 'type: redis' 1 ;;
    sqs)      wmatrix "$provider: aws-sqs trigger"    "$on" 'type: aws-sqs-queue' 1 ;;
  esac
  # Still no literal secret material anywhere in the KEDA-on render.
  if grep -inE 'password: |nookdevsecret' <<<"$on" | grep -vE 'secretKeyRef|secretName|TriggerAuth|name:' >/dev/null; then
    echo "  FAIL: $provider KEDA render leaked secret material"
    fail=1
  fi
done

# ── Tunnel wildcard rule (MAIN-512) ──────────────────────────────────────────
# Two states, and the ABSENT one is load-bearing: unset must render exactly the
# manifests it rendered before this value existed — one rule, one TLS host, no
# TUNNEL_DOMAIN — because every deployment that never wants tunnels is on that
# path.
need "one Ingress rule by default"   '^    - host: ' 1
need "no wildcard host by default"   '^    - host: "\\*\\.' 0
need "no TUNNEL_DOMAIN by default"   '^  TUNNEL_DOMAIN: ' 0

tlsout="$(render "${min[@]}" --set ingress.tls.enabled=true --set ingress.tls.secretName=nook-tls)"
if [ "$(grep -cE '^        - "' <<<"$tlsout")" -ne 1 ]; then
  echo "  FAIL: default TLS render should carry exactly one host"
  fail=1
else
  echo "  ok:   one TLS host by default"
fi

echo "==> helm template (ingress.tunnelHost)"
tunout="$(render "${min[@]}" \
  --set ingress.tunnelHost=tunnels.example.com \
  --set ingress.tls.enabled=true --set ingress.tls.secretName=nook-tls)"

tneed() {
  local label="$1" pattern="$2" want="$3" got
  got="$(grep -cE "$pattern" <<<"$tunout" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}

tneed "two Ingress rules"        '^    - host: ' 2
tneed "apex rule unchanged"      '^    - host: "nook\.example\.com"$' 1
tneed "wildcard rule"            '^    - host: "\*\.tunnels\.example\.com"$' 1
tneed "wildcard joins tls.hosts" '^        - "\*\.tunnels\.example\.com"$' 1
tneed "apex still in tls.hosts"  '^        - "nook\.example\.com"$' 1
tneed "TUNNEL_DOMAIN from the same value" '^  TUNNEL_DOMAIN: "tunnels\.example\.com"$' 1

# The backend is the whole point: every path on a tunnel host is the control
# plane's, and a wildcard rule copied from the apex inherits `web`, whose SPA
# fallback answers 200 with the app shell on every path (docs/tunnel-proxy.md).
if awk '/^    - host: "\*\.tunnels\.example\.com"$/,/^---$/' <<<"$tunout" \
     | grep -q 'name: nook-nook-control-control'; then
  echo "  ok:   the tunnel rule's backend is the control-plane Service"
else
  echo "  FAIL: the tunnel rule does not point at the control-plane Service"
  fail=1
fi
if awk '/^    - host: "nook\.example\.com"$/,/^    - host: "\*/' <<<"$tunout" \
     | grep -q 'name: nook-nook-control-web'; then
  echo "  ok:   the apex rule still points at web"
else
  echo "  FAIL: the apex rule no longer points at web"
  fail=1
fi

# A stored wildcard would render "*.*.zone"; a zone above the apex would make
# the control plane read the apex as a tunnel. Both are refused by name.
tunguard="$(render "${min[@]}" --set ingress.tunnelHost='*.tunnels.example.com' 2>&1 || true)"
if grep -q 'must be the zone itself' <<<"$tunguard"; then
  echo "  ok:   a wildcard-shaped tunnelHost is refused"
else
  echo "  FAIL: a wildcard-shaped tunnelHost was not refused"
  fail=1
fi
parentguard="$(render "${min[@]}" --set ingress.tunnelHost=example.com 2>&1 || true)"
if grep -q 'is a parent of ingress.host' <<<"$parentguard"; then
  echo "  ok:   a tunnelHost above the apex is refused"
else
  echo "  FAIL: a tunnelHost above the apex was not refused"
  fail=1
fi

# ── The fleet GitHub token (MAIN-448) ────────────────────────────────────────
# The control plane reads this to size a repo's review loops to its open PRs.
# Two assertions, and the ABSENCE one is the load-bearing half: a deployment
# with no token is a supported state — review loops run at their declared
# ceiling, exactly as before the forge — so an unset `ghToken` must render no
# env entry at all rather than a reference to a key nobody put in the Secret.
need "no gh token env by default"  '^            - name: NOOK_GH_TOKEN$' 0

echo "==> helm template (secretKeys.ghToken)"
ghout="$(render "${min[@]}" --set secretKeys.ghToken=ghToken)"

ghneed() {
  local label="$1" pattern="$2" want="$3" got
  got="$(grep -cE "$pattern" <<<"$ghout" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}

ghneed "gh token env"              '^            - name: NOOK_GH_TOKEN$' 1
ghneed "gh token by reference"     '^                  key: ghToken$' 1
# `optional: true` for the reason the operator node's chart gives: a Secret
# carrying everything BUT this key must still start the pod. Dropping the line
# renders a control plane that CrashLoops on a Secret that was valid yesterday.
ghneed "gh token key is optional"  '^                  optional: true$' 1
if grep -qE 'NOOK_GH_TOKEN' <<<"$ghout" && grep -A2 'name: NOOK_GH_TOKEN' <<<"$ghout" | grep -q 'value:'; then
  echo "  FAIL: NOOK_GH_TOKEN is rendered as a literal, not a reference"
  fail=1
else
  echo "  ok:   NOOK_GH_TOKEN is a reference, never a literal"
fi

if [ "$fail" -ne 0 ]; then
  echo "chart validation FAILED"
  exit 1
fi
echo "chart validation passed"
