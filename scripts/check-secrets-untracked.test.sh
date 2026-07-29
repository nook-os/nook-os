#!/usr/bin/env bash
# MAIN-238 AC-5: the fleet's Claude credentials must never be tracked.
#
# A gitignore entry is a claim; this checks the claim. It asks git directly
# rather than reading `.gitignore`, so a file force-added past the ignore rule —
# the way credentials actually get committed — still fails.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

tracked="$(git ls-files -- '.nook-secrets' '.nook-secrets/**' | head -20)"
if [ -n "$tracked" ]; then
  echo "✗ credential files are TRACKED by git:" >&2
  echo "$tracked" >&2
  echo "  Remove them from the index (git rm --cached) — they are real secrets." >&2
  fail=1
fi

# The ignore rule itself must hold for a path that does not exist yet, which is
# the state a fresh clone is in.
if ! git check-ignore -q .nook-secrets/claude/.claude.json; then
  echo "✗ .nook-secrets/claude/.claude.json is NOT ignored by .gitignore" >&2
  fail=1
fi

# NG-1 / AC-5: no API-key path may creep into the dev wiring. The rule is
# subscription device-login only, and the cheapest way for that to rot is
# someone adding an env var "just for local testing".
if grep -rn 'ANTHROPIC_API_KEY' docker-compose.yml run.sh .env.example 2>/dev/null; then
  echo "✗ an ANTHROPIC_API_KEY path appeared in the dev wiring — device-login only." >&2
  fail=1
fi

[ "$fail" = "0" ] && echo "✓ credentials untracked, ignored, and no API-key path"
exit "$fail"
