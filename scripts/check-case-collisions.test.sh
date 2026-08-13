#!/usr/bin/env bash
#
# Self-test for check-case-collisions.sh, in the house style: prove the guard
# DETECTS an injected collision, not merely that it passes on a clean tree.
# A guard that cannot fail is not a guard, and this one's whole value is that
# it fires on a platform nobody develops on.
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."
GUARD=./scripts/check-case-collisions.sh

fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# 1. A genuine collision is reported.
out="$(printf '%s\n' src/BuildLoop.ts src/buildLoop.ts | "$GUARD" --filter)"
[[ -n "$out" ]] || fail "did not detect BuildLoop.ts vs buildLoop.ts"
grep -q 'BuildLoop.ts' <<<"$out" || fail "report omits the colliding path"

# 2. Paths differing by more than case are NOT reported — the fix must pass.
out="$(printf '%s\n' src/BuildLoop.tsx src/buildRuns.ts | "$GUARD" --filter)"
[[ -z "$out" ]] || fail "false positive on distinct names: $out"

# 3. The same path twice is not a collision with itself.
out="$(printf '%s\n' src/a.ts src/a.ts | "$GUARD" --filter)"
[[ -z "$out" ]] || fail "reported a path as colliding with itself"

# 4. Collisions in different directories are independent.
out="$(printf '%s\n' a/x.ts b/X.ts | "$GUARD" --filter)"
[[ -z "$out" ]] || fail "reported paths in different directories: $out"

# 5. Directory-level case collisions count too — a checkout cannot represent
#    them either.
out="$(printf '%s\n' Foo/x.ts foo/x.ts | "$GUARD" --filter)"
[[ -n "$out" ]] || fail "did not detect a directory-level collision"

# 6. And the real tree is clean.
"$GUARD" >/dev/null || fail "the tree itself has a case collision"

echo "✓ check-case-collisions.sh self-test passed"
