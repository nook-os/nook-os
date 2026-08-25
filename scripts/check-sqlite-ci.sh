#!/usr/bin/env bash
# MAIN-270 (epic AC-6): the SQLite leg's verdict, and the thing that makes the
# allow-list shrink.
#
# The suite does not pass on SQLite yet, and the tests that do not pass are each
# owned by an upstream card. None is a legitimately Postgres-specific test, so
# "exclude the Postgres-specific ones" was never a mechanism that could describe
# this. What CAN be described is the boundary: these tests pass on SQLite today,
# those do not, and the second set only ever gets smaller.
#
# So the leg is REQUIRED FROM DAY ONE over the covered set. The runner exits
# non-zero on the SQLite leg by design — the excluded tests are expected to fail
# — which is exactly why the workflow ignores its exit code and asks this script
# instead. Two questions, one run:
#
#   1. did every COVERED test pass?      a failure here is a real regression
#   2. did every EXCLUDED test fail?     a pass here means the list is stale
#
# (2) is what stops the list widening by neglect. Without it an upstream card
# could fix a test, forget the allow-list line, and the exemption would outlive
# its reason — the leg would then be quietly protecting less than its line count
# claims.
#
# TEST-KEYED, NOT BINARY-KEYED (MAIN-656). The lists used to name whole test
# BINARIES, because `cargo test` reports one pass/fail line per binary and a
# per-test verdict had to be scraped out of prose. nextest reports per test, so
# an exemption is now exactly as wide as the failure it describes: `job_reaper`
# excluded nine tests to excuse six, and the three that passed were excluded
# from the leg for free. A key is `<binary-id>::<test-name>` — nextest's own
# two-part identity, joined.
#
# AN ENTRY NAMES A SCOPE, not necessarily one test (MAIN-657). nook-control
# builds ONE integration binary (`it`), so `nook-control::it` alone would excuse
# every test in the crate; but a module inside it — `nook-control::it::
# multi_instance` — is an honest grain for a whole-file exemption, and staying
# per-test where the failures are per-test is what keeps the leg wide. So an
# entry covers itself plus everything under `::`, and the finest grain that
# describes the failure is the right one.
#
# IT READS nextest's JUNIT REPORT, not a scraped log. The report is nextest's
# own machine-readable output: .config/nextest.toml's `sqlite` profile writes
# it, and both CI and `./test.sh rust --sqlite` run that profile, which is what
# makes the two give the same verdict. A scraped log could be broken by a colour
# escape or a line-wrap — and was, on this guard's first CI run.
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
usage: check-sqlite-ci.sh <junit.xml>          verdict for the SQLite leg
       check-sqlite-ci.sh --list <junit.xml>   tests that FAILED (to seed the allow-list)

The report is nextest's JUnit output, written by .config/nextest.toml's
`sqlite` profile to <target>/nextest/sqlite/junit.xml.
MSG
  exit 2
}

mode="check"
if [ "${1:-}" = "--list" ]; then mode="list"; shift; fi
REPORT="${1:-}"
[ -n "$REPORT" ] || usage
[ -f "$REPORT" ] || { echo "✗ no such nextest report: $REPORT" >&2; exit 2; }

# Per-test outcome, as `binary-id::test-name<TAB>ok|FAILED|skipped`.
#
# `RS="<"` makes every XML element its own record, so the parse does not depend
# on how the writer indented or wrapped anything — and cannot be confused by the
# failure bodies, which carry a test's captured output with every `<` escaped.
#
# A `<testcase>` with a `<failure>` or `<error>` child FAILED; one that
# self-closes passed. nextest runs a process per test, so a test that panics,
# aborts or times out is a failure of that test alone — the whole "a binary died
# before printing a result" class the log parser had to reason about does not
# arise.
outcomes() {
  awk -v RS='<' '
    function attr(rec, key,   s) {
      s = rec
      if (s !~ "[[:space:]]" key "=\"") return ""
      sub(".*[[:space:]]" key "=\"", "", s)
      sub("\".*", "", s)
      return s
    }
    /^testsuite[[:space:]]/ { suite = attr($0, "name"); next }
    /^testcase[[:space:]]/ {
      name = attr($0, "name")
      # `classname` is the binary id too, and is present even when the report
      # nests differently. Prefer it; fall back to the enclosing suite.
      bin = attr($0, "classname"); if (bin == "") bin = suite
      cur = bin "::" name
      status = "ok"
      # `<testcase … />` — no children, so the verdict is already final.
      if ($0 ~ /\/>[[:space:]]*$/) { print cur "\t" status; cur = ""; }
      next
    }
    /^failure/ || /^error/ { if (cur != "") status = "FAILED"; next }
    /^skipped/             { if (cur != "") status = "skipped"; next }
    /^\/testcase/          { if (cur != "") { print cur "\t" status; cur = "" } next }
  ' "$REPORT"
}

