#!/usr/bin/env bash
# MAIN-270 (epic AC-6): the SQLite leg's verdict, and the thing that makes the
# allow-list shrink.
#
# The suite does not pass on SQLite yet — every failure is owned by an upstream
# card (see scripts/sqlite-ci-allowlist.txt). None is a legitimately
# Postgres-specific test, so "exclude the Postgres-specific ones" was never a
# mechanism that could describe this. What CAN be described is the boundary:
# this much passes on SQLite today, that much does not, and the second set only
# ever gets smaller.
#
# So the leg is REQUIRED FROM DAY ONE over the covered set. `cargo test` exits
# non-zero on the SQLite leg by design — the excluded tests are expected to
# fail — which is exactly why the workflow ignores cargo's exit code and asks
# this script instead. Two questions, one run:
#
#   1. did every COVERED test pass?      a failure here is a real regression
#   2. did every EXCLUDED test fail?     a pass here means the list is stale
#
# (2) is what stops the list widening by neglect. Without it an upstream card
# could fix something, forget the allow-list line, and the exemption would
# outlive its reason — the leg would then be quietly protecting less than its
# line count claims.
#
# IDENTITY IS A TEST NOW, NOT A BINARY (MAIN-657). It used to be the deps
# basename, i.e. the file under `tests/` — which worked only while nook-control
# had 171 of them. That crate now builds ONE integration-test binary (`it`), so
# a whole-binary verdict there would answer "did anything in the crate fail",
# which is not a question worth asking: three pending failures would exclude
# 1160 tests and the leg would protect almost nothing.
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
       check-sqlite-ci.sh --list <cargo-test-log>   what FAILED (to seed the allow-list)
MSG
  exit 2
}

