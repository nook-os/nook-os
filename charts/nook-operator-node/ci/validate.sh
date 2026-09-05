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

# `executor.mode=kubernetes` renders only with an apiserver address — the node
# reaches the API on a private one, which the egress deny list severs. Every
# kubernetes-mode render below carries it; the requirement itself is asserted
# separately.
k8smin=("${min[@]}" --set executor.mode=kubernetes
        --set 'networkPolicy.apiServer.cidrs={10.0.0.1/32,10.0.0.2/32}')

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
need "loop kinds env"              '^              value: "spec,decompose,review,epic-run,investigate"$' 1
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

# ── executor mode (MAIN-623) ────────────────────────────────────────────────
# The default must grant NOTHING. An upgrade that does not opt in cannot be the
# thing that hands a cluster permission to an agent.
for pattern in '^kind: Role$' '^kind: RoleBinding$' '^kind: ServiceAccount$' 'NOOK_EXECUTOR'; do
  if grep -qE "$pattern" <<<"$out"; then
    echo "  FAIL: local mode rendered $pattern"
    fail=1
  fi
done
echo "  ok:   local mode grants no RBAC and sets no executor env"

k8s="$(render "${min[@]}" "${k8smin[@]:2}" --set executor.image=ghcr.io/x/job:1)"
kneed() {
  local label="$1" pattern="$2" want="$3" got
  got="$(grep -cE "$pattern" <<<"$k8s" || true)"
  if [ "$got" -ne "$want" ]; then
    echo "  FAIL: $label — expected $want, got $got"
    fail=1
  else
    echo "  ok:   $label ($got)"
  fi
}
kneed "ServiceAccount"        '^kind: ServiceAccount$' 1
kneed "Role, not ClusterRole" '^kind: Role$' 1
kneed "no ClusterRole"        '^kind: ClusterRole' 0
kneed "RoleBinding"           '^kind: RoleBinding$' 1
kneed "pod verbs, exactly"    '^    verbs: \["create", "get", "list", "watch", "delete"\]$' 1
kneed "logs are read-only"    '^    verbs: \["get"\]$' 1
kneed "two rules and no more" '^  - apiGroups:' 2
kneed "executor env"          'NOOK_EXECUTOR$' 1

# The runtime's credential directory has to be on a VOLUME. `claude` defaults to
# $HOME/.claude, which is the container's ephemeral layer — a sign-in there is
# lost on the next restart, silently, on a StatefulSet that promises identity
# survives one (MAIN-650).
credir="$(grep -A1 'name: CLAUDE_CONFIG_DIR' <<<"$out" | awk '/value:/ {print $2}')"
mounts="$(sed -n '/volumeMounts:/,/^          [a-z]/p' <<<"$out" | awk '/mountPath:/ {print $2}')"
if [ -z "$credir" ]; then
  echo "  FAIL: no CLAUDE_CONFIG_DIR — the runtime would write to an ephemeral \$HOME"
  fail=1
elif grep -qF "$(dirname "$credir")" <<<"$mounts"; then
  echo "  ok:   the runtime credential dir sits on a persistent volume"
else
  echo "  FAIL: CLAUDE_CONFIG_DIR ${credir} is on no mounted volume — a restart signs the node out"
  fail=1
fi

# The job image (MAIN-650). `executor.mode=kubernetes` used to be unusable
# without an `executor.image` nobody could look up — the sandbox is published in
# lockstep with the node, so it is derived from the same registry and version.
appver="$(awk -F'"' '/^appVersion:/ {print $2}' "$chart/Chart.yaml")"
derived="$(render "${k8smin[@]}")"
if grep -qF "ghcr.io/nook-os/nook-job-sandbox:${appver}" <<<"$derived"; then
  echo "  ok:   job image defaults to the sandbox at appVersion ${appver}"
else
  echo "  FAIL: job image did not default to nook-job-sandbox:${appver}"
  fail=1
fi
# And an explicit value still wins, for a private mirror or a sandbox built
# from scripts/build-job-sandbox.sh.
if grep -qF 'ghcr.io/x/job:1' <<<"$k8s"; then
  echo "  ok:   an explicit executor.image overrides the derived default"
else
  echo "  FAIL: executor.image was not honoured"
  fail=1
fi

# The permission that would make a Pod job steerable, and is deliberately not
# granted: anything holding it can write into a running container.
if grep -qE 'pods/(attach|exec|portforward)' <<<"$k8s"; then
  echo "  FAIL: the Role grants attach/exec/port-forward"
  fail=1
