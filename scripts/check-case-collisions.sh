#!/usr/bin/env bash
#
# check-case-collisions.sh — refuse two tracked paths that differ only in CASE.
#
# WHY THIS EXISTS. `frontend/packages/app/src/` held both `BuildLoop.tsx` (the
# component) and `buildLoop.ts` (the helpers). On Linux those are two distinct
# modules and everything resolves, so `frontend` CI and every developer's
# `pnpm -r typecheck` passed. On a case-INSENSITIVE filesystem — the macOS
# runner that builds the desktop app — they are one file. TypeScript then
# reported TS1149 ("differs from already included file name only in casing")
# and TS2724 for the export it could no longer see, and the RELEASE build died
# there, hours after the change had merged clean.
#
# That is the shape worth guarding: green on every machine anyone develops on,
# red only on the one platform nobody runs locally, and discovered at release
# time. A checkout on macOS or Windows cannot even represent both files, so it
# is also a repository that will not clone correctly there.
#
# Naming both files by the repo's own conventions is what created it —
# PascalCase for a component, camelCase for a module — so this is not a style
# violation anyone would spot in review. Only a machine notices.
set -euo pipefail
export LC_ALL=C

# The detection, separated from the tree so the self-test can drive it with a
# fixture instead of a real collision (which this repo, by construction, must
# never contain).
collisions() { # reads paths on stdin, prints one "a <-> b" line per collision
  awk '{
    key = tolower($0)
    if (key in seen && seen[key] != $0) print seen[key] "  <->  " $0
    else seen[key] = $0
  }'
}

# `--self-test-filter` is the seam the test script uses; no other caller passes it.
if [[ "${1:-}" == "--filter" ]]; then
  collisions
  exit 0
fi

found="$(git ls-files | collisions || true)"
if [[ -n "$found" ]]; then
  printf '\033[1;31m✗ paths differing only in case:\033[0m\n' >&2
  printf '%s\n' "$found" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'

  These cannot coexist on a case-insensitive filesystem (macOS, Windows). The
  build is green on Linux and fails on the macOS runner — historically at
  RELEASE time, which is the most expensive moment to find out.

  Rename one so the two names differ by more than case.
MSG
  exit 1
fi
echo "✓ no case-colliding paths"