mode="check"
if [ "${1:-}" = "--list" ]; then mode="list"; shift; fi
LOG="${1:-}"
[ -n "$LOG" ] || usage
[ -f "$LOG" ] || { echo "✗ no such log: $LOG" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Outcomes, as `kind<TAB>name<TAB>status`, in two kinds:
#
#   N   every test BINARY the run reached. Only the emptiness and duplicate-name
#       checks read these; they are not what an allow-list entry names.
#   I   an IDENTITY an entry may name, with its verdict: `ok`, `FAILED`, or
#       `ignored`. A binary that reported individual tests contributes those
#       (`<binary>::<test path>`) and NOT itself — otherwise `it` would show up
#       as a failing identity nobody can sensibly exclude, since three pending
#       failures out of 1160 tests still fail the binary.
#
#       A binary that printed NO `test result:` line contributes itself as well,
#       and that is the crash case: 500 passing tests followed by an abort would
#       otherwise leave no failing identity at all and read as green. Excusing a
#       crash therefore takes a binary-scope entry, which is the honest grain
#       for it — nobody can say which test the process died in.
#
# The binary half of the name survives because it is still what makes an
# identity unique across the workspace: two crates may both have a `roundtrip`
# unit test.
#
# A crate with both a lib and a binary emits TWO unittest targets whose deps
# names are IDENTICAL (`nook_control` for `src/lib.rs` and again for
# `src/main.rs`), so the name alone is not an identity. Unit-test targets are
# therefore suffixed with their source: `nook_control:lib`, `nook_control:main`.
# This was not a hypothetical — the first real run of this script reported
# nook_control and nook_worker as duplicate names.
#
# Doc-test blocks emit their own `test result:` line and must NOT be attributed
# to the binary above them, so `Doc-tests` clears the current target. It also
# means doc-test `test … ok` lines are dropped, which is correct: they are not
# engine-dependent and nothing excludes them.
#
# A binary that dies before printing a result (a panic in a fixture, an abort)
# has no `test result:` line at all. That is recorded as FAILED rather than
# skipped — a crash is the one outcome that must never read as "covered and
# fine".
#
# CI sets CARGO_TERM_COLOR=always, so every line arrives wrapped in ANSI escapes
# and an anchored `^ *Running` never matches. That is not hypothetical: it is how
# this first ran in CI — the guard reported "no test binaries found" and failed
# the leg. It failing loudly is the only reason it was not silently decorative,
# which is exactly why "found nothing" is an error here and not an empty pass.
# Escapes are stripped before anything is matched, so the parse does not depend
# on whether the caller had a TTY.
#
# The `$0`/`$1` inside the awk program are awk's fields, not shell parameters.
# shellcheck disable=SC2016
outcomes() {
  awk '
    function close_binary() {
      if (cur == "") return
      if (!reported[cur]) { bins[++nb] = cur; status[cur] = "FAILED"; crashed[cur] = 1 }
      cur = ""
    }

    { gsub(/\033\[[0-9;]*[a-zA-Z]/, "") }

    /^[[:space:]]*Running / {
      close_binary()
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
    /^[[:space:]]*Doc-tests / { close_binary(); next }

    # `test <path> ... ok` / `... FAILED` / `... ignored[, reason]`. The
    # `test result:` summary is excluded by the `\.\.\.` this requires, and
    # `test <name> has been running for over 60 seconds` by the same.
    /^test .* \.\.\. / {
      if (cur == "") next
      rest = $0
      sub(/^test[[:space:]]+/, "", rest)
      name = rest; sub(/[[:space:]]+\.\.\.[[:space:]]+.*$/, "", name)
      verdict = rest; sub(/^.*[[:space:]]\.\.\.[[:space:]]+/, "", verdict)
      if (verdict ~ /^ok/)           v = "ok"
      else if (verdict ~ /^ignored/) v = "ignored"
      else                           v = "FAILED"
      ids[++ni] = cur "::" name; idstatus[ni] = v
      hastests[cur] = 1
      next
    }

    /^test result: / {
      if (cur == "") next
      bins[++nb] = cur
      status[cur] = ($0 ~ /^test result: ok\./) ? "ok" : "FAILED"
      reported[cur] = 1
      cur = ""
      next
    }

    END {
      close_binary()
      for (i = 1; i <= nb; i++) {
        b = bins[i]
        print "N\t" b "\t" status[b]
        if (!hastests[b] || crashed[b]) print "I\t" b "\t" status[b]
      }
      for (i = 1; i <= ni; i++) print "I\t" ids[i] "\t" idstatus[i]
    }
  ' "$LOG"
}

ALL="$(outcomes)"
BINARIES="$(printf '%s\n' "$ALL" | awk -F'\t' '$1 == "N" { print $2 }')"
[ -n "$BINARIES" ] || {
  echo "✗ no test binaries found in $LOG — the run did not get as far as testing." >&2
  echo "  A leg that tested nothing must not report success." >&2
  exit 1
}

# Ambiguous identity would silently merge two binaries' verdicts, so it is a
# hard error rather than a note. Names are unique across the workspace today;
# this is what keeps that true if someone adds tests/foo.rs to a second crate.
dupes="$(printf '%s\n' "$BINARIES" | sort | uniq -d || true)"
if [ -n "$dupes" ]; then
  echo "✗ two test binaries share a name — every identity under one would be ambiguous:" >&2
  printf '%s\n' "$dupes" | sed 's/^/    /' >&2
  echo "  Rename one of the test targets. Binary names prefix every identity here." >&2
  exit 1
fi

printf '%s\n' "$ALL" | awk -F'\t' '$1 == "I" { print $2 "\t" $3 }' > "$TMP/ids"
awk -F'\t' '$2 == "FAILED" { print $1 }' "$TMP/ids" | sort > "$TMP/failed"

if [ "$mode" = "list" ]; then
  cat "$TMP/failed"
  exit 0
fi

[ -f "$ALLOWLIST" ] || {
  echo "✗ $ALLOWLIST is missing — the guard cannot tell pending work from a regression." >&2
  exit 1
}

# Entries are `identity` or `identity  # reason (CARD)`; blanks and full-line
# comments ignored.
strip() { sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$1" | grep -v '^$' || true; }
pending="$(strip "$ALLOWLIST")"
engine_only=""
[ -f "$ENGINE_ONLY" ] && engine_only="$(strip "$ENGINE_ONLY")"
printf '%s\n%s\n' "$pending" "$engine_only" | grep -v '^$' > "$TMP/excluded" || true

# An entry names a SCOPE, and covers every identity at or under it: a single
# test (`it::job_reaper::a_thing`), the module it lives in (`it::multi_instance`),
# or a whole binary (`nook_db:lib`). The binary form is what every entry used to
# be, so nothing about the old lines changed meaning — this only adds the two
# finer grains that MAIN-657 made necessary.
#
# shellcheck disable=SC2016
awk -F'\t' -v ents="$TMP/excluded" '
  BEGIN { while ((getline l < ents) > 0) if (l != "") e[++n] = l }
  { for (i = 1; i <= n; i++)
      if ($1 == e[i] || index($1, e[i] "::") == 1) print e[i] "\t" $1 "\t" $2 }
' "$TMP/ids" > "$TMP/coverage"

fail=0

# (1) Something COVERED failed. This is the leg doing its job: a SQLite-only
# regression in territory we claim to cover.
regressions="$(
  awk -F'\t' -v cov="$TMP/coverage" '
    BEGIN { while ((getline l < cov) > 0) { split(l, f, "\t"); excused[f[2]] = 1 } }
    $0 != "" && !excused[$0]
  ' "$TMP/failed"
)"

if [ -n "$regressions" ]; then
  echo "✗ SQLite regression — these are covered by the SQLite leg and failed:" >&2
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
# all (a renamed or deleted test). An entry covering only `ignored` tests is
# neither — the run rendered no verdict on it, so neither should this.
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