ALL="$(outcomes)"
[ -n "$ALL" ] || {
  echo "✗ no tests found in $REPORT — the run did not get as far as testing." >&2
  echo "  A leg that tested nothing must not report success." >&2
  exit 1
}

# Ambiguous identity would silently merge two tests' verdicts, so it is a hard
# error rather than a note.
dupes="$(printf '%s\n' "$ALL" | cut -f1 | sort | uniq -d || true)"
if [ -n "$dupes" ]; then
  echo "✗ two tests share a key — the allow-list cannot name one without the other:" >&2
  printf '%s\n' "$dupes" | sed 's/^/    /' >&2
  echo "  Rename one of them. '<binary-id>::<test-name>' is this list's only identity." >&2
  exit 1
fi

FAILED="$(printf '%s\n' "$ALL" | awk -F'\t' '$2 == "FAILED" { print $1 }' | sort)"

if [ "$mode" = "list" ]; then
  printf '%s\n' "$FAILED"
  exit 0
fi

[ -f "$ALLOWLIST" ] || {
  echo "✗ $ALLOWLIST is missing — the guard cannot tell pending work from a regression." >&2
  exit 1
}

# Entries are `test` or `test  # reason (CARD)`; blanks and full-line comments
# ignored.
strip() { sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$1" | grep -v '^$' || true; }
pending="$(strip "$ALLOWLIST")"
engine_only=""
[ -f "$ENGINE_ONLY" ] && engine_only="$(strip "$ENGINE_ONLY")"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
printf '%s\n' "$ALL" > "$TMP/ids"
printf '%s\n%s\n' "$pending" "$engine_only" | grep -v '^$' > "$TMP/excluded" || true

# Which entry covers which test, as `entry<TAB>test<TAB>verdict`. An entry
# covers a test it names outright and every test beneath it under `::`.
#
# shellcheck disable=SC2016
awk -F'\t' -v ents="$TMP/excluded" '
  BEGIN { while ((getline l < ents) > 0) if (l != "") e[++n] = l }
  { for (i = 1; i <= n; i++)
      if ($1 == e[i] || index($1, e[i] "::") == 1) print e[i] "\t" $1 "\t" $2 }
' "$TMP/ids" > "$TMP/coverage"

fail=0

# (1) A covered test failed. This is the leg doing its job: a SQLite-only
# regression in territory we claim to cover.
regressions="$(
  awk -F'\t' -v cov="$TMP/coverage" '
    BEGIN { while ((getline l < cov) > 0) { split(l, f, "\t"); excused[f[2]] = 1 } }
    $0 != "" && !excused[$0]
  ' <<< "$FAILED"
)"

if [ -n "$regressions" ]; then
  echo "✗ SQLite regression — these tests are covered by the SQLite leg and failed:" >&2
  printf '%s\n' "$regressions" | sed 's/^/    /' >&2
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

# (2) An excluded entry no longer excuses anything. Without this the list never
# shrinks. Two shapes: everything it covers now PASSES, or it covers nothing at
# all (a renamed or deleted test). An entry covering only SKIPPED tests is
# neither — the run rendered no verdict on it, so neither does this.
stale="$(
  awk -F'\t' '
    { seen[$1] = 1; if ($3 == "FAILED") failed[$1] = 1; if ($3 == "ok") passed[$1] = 1 }
    END { for (e in seen) if (!failed[e] && passed[e]) print e "  (passes now)" }
  ' "$TMP/coverage" | sort
  awk -F'\t' -v cov="$TMP/coverage" '
    BEGIN { while ((getline l < cov) > 0) { split(l, f, "\t"); covers[f[1]] = 1 } }
    { if (!covers[$0]) print $0 "  (nothing in this run matches it)" }
  ' "$TMP/excluded"
)"

if [ -n "$stale" ]; then
  echo "✗ stale entries in $ALLOWLIST / $ENGINE_ONLY:" >&2
  printf '%s\n' "$stale" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'
  Delete those lines — deleting them is what grows the SQLite leg's coverage.
  An exemption that outlives its reason makes the leg protect less than its
  line count suggests.
MSG
  fail=1
fi

if [ "$fail" = "0" ]; then
  nc="$(awk -F'\t' -v cov="$TMP/coverage" '
    BEGIN { while ((getline l < cov) > 0) { split(l, f, "\t"); excused[f[2]] = 1 } }
    $2 == "ok" && !excused[$1] { n++ }
    END { print n + 0 }
  ' "$TMP/ids")"
  np="$(printf '%s\n' "$pending" | grep -c . || true)"
  neo="$(printf '%s\n' "$engine_only" | grep -c . || true)"
  echo "✓ SQLite leg green over $nc covered tests" \
       "($np entries excluded pending a card, $neo deliberately Postgres-only)"
fi
exit "$fail"
