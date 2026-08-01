#!/usr/bin/env bash
# MAIN-354: a Postgres literal may not ride INSIDE an argument to a dialect seam.
#
# The sweep cards' own AC-1 greps look for the `Postgres.<x>()` HANDLE. That
# finds the outer call and misses a literal smuggled in as an argument:
#
#     Postgres.now_minus_scaled("$1::bigint", "1 second")
#
# Swap the handle for `time_math(engine)` and the grep goes quiet while
# `$1::bigint` is still in the string — Postgres SQL, now shipped THROUGH the
# seam that exists to prevent it. MAIN-350 did exactly this; sweep B's grep
# caught it by luck of how that one was written, and nothing on main stopped it
# recurring.
#
# WHY THIS RUNS ON EVERY PR AND NOT ONLY THE SQLITE LEG (AC-2): the defect is
# invisible to Postgres by construction. `$1::bigint` is valid there, so the
# Postgres leg passes, and the SQLite leg is a whole engine-run later — or, for
# a binary still on the exclusion list, never. A source check is the only place
# this is cheap to catch.
set -euo pipefail
cd "$(dirname "$0")/.."

ALLOWLIST="scripts/nested-dialect-allowlist.txt"

# `--list` prints the offending sites and exits; used to (re)generate the
# allow-list, and by the self-test.
mode="${1:-check}"

# The seam methods that take a composed SQL fragment as a string. Anything a
# caller can hand a literal to; the ones that take only static type names
# (`uuid_column`, `now`) cannot carry one and are not here.
SEAM_METHODS='now_plus|now_minus|now_plus_scaled|now_minus_scaled|cast|greatest|ci_match|get_text|get_json|contains|array_elements|set'

# What counts as a Postgres literal INSIDE such an argument.
#
# The `::` half is deliberately NOT bare. Rust paths are full of `::` —
# `body.contains("notify::raise")`, `src.contains("Permission::CaRotate")` — and
# `contains` is both a seam method and `str::contains`, so a bare `::` pattern
# reports four false positives on main today and would train a reader to ignore
# it. A CAST is `$N::type` or `something::<sql type>`; a Rust path is neither,
# because SQL type names are a closed set and `$N` never appears in one.
SQL_TYPES='uuid|jsonb|json|text|bigint|integer|int|int4|int8|smallint|bool|boolean|timestamptz|timestamp|date|numeric|real|double precision|bytea'
BAD_LITERAL="(\\\$[0-9]+::|::($SQL_TYPES)\\b|\\bnow\\(\\)|gen_random_uuid|interval ')"

# The seam's own home is out of scope, not allow-listed.
#
# `crates/nook-db/src/dialect.rs` is where Postgres SQL is the job: the Postgres
# arm returns those exact fragments, and its unit tests assert them verbatim —
# `pg.now_minus_scaled("$1::bigint", "1 second")` is that arm being TESTED, not
# a caller leaking. Listing it would put a permanent entry on a list whose whole
# contract is that it shrinks (AC-3), so it is scope instead. NG-3 keeps this
# card out of the seams regardless.
hits() {
  grep -rnE "($SEAM_METHODS)\(\s*&?\"[^\"]*$BAD_LITERAL" \
    --include='*.rs' crates/ 2>/dev/null |
    grep -v '^crates/nook-db/src/dialect.rs:' |
    sort || true
}

ALL_HITS="$(hits)"
HIT_FILES="$(printf '%s\n' "$ALL_HITS" | sed 's/:.*//' | sort -u | grep -v '^$' || true)"

if [ "$mode" = "--list" ]; then
  printf '%s\n' "$HIT_FILES" | grep -v '^$' || true
  exit 0
fi

# Allow-list entries are `path` or `path  # reason`; blank lines and full-line
# comments are ignored. Per FILE, and every line names the card that owns it —
# the same contract as the sibling guards, so a list entry is a debt with a
# creditor rather than a permanent excuse.
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
  echo "✗ a Postgres literal is nested inside a dialect seam call:" >&2
  printf '%s' "$offenders" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'

  The seam is being handed Postgres SQL, so swapping the handle changed
  nothing: the fragment still only runs on Postgres, and the Postgres leg
  cannot see it.

  The argument has to go through a seam too. The cast is the usual one:

      time_math(engine).now_minus_scaled("$1::bigint", "1 second")            ✗
      time_math(engine).now_minus_scaled(                                     ✓
          &type_mapping(engine).cast("$1", "bigint"), "1 second")

  Same shape for the others — `now()` is `type_mapping(engine).now()`,
  `gen_random_uuid()` belongs in the migration's DEFAULT rather than a query,
  and `interval '…'` is what `now_plus`/`now_minus` already spell for you.

  If a site genuinely cannot be converted yet, add its FILE to
  scripts/nested-dialect-allowlist.txt with the card that owns the fix.
MSG
  fail=1
fi

# A stale allow-list entry. Without this the list never shrinks: a card can
# convert its sites and forget the removal, and the exemption outlives its
# reason.
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
  echo "✗ stale entries in $ALLOWLIST — these files nest no dialect literal any more:" >&2
  printf '%s' "$stale" | sed 's/^/    /' >&2
  echo "  Delete those lines. An exemption that outlives its reason is how the list stops shrinking." >&2
  fail=1
fi

if [ "$fail" = "0" ]; then
  n="$(printf '%s\n' "$allowed" | grep -c . || true)"
  echo "✓ no dialect literal nested in a seam call ($n file(s) allow-listed, pending their card)"
fi
exit "$fail"
