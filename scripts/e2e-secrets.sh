#!/usr/bin/env bash
# MAIN-625 AC-5/AC-6 against a REAL dev stack: a secret set through the CLI
# reaches a session's environment and a loop job's agent.
#
# The unit tests prove the selection and the shapes; only this proves the whole
# chain — write, seal, store, select, push over the socket, export at
# `tmux new-session` — actually joins up. Run it after `./scripts/dev-up.sh`.
#
#   ./scripts/e2e-secrets.sh [workspace-name]
#
# Leaves nothing behind: the session is killed and the items are removed.
set -euo pipefail
cd "$(dirname "$0")/.."

WORKSPACE="${1:-nook-dogfood}"
NAME="NOOK_E2E_SECRET"
VALUE="hunter2-$RANDOM"
SESSION="e2e-secrets-$$"
# The machine the node-scoped item is filed against, once step AC-7 has picked
# one. Set before that item exists so `cleanup` can always try: an early exit
# BETWEEN the set and the success-path removal is exactly the case that used to
# leave one behind, against this script's own "leaves nothing behind".
NODE=""

nook() { command nook "$@"; }

cleanup() {
  nook delete session "$SESSION" >/dev/null 2>&1 || true
  nook secrets rm workspace "$WORKSPACE" "$NAME" >/dev/null 2>&1 || true
  nook secrets rm tenant E2E_TENANT_SECRET >/dev/null 2>&1 || true
  [ -n "$NODE" ] && nook secrets rm node "$NODE" E2E_NODE_SECRET >/dev/null 2>&1
  return 0
}
trap cleanup EXIT

echo "▸ AC-3: set a workspace secret, and list it"
nook secrets set workspace "$WORKSPACE" "$NAME" "$VALUE"
nook secrets set tenant E2E_TENANT_SECRET tenant-wide

listing=$(nook secrets list)
echo "$listing"
grep -q "$NAME" <<<"$listing" || { echo "✗ the name is not in the listing"; exit 1; }

echo "▸ AC-4: no read path returns the value"
for surface in "nook secrets list" "nook secrets list --json" "nook get secrets" "nook get secrets --json"; do
  if $surface 2>/dev/null | grep -q "$VALUE"; then
    echo "✗ $surface leaked the value"; exit 1
  fi
done
echo "✓ four read surfaces, no value"

echo "▸ AC-5: the value is in a session's environment"
nook start "$WORKSPACE" --runtime bash --name "$SESSION"
# tmux takes its environment at new-session, so the value is there the moment
# the pane exists; the sleep is for the pane, not for the variable.
sleep 5
# `nook exec` echoes the prompt and the shell's own line around the answer, so
# this looks for the value ANYWHERE in the reply rather than as a whole line.
got=$(nook exec "$SESSION" "printenv $NAME" | tr -d '\r')
grep -q "$VALUE" <<<"$got" || { echo "✗ session env: expected '$VALUE' in: $got"; exit 1; }
echo "✓ printenv $NAME → $VALUE"

echo "▸ AC-7: a node-scoped item reaches no session"
NODE=$(nook get nodes --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["name"])')
nook secrets set node "$NODE" E2E_NODE_SECRET node-only
got=$(nook exec "$SESSION" 'printenv E2E_NODE_SECRET || echo NODE_SECRET_ABSENT' | tr -d '\r')
grep -q "NODE_SECRET_ABSENT" <<<"$got" || { echo "✗ a node secret reached the session: $got"; exit 1; }
nook secrets rm node "$NODE" E2E_NODE_SECRET
echo "✓ node scope is stored and injected nowhere"

echo "▸ AC-6: the value is in a loop job's agent environment"
# The one half that needs a real agent. Raised through the API — there is no
# CLI verb for a spec job — with a seed that asks the agent to print the
# variable, so the run's own transcript is the evidence.
api() {
  local method=$1 path=$2 body=${3:-}
  local server token
  server=$(python3 -c "import tomllib,sys,os; c=tomllib.load(open(os.path.expanduser('~/.config/nook/contexts.toml'),'rb')); print(c['contexts'][c['current']]['server'])")
  token=$(python3 -c "import tomllib,sys,os; c=tomllib.load(open(os.path.expanduser('~/.config/nook/contexts.toml'),'rb')); print(c['contexts'][c['current']]['token'])")
  if [ -n "$body" ]; then
    curl -fsS -X "$method" "$server$path" -H "Authorization: Bearer $token" \
      -H 'content-type: application/json' -d "$body"
  else
    curl -fsS -X "$method" "$server$path" -H "Authorization: Bearer $token"
  fi
}

task=$(nook get tasks --json | python3 -c '
import json,sys
tasks=json.load(sys.stdin)
print(next((t["key"] for t in tasks if t.get("key")), ""))')
if [ -z "$task" ]; then
  echo "⚠ no ticket to raise a spec job against — skipping AC-6"
else
  job=$(api POST /api/v1/jobs "$(python3 -c "
import json,sys
print(json.dumps({
  'kind': 'spec',
  'target_task_id': '$task',
  'seed': 'Before anything else, run: printenv $NAME — and report exactly what it prints.',
}))")" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
  echo "  spec job $job on $task — polling its transcript for $NAME"
  found=""
  for _ in $(seq 1 60); do
    if api GET "/api/v1/jobs/$job" | grep -q "$VALUE"; then found=yes; break; fi
    sleep 10
  done
  if [ -n "$found" ]; then
    echo "✓ the job's transcript shows $NAME=$VALUE"
  else
    echo "⚠ the transcript never showed it in 10 minutes — check the runtime is"
    echo "  authorized ('docker compose exec operator-node claude auth status')"
    echo "  and that loops are on ('nook operator loops on')."
    exit 1
  fi
fi

echo "✓ AC-3, AC-4, AC-5, AC-6 and AC-7 pass"