else
  echo "  ok:   no attach, exec or port-forward"
fi

# AC-10. A Pod's env is returned by `get pods`, which the Role above grants
# across this namespace — so a credential must arrive by reference or not at
# all. Unset is a WORKING state, and the chart must not invent a Secret name.
kneed "no credential secret by default" 'NOOK_JOB_CREDENTIALS_SECRET' 0

creds=$(render "${min[@]}" "${k8smin[@]:2}" --set executor.image=i:1 \
  --set executor.credentialsSecret=nook-job-credentials)
if grep -q 'NOOK_JOB_CREDENTIALS_SECRET' <<<"$creds" \
   && grep -q 'nook-job-credentials' <<<"$creds"; then
  echo "  ok:   a named credential Secret is passed through"
else
  echo "  FAIL: executor.credentialsSecret did not reach the pod"
  fail=1
fi

# The Role must NOT gain a secrets verb for this: the agent references a
# hand-created Secret and never reads, writes or creates one.
# No credential Secret named ⇒ no secrets verb at all.
if grep -qE '^    resources: \[.*"secrets".*\]' <<<"$k8s"; then
  echo "  FAIL: the Role grants a secrets verb with no credentialsSecret named"
  fail=1
else
  echo "  ok:   no secrets verb by default"
fi

# MAIN-650. Naming one DOES change the Role now, and the shape of that change is
# the whole security argument: the control plane delivers the credential to this
# node, and on a Pod executor the only place a job can read it is the Secret. So
# the node may write that ONE Secret, by name.
if grep -qE '^    resourceNames: \["nook-job-credentials"\]' <<<"$creds"; then
  echo "  ok:   the secrets rule is pinned to the named Secret"
else
  echo "  FAIL: the secrets rule is not restricted by resourceNames"
  fail=1
fi
# `list` and `watch` IGNORE resourceNames — either one turns "this Secret" into
# "every Secret in the namespace" without looking like it.
secrule="$(sed -n '/resources: \["secrets"\]/,/verbs:/p' <<<"$creds")"
if grep -qE '"(list|watch|delete|deletecollection)"' <<<"$secrule"; then
  echo "  FAIL: the secrets rule grants a verb resourceNames cannot restrict"
  fail=1
else
  echo "  ok:   no list/watch on secrets"
fi
if grep -qE 'verbs: \["get", "create", "patch"\]' <<<"$secrule"; then
  echo "  ok:   secrets verbs are exactly get/create/patch"
else
  echo "  FAIL: unexpected secrets verbs: $secrule"
  fail=1
fi
# And the POD's own permissions are untouched — a mounted Secret is resolved by
# the kubelet, so nothing about what the JOB may do has changed. Compared as the
# resource/verb pairs that are not the secrets rule, so a verb quietly added to
# the pods rule is still caught.
podrules() {
  # Drop the secrets stanza whole — it is three lines where a pods rule is two,
  # so anything that pairs lines misaligns on it rather than ignoring it.
  sed -n '/^rules:/,/^---/p' <<<"$1" \
    | grep -E '^    (resources|verbs|resourceNames):' \
    | awk '/"secrets"/ { skip = 3 } skip > 0 { skip--; next } { print }'
}
if diff <(podrules "$k8s") <(podrules "$creds") >/dev/null; then
  echo "  ok:   naming a Secret adds no OTHER permission"
else
  echo "  FAIL: naming a credential Secret changed more than the secrets rule"
  diff <(podrules "$k8s") <(podrules "$creds") | head -6
  fail=1
fi

# MAIN-669 AC-5. The Secret carries the fleet's Claude SESSION, which is a
# directory and not a variable — so the README has to say what goes in it, who
# creates it, what it is (scaffolding), and who can read it. Every one of those
# is a thing an operator gets wrong silently, and the last is a security
# property nobody should have to infer.
readme="$chart/README.md"
readme_says() {
  if grep -qF "$2" "$readme"; then
    echo "  ok:   README states $1"
  else
    echo "  FAIL: README does not state $1 (looked for: $2)"
    fail=1
  fi
}
readme_says "what the Secret must contain" '.credentials.json'
readme_says "…and its configuration half"  '.claude.json'
readme_says "where it is mounted"          'CLAUDE_CONFIG_DIR'
readme_says "that a human creates it"      '**A human creates that Secret.**'
readme_says "that it is scaffolding"       'scaffolding pending MAIN-337'
readme_says "who can read it"              'readable by any agent this node runs'
readme_says "subscription login only"      'never an API key'

