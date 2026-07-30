#!/usr/bin/env bash
# MAIN-270 (epic AC-6): the SQLite leg's verdict, and the thing that makes the
# allow-list shrink.
#
# The suite does not pass on SQLite yet — 197 of 940 tests fail, every one of
# them owned by an upstream card (bed.pool → MAIN-268, dialect → MAIN-289, and
# the three gap cards). None is a legitimately Postgres-specific test, so
# "exclude the Postgres-specific ones" was never a mechanism that could describe
# this. What CAN be described is the boundary: these binaries pass on SQLite
# today, those do not, and the second set only ever gets smaller.
#
# So the leg is REQUIRED FROM DAY ONE over the covered set. `cargo test` exits
# non-zero on the SQLite leg by design — the excluded binaries are expected to
# fail — which is exactly why the workflow ignores cargo's exit code and asks
# this script instead. Two questions, one run:
#
#   1. did every COVERED binary pass?      a failure here is a real regression
#   2. did every EXCLUDED binary fail?     a pass here means the list is stale
#
# (2) is what stops the list widening by neglect. Without it an upstream card
# could fix a binary, forget the allow-list line, and the exemption would
# outlive its reason — the leg would then be quietly protecting less than its
# line count claims.
#
# This reads a captured `cargo test --workspace --no-fail-fast` log rather than
# running cargo itself, so the workflow keeps one run and this stays a pure
# function of its output (and the self-test can feed it synthetic logs).
set -euo pipefail
cd "$(dirname "$0")/.."

# TWO lists, because they mean opposite things and must not decay into each
# other (AC-2):
#
#   ALLOWLIST      pending work. Every line names the card that owns it, and the
#                  file shrinks to empty as those cards land. When it is empty
#                  the SQLite leg covers the whole suite.
#   ENGINE_ONLY    permanent. Tests that assert Postgres-specific behaviour and
#                  are SUPPOSED to be Postgres-only — nook-db's `pg_*` dialect
#                  tests execute against a real Postgres connection on purpose.
#                  These never get deleted, and lumping them in with pending
#                  work would make the pending list look like it can never
#                  reach zero.
#
# Both are checked for staleness: a "permanent" exclusion that starts passing on
# SQLite was misclassified, which is worth knowing too.
#
# Overridable only so the self-test can drive real runs of this script against
# synthetic lists. CI never sets them.
ALLOWLIST="${SQLITE_CI_ALLOWLIST:-scripts/sqlite-ci-allowlist.txt}"
ENGINE_ONLY="${SQLITE_CI_ENGINE_ONLY:-scripts/sqlite-ci-engine-specific.txt}"

usage() {
  cat >&2 <<'MSG'
usage: check-sqlite-ci.sh <cargo-test-log>          verdict for the SQLite leg
       check-sqlite-ci.sh --list <cargo-test-log>   binaries that FAILED (to seed the allow-list)
MSG
  exit 2
}

mode="check"
if [ "${1:-}" = "--list" ]; then mode="list"; shift; fi
LOG="${1:-}"
[ -n "$LOG" ] || usage
[ -f "$LOG" ] || { echo "✗ no such log: $LOG" >&2; exit 2; }

