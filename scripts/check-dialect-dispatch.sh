#!/usr/bin/env bash
# MAIN-289: production must ask the ENGINE for its SQL fragments, not assume
# Postgres.
#
# The dialect seam has a Postgres arm, a SQLite arm and dispatchers. What it
# did not have was anything stopping production calling the Postgres arm
# directly — 206 sites did, so a `sqlite://` bed emitted `now()` and `FOR
# UPDATE` and failed by construction. The fix is per call site; this is what
# stops the next one being added.
#
# THE RULE IS "do not add a hardcoded Postgres.<fragment>() to a file that has
# none", not "no Postgres arm anywhere". Files still holding one legitimately —
# the ones whose sweep has not landed — are named in the allow-list beside this
# script. The list shrinks as they land; it must never grow silently.
#
# crates/nook-db is out of scope: it DEFINES both arms, which is its whole job.
set -euo pipefail
cd "$(dirname "$0")/.."

ALLOWLIST="scripts/dialect-dispatch-allowlist.txt"
mode="${1:-check}"

# The hardcoded arm. `type_mapping(…)`, `time_math(…)` and `json(…)` are the
# dispatchers and are exactly what this is pushing people towards, so they are
# not matched.
#
# shellcheck disable=SC2016
hits() {
  find crates -name '*.rs' -not -path 'crates/nook-db/*' -print0 |
    sort -z |
    xargs -0 awk '
      FNR == 1 { depth = 0; intest = 0; pending = 0 }
      {
        line = $0; opens = gsub(/[{]/, "{", line)
        line = $0; closes = gsub(/[}]/, "}", line)

        if (!intest && $0 ~ /^[ \t]*#\[cfg\(test\)\]/) pending = 1

        # A test may assert the Postgres arm directly — that is what pins the
        # arm itself — so #[cfg(test)] is skipped, by BRACES rather than to
        # end-of-file (the MAIN-249 lesson: production code below an inline
        # test module is all it takes to get this wrong).
        code = $0
        sub(/\/\/.*/, "", code)
        if (!intest && code ~ /\yPostgres\.[a-z_]+\(/)
          print FILENAME ":" FNR

        depth += opens - closes
        if (pending && opens > 0) { intest = 1; testdepth = depth - 1; pending = 0 }
        else if (intest && depth <= testdepth) { intest = 0 }
      }
    '
}

ALL_HITS="$(hits)"
HIT_FILES="$(printf '%s\n' "$ALL_HITS" | cut -d: -f1 | sort -u | grep -v '^$' || true)"

if [ "$mode" = "--list" ]; then
  printf '%s\n' "$HIT_FILES"
  exit 0
fi

[ -f "$ALLOWLIST" ] || {
  echo "✗ $ALLOWLIST is missing — the guard cannot tell pending work from drift." >&2
  exit 1
}

allowed="$(sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$ALLOWLIST" | grep -v '^$' || true)"
fail=0

offenders=""
while IFS= read -r hit; do
  [ -n "$hit" ] || continue
  file="${hit%%:*}"
  if ! printf '%s\n' "$allowed" | grep -Fxq "$file"; then
    offenders="${offenders}${hit}"$'\n'
  fi
done < <(printf '%s\n' "$ALL_HITS")

if [ -n "$offenders" ]; then
  echo "✗ hardcoded Postgres dialect fragments outside crates/nook-db:" >&2
  printf '%s' "$offenders" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'

  Two different situations, and the fix is different for each:

  IF YOU ARE SWEEPING THIS FILE — ask the engine instead of assuming it:
      nook_db::dialect::type_mapping(<pool>.engine()).now()      // cast, now
      nook_db::dialect::time_math(<pool>.engine()).now_plus(..)  // interval math
      nook_db::dialect::json(<pool>.engine()).get_text(..)       // jsonb/JSON1
  then remove this file from scripts/dialect-dispatch-allowlist.txt.

  IF YOU ARE NOT — this file emits engine-correct SQL today and should keep
  doing so. `Postgres.now()` returns the literal string "now()" whatever engine
  is underneath, which is a query that fails on SQLite rather than a query that
  is merely slow.
MSG
  fail=1
fi

stale=""
while IFS= read -r file; do
  [ -n "$file" ] || continue
  if [ ! -f "$file" ]; then
    stale="${stale}${file}  (no such file)"$'\n'
  elif ! printf '%s\n' "$HIT_FILES" | grep -Fxq "$file"; then
    stale="${stale}${file}"$'\n'
  fi
done < <(printf '%s\n' "$allowed")

if [ -n "$stale" ]; then
  echo "✗ stale entries in $ALLOWLIST — these files hardcode no dialect any more:" >&2
  printf '%s' "$stale" | sed 's/^/    /' >&2
  echo "  Delete those lines. An exemption that outlives its reason is how the list stops shrinking." >&2
  fail=1
fi

if [ "$fail" = "0" ]; then
  n="$(printf '%s\n' "$allowed" | grep -c . || true)"
  echo "✓ no hardcoded dialect outside crates/nook-db ($n file(s) still allow-listed, pending their sweep)"
fi
exit "$fail"