# MAIN-672 AC-4. What a refresh CANNOT persist is the one property of this
# mechanism an operator cannot discover by reading the chart: the Pod's copy of
# the session dies with the Pod, so a refreshed credential never reaches the
# Secret and the Secret goes stale on its own schedule. Left unsaid, a fleet
# that worked for a fortnight stops for a reason nobody can name.
readme_says "that the session is a snapshot" 'A Pod-mounted session is a snapshot'
readme_says "that nothing refreshes it"      '**Nothing refreshes the Secret.**'
readme_says "which token ends it"            'The refresh token expiring is not.'
readme_says "that re-seeding is a human"     'until a human replaces the Secret'
readme_says "who owns closing the gap"       "Automatic re-seeding is MAIN-337's"

# Both or neither: half a build pool reads as protection and is not.
if render "${min[@]}" "${k8smin[@]:2}" --set executor.image=i:1 \
     --set executor.buildPool.taint=t >/dev/null 2>&1; then
  echo "  FAIL: a taint with no selector rendered"
  fail=1
else
  echo "  ok:   half a build pool is refused"
fi

# ── builds in cluster (MAIN-655) ────────────────────────────────────────────
# `build` is the one privileged kind. It is reachable here, but only with a pool
# of its own — the chart refuses to render one half of that arrangement, because
# a privileged Pod on a general node pool is the thing the control plane's wall
# exists to prevent.
# The apiserver hole is REQUIRED in kubernetes mode: without it the node joins,
# reports healthy, and every reconcile fails against an API the policy severed.
if render "${min[@]}" --set executor.mode=kubernetes >/dev/null 2>&1; then
  echo "  FAIL: kubernetes mode rendered with no networkPolicy.apiServer.cidr"
  fail=1
else
  echo "  ok:   kubernetes mode without an apiserver address is refused"
fi
# Both addresses, because the ClusterIP alone is the trap: a CNI that DNATs it
# before evaluating policy never matches such a rule, and the failure is
# indistinguishable from setting nothing (MAIN-650).
apirule="$(render "${k8smin[@]}")"
if grep -qE 'cidr: "10\.0\.0\.1/32"' <<<"$apirule" && grep -qE 'cidr: "10\.0\.0\.2/32"' <<<"$apirule"; then
  echo "  ok:   every apiserver address renders"
else
  echo "  FAIL: the apiserver egress rule did not render both addresses"
  fail=1
fi
# …and it is skipped entirely when the policy is off, rather than demanded.
if render "${min[@]}" --set executor.mode=kubernetes \
     --set networkPolicy.enabled=false >/dev/null 2>&1; then
  echo "  ok:   networkPolicy.enabled=false needs no apiserver address"
else
  echo "  FAIL: kubernetes mode is blocked even with the policy off"
  fail=1
fi

echo "==> helm template (build kind)"
if render "${min[@]}" "${k8smin[@]:2}" --set 'loopKinds={spec,build}' \
     >/dev/null 2>&1; then
  echo "  FAIL: loopKinds=build rendered with no executor.buildPool"
  fail=1
else
  echo "  ok:   build without a pool is refused"
fi
if render "${min[@]}" --set 'loopKinds={spec,build}' \
     --set executor.buildPool.selector=nook.io/pool=build \
     --set executor.buildPool.taint=nook.io/build >/dev/null 2>&1; then
  echo "  FAIL: loopKinds=build rendered in local mode — a containerised node runs no builds"
  fail=1
else
  echo "  ok:   build outside kubernetes mode is refused"
fi
pooled="$(render "${min[@]}" "${k8smin[@]:2}" --set 'loopKinds={spec,build}' \
  --set executor.buildPool.selector=nook.io/pool=build \
  --set executor.buildPool.taint=nook.io/build 2>/dev/null)"
if grep -q 'value: "spec,build"' <<<"$pooled"; then
  echo "  ok:   build renders when a pool is named"
else
  echo "  FAIL: build did not render even with a pool"
  fail=1
fi
# The default must stay safe: nothing about installing this chart grants builds.
if grep -qE '^loopKinds:' -A8 "$chart/values.yaml" && \
   sed -n '/^loopKinds:/,/^[a-z]/p' "$chart/values.yaml" | grep -q '^  - build$'; then
  echo "  FAIL: the chart's DEFAULT loopKinds offers build"
  fail=1
else
  echo "  ok:   build is not a default"
fi

if [ "$fail" -ne 0 ]; then
  echo "chart validation FAILED"
  exit 1
fi
echo "chart validation passed"
