#!/usr/bin/env bash
# Lint the operator-node chart and assert the rendered manifest is what MAIN-125
# promises: a StatefulSet with three persistent volumes (config, workspace,
# tmux), the join token by secretKeyRef (never a literal), the shared-operator
# designation, and the join guardrails.
#
# Run: charts/nook-operator-node/ci/validate.sh
set -euo pipefail

chart="$(cd "$(dirname "$0")/.." && pwd)"

render() { helm template operator "$chart" "$@"; }

# Minimal valid inputs: server address + a Secret holding the join token.
min=(--set server=agent.nook.example.com:8081
     --set existingSecret=nook-operator-join)

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

need "StatefulSet"                 '^kind: StatefulSet$' 1
need "no Deployment"               '^kind: Deployment$' 0
need "three volumeClaimTemplates"  '^        name: (config|workspace|tmux)$' 3
need "join token secretKeyRef"     'key: joinToken' 1
need "no chart-created Secret"     '^kind: Secret$' 0
need "shared-operator designation" 'name: NOOK_SHARED_OPERATOR' 1
need "workspace root env"          'value: /workspace' 1
need "tmux runtime dir env"        'value: /var/lib/nook/tmux' 1
need "server baked in"             'value: "agent.nook.example.com:8081"' 1

# No secret *material* may appear — only references. A real token value looks
# like `nook_join_<chars>`; the env var NAME (NOOK_JOIN_TOKEN) is not a value.
if grep -nE 'nook_join_[A-Za-z0-9]{3,}' <<<"$out" >/dev/null; then
  echo "  FAIL: rendered manifest contains a literal join token"
  fail=1
else
  echo "  ok:   no literal join token"
fi

# ── the fleet's GitHub credential (MAIN-143 AC-2, provisioned by MAIN-407) ──
#
# `optional: true` is the load-bearing part: a Secret carrying only the join
# token must still start the pod. A node with no GitHub reach is a supported
# state — it cannot take review work, and a review job then refuses by name
# rather than reporting a pass that examined nothing.
need "gh token env"                '^            - name: NOOK_GH_TOKEN$' 1
need "gh token secretKeyRef"       '^                  key: ghToken$' 1
need "gh token key is optional"    '^                  optional: true$' 1

# The key name is configurable, so assert the value is followed rather than the
# default happening to match.
renamed="$(render "${min[@]}" --set secretKeys.ghToken=fleetPat)"
if grep -qE '^                  key: fleetPat$' <<<"$renamed"; then
  echo "  ok:   secretKeys.ghToken is honoured"
else
  echo "  FAIL: secretKeys.ghToken was not honoured"
  fail=1
fi

# Same rule as the join token: references only. GitHub's classic tokens are
# `ghp_`/`gho_`/`ghs_`/`ghu_`/`ghr_`; fine-grained ones are `github_pat_`.
if grep -nE '(gh[posur]_|github_pat_)[A-Za-z0-9_]{3,}' <<<"$out" >/dev/null; then
  echo "  FAIL: rendered manifest contains a literal GitHub token"
  fail=1
else
  echo "  ok:   no literal GitHub token"
fi

# Guardrails must stop a misconfigured install with a clear message. Capture
# first — helm exits non-zero here (by design), which pipefail would surface.
noserver="$(render --set existingSecret=nook-operator-join 2>&1 || true)"
if grep -q 'server is required' <<<"$noserver"; then
  echo "  ok:   missing server is refused"
else
  echo "  FAIL: missing server was not refused"
  fail=1
fi

nosecret="$(render --set server=agent.nook.example.com:8081 2>&1 || true)"
if grep -q 'existingSecret is required' <<<"$nosecret"; then
  echo "  ok:   missing existingSecret is refused"
else
  echo "  FAIL: missing existingSecret was not refused"
  fail=1
fi

# ── egress confinement (MAIN-141) ──────────────────────────────────────────
#
# Rendered-manifest assertions, because CI has no cluster to enforce against.
# What they protect is the SHAPE: an ingress rule list that stays empty, the
# five private ranges staying subtracted, and the two escape hatches rendering
# when asked for.
need "NetworkPolicy present"       '^kind: NetworkPolicy$' 1
need "both policy types"           '^    - (Ingress|Egress)$' 2
need "five denied CIDRs"           '^              - "(10\.0\.0\.0/8|172\.16\.0\.0/12|192\.168\.0\.0/16|169\.254\.0\.0/16|100\.64\.0\.0/10)"$' 5
need "public egress base"          '^            cidr: 0\.0\.0\.0/0$' 1
need "DNS on both protocols"       '^        - protocol: (UDP|TCP)$' 2

# Deny-all ingress is the absence of a key, so assert the absence — a future
# edit that adds an `ingress:` list would open the node to being dialled.
if grep -qE '^  ingress:' <<<"$out"; then
  echo "  FAIL: an ingress rule list appeared — the node must never be dialled"
  fail=1
else
  echo "  ok:   ingress is deny-all (no rule list)"
fi

# The toggle removes the object entirely rather than rendering an empty one.
off="$(render "${min[@]}" --set networkPolicy.enabled=false)"
if grep -q '^kind: NetworkPolicy$' <<<"$off"; then
  echo "  FAIL: networkPolicy.enabled=false still rendered a policy"
  fail=1
else
  echo "  ok:   networkPolicy.enabled=false removes the policy"
fi

# The in-cluster control plane is selected by POD LABEL, not by address.
cp="$(render "${min[@]}" --set networkPolicy.controlPlane.enabled=true)"
if grep -q 'app.kubernetes.io/name: nook-control' <<<"$cp" && grep -qE '^          port: 8081$' <<<"$cp"; then
  echo "  ok:   in-cluster control-plane allowance renders"
else
  echo "  FAIL: controlPlane.enabled=true did not render its selector and port"
  fail=1
fi
if grep -q 'app.kubernetes.io/name: nook-control' <<<"$out"; then
  echo "  FAIL: the control-plane allowance rendered while disabled"
  fail=1
else
  echo "  ok:   control-plane allowance is off by default"
fi

# A private-IP control plane needs a hole punched back through the deny list.
extra="$(render "${min[@]}" --set 'networkPolicy.additionalAllowedCIDRs={10.20.0.5/32}')"
if grep -qE '^            cidr: "10\.20\.0\.5/32"$' <<<"$extra"; then
  echo "  ok:   additionalAllowedCIDRs renders an extra ipBlock"
else
  echo "  FAIL: additionalAllowedCIDRs did not render"
  fail=1
fi

# ── the loop-kind wall's shipped configuration (MAIN-142) ──────────────────
need "loop kinds env"              '^              value: "spec,decompose,review,epic-run"$' 1
need "capacity env"                '^            - name: NOOK_MAX_LOOP_JOBS$' 1

# `build` must not ship in the operator's declared kinds. The control plane
# refuses it regardless, but shipping it would state the opposite of the rule.
if grep -qE '^              value: "[^"]*build' <<<"$out"; then
  echo "  FAIL: the operator image declares a build kind"
  fail=1
else
  echo "  ok:   build is not among the shipped kinds"
fi

# Quiescing an operator is a values change, not a redeploy of something else.
quiet="$(render "${min[@]}" --set maxLoopJobs=0)"
if grep -qE '^              value: "0"$' <<<"$quiet"; then
  echo "  ok:   maxLoopJobs=0 renders (claiming disabled)"
else
  echo "  FAIL: maxLoopJobs=0 did not render"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "chart validation FAILED"
  exit 1
fi
echo "chart validation passed"