# Per-binary outcome, as `name<TAB>ok|FAILED`.
#
# Identity is the `deps/<name>-<hash>` basename for an integration test — the
# file name under tests/, unique across the workspace.
#
# A crate with both a lib and a binary emits TWO unittest targets whose deps
# names are IDENTICAL (`nook_control` for `src/lib.rs` and again for
# `src/main.rs`), so the name alone is not an identity. Unit-test targets are
# therefore suffixed with their source: `nook_control:lib`, `nook_control:main`.
# This was not a hypothetical — the first real run of this script reported
# nook_control and nook_worker as duplicate names.
#
# Doc-test blocks emit their own `test result:` line and must NOT be attributed
# to the binary above them, so `Doc-tests` clears the current target.
#
# A binary that dies before printing a result (a panic in a fixture, an abort)
# has no `test result:` line at all. That is recorded as FAILED rather than
# skipped — a crash is the one outcome that must never read as "covered and
# fine".
#
# The `$0`/`$1` inside the awk program are awk's fields, not shell parameters.
# shellcheck disable=SC2016
outcomes() {
  awk '
    /^[[:space:]]*Running / {
      if (cur != "" && !seen[cur]) { print cur "\tFAILED" }
      # .../deps/<name>-<hash>[.exe])
      line = $0
      sub(/.*\/deps\//, "", line)
      sub(/-[0-9a-f]+(\.exe)?\)?[[:space:]]*$/, "", line)
      cur = line
      # `Running unittests src/lib.rs (...)` — disambiguate a crate lib target
      # from that same crate binary target, which share a deps name.
      if ($0 ~ /Running[[:space:]]+unittests[[:space:]]/) {
        src = $0
        sub(/.*Running[[:space:]]+unittests[[:space:]]+/, "", src)
        sub(/[[:space:]]*\(.*$/, "", src)          # drop the (path) tail
        sub(/.*\//, "", src); sub(/\.rs$/, "", src) # src/lib.rs -> lib
        if (src != "") cur = cur ":" src
      }
      next
    }
    /^[[:space:]]*Doc-tests / {
      if (cur != "" && !seen[cur]) { print cur "\tFAILED" }
      cur = ""
      next
    }
    /^test result: / {
      if (cur == "") next
      status = ($0 ~ /^test result: ok\./) ? "ok" : "FAILED"
      print cur "\t" status
      seen[cur] = 1
      cur = ""
      next
    }
    END { if (cur != "" && !seen[cur]) print cur "\tFAILED" }
  ' "$LOG"
}

ALL="$(outcomes)"
[ -n "$ALL" ] || {
  echo "✗ no test binaries found in $LOG — the run did not get as far as testing." >&2
  echo "  A leg that tested nothing must not report success." >&2
  exit 1
}

# Ambiguous identity would silently merge two binaries' verdicts, so it is a
# hard error rather than a note. Names are unique across the workspace today;
# this is what keeps that true if someone adds tests/foo.rs to a second crate.
dupes="$(printf '%s\n' "$ALL" | cut -f1 | sort | uniq -d || true)"
if [ -n "$dupes" ]; then
  echo "✗ two test binaries share a name — the allow-list cannot name one without the other:" >&2
  printf '%s\n' "$dupes" | sed 's/^/    /' >&2
  echo "  Rename one of the test files. Binary names are this list's only identity." >&2
  exit 1
fi

FAILED="$(printf '%s\n' "$ALL" | awk -F'\t' '$2 == "FAILED" { print $1 }' | sort)"
PASSED="$(printf '%s\n' "$ALL" | awk -F'\t' '$2 == "ok"     { print $1 }' | sort)"

if [ "$mode" = "list" ]; then
  printf '%s\n' "$FAILED"
  exit 0
fi

[ -f "$ALLOWLIST" ] || {
  echo "✗ $ALLOWLIST is missing — the guard cannot tell pending work from a regression." >&2
  exit 1
}

# Entries are `binary` or `binary  # reason (CARD)`; blanks and full-line
# comments ignored.
strip() { sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$1" | grep -v '^$' || true; }
pending="$(strip "$ALLOWLIST")"
engine_only=""
[ -f "$ENGINE_ONLY" ] && engine_only="$(strip "$ENGINE_ONLY")"
excluded="$(printf '%s\n%s\n' "$pending" "$engine_only" | grep -v '^$' || true)"

fail=0

# (1) A covered binary failed. This is the leg doing its job: a SQLite-only
# regression in territory we claim to cover.
regressions=""
while IFS= read -r b; do
  [ -n "$b" ] || continue
  printf '%s\n' "$excluded" | grep -Fxq "$b" || regressions="${regressions}${b}"$'\n'
done < <(printf '%s\n' "$FAILED")

if [ -n "$regressions" ]; then
  echo "✗ SQLite regression — these binaries are covered by the SQLite leg and failed:" >&2
  printf '%s' "$regressions" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'

  Two different situations, and the fix is different for each:

  IF YOUR CHANGE BROKE THEM ON SQLITE — that is the point of this leg. The
  Postgres leg cannot see it: engine-specific SQL, a `now()` or `::` cast that
  reached production, a migration that only exists on one track.

  IF THEY WERE ALREADY BROKEN — they must not be here. Adding a line to
  scripts/sqlite-ci-allowlist.txt is only correct when an UPSTREAM card owns the
  failure; name that card on the line. The list is supposed to shrink.
MSG
  fail=1
fi

# (2) An excluded binary passed. Without this the list never shrinks.
stale=""
while IFS= read -r b; do
  [ -n "$b" ] || continue
  if printf '%s\n' "$PASSED" | grep -Fxq "$b"; then
    stale="${stale}${b}  (passes now)"$'\n'
  elif ! printf '%s\n' "$FAILED" | grep -Fxq "$b"; then
    stale="${stale}${b}  (no such test binary in this run)"$'\n'
  fi
done < <(printf '%s\n' "$excluded")

if [ -n "$stale" ]; then
  echo "✗ stale entries in $ALLOWLIST / $ENGINE_ONLY:" >&2
  printf '%s' "$stale" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'
  Delete those lines — deleting them is what grows the SQLite leg's coverage.
  An exemption that outlives its reason makes the leg protect less than its
  line count suggests.
MSG
  fail=1
fi

if [ "$fail" = "0" ]; then
  nc="$(printf '%s\n' "$PASSED" | grep -c . || true)"
  np="$(printf '%s\n' "$pending" | grep -c . || true)"
  neo="$(printf '%s\n' "$engine_only" | grep -c . || true)"
  echo "✓ SQLite leg green over $nc covered binaries" \
       "($np excluded pending a card, $neo deliberately Postgres-only)"
fi
exit "$fail"
