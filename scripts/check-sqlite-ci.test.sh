#!/usr/bin/env bash
# Does the SQLite-leg guard actually catch anything? (MAIN-270 AC-2)
#
# This guard is the only thing standing between "the SQLite leg is required" and
# "the SQLite leg is decorative", and it is asserted in BOTH directions, because
# each direction fails differently and silently:
#
#   * blind to a covered test failing  → the leg is required but proves nothing
#   * blind to an excluded test passing → the allow-list never shrinks, and the
#     leg keeps claiming coverage it no longer needs to exclude
#
# The guard is a pure function of a nextest JUnit report, so every case here is
# a real run of the real script against a synthetic report — no mocking of its
# internals.
set -euo pipefail
cd "$(dirname "$0")/.."

GUARD=scripts/check-sqlite-ci.sh
TMP="$(mktemp -d)"
fail=0
trap 'rm -rf "$TMP"' EXIT

# A report in nextest's actual shape, from `binary|test|verdict` triples.
# quick-junit self-closes a <testcase> that passed, hangs a <failure> child on
# one that failed, and an <error> on one whose process died — nextest runs a
# process per test, so an abort is that test's failure and nobody else's.
report() {
  local out="$TMP/$1"; shift
  {
    printf '<?xml version="1.0" encoding="UTF-8"?>\n'
    printf '<testsuites name="nextest-run" tests="%d" failures="0" errors="0" time="1.0">\n' "$#"
    local spec bin test verdict
    for spec in "$@"; do
      IFS='|' read -r bin test verdict <<<"$spec"
      printf '    <testsuite name="%s" tests="1" disabled="0" errors="0" failures="0">\n' "$bin"
      case "$verdict" in
        ok)   printf '        <testcase name="%s" classname="%s" timestamp="2026-08-24T00:00:00.000+00:00" time="0.05" />\n' "$test" "$bin" ;;
        fail) printf '        <testcase name="%s" classname="%s" timestamp="2026-08-24T00:00:00.000+00:00" time="0.05">\n            <failure type="test failure">assertion failed: 1 &lt; 0</failure>\n        </testcase>\n' "$test" "$bin" ;;
        died) printf '        <testcase name="%s" classname="%s" timestamp="2026-08-24T00:00:00.000+00:00" time="0.05">\n            <error type="test failure">process aborted: SIGABRT</error>\n        </testcase>\n' "$test" "$bin" ;;
        skip) printf '        <testcase name="%s" classname="%s" timestamp="2026-08-24T00:00:00.000+00:00" time="0.05">\n            <skipped />\n        </testcase>\n' "$test" "$bin" ;;
        *)    echo "unknown verdict '$verdict'" >&2; exit 2 ;;
      esac
      printf '    </testsuite>\n'
    done
    printf '</testsuites>\n'
  } >"$out"
}

# One passing test and one failing one, in two different binaries.
run_report() {
  report run.xml \
    'nook-control::good_one|x|ok' \
    'nook-control::bad_one|y|fail' \
    'nook-control::bad_one|z|ok'
}
run_report

allow() { printf '%s\n' "$@" >"$TMP/allow.txt"; }
engine_only() { printf '%s\n' "$@" >"$TMP/engine.txt"; }
engine_only '# none'
# The guard cd's to the repo root, so reports are named absolutely.
guard() {
  SQLITE_CI_ALLOWLIST="$TMP/allow.txt" SQLITE_CI_ENGINE_ONLY="$TMP/engine.txt" \
    ./$GUARD "$TMP/run.xml"
}

check() { # name, expect(pass|fail)
  local name="$1" expect="$2" got
  if guard >"$TMP/out" 2>&1; then got=pass; else got=fail; fi
  if [ "$got" = "$expect" ]; then
    echo "  ✓ $name"
  else
    echo "  ✗ $name — expected $expect, got $got" >&2
    sed 's/^/      /' "$TMP/out" >&2
    fail=1
  fi
}

# ── the shape it is meant to accept ─────────────────────────────────────────
allow 'nook-control::bad_one::y  # bed.pool on a SQLite bed (MAIN-268)'
check "green when the only failure is allow-listed" pass

# ── an entry names a SCOPE and covers what is under it (MAIN-657) ──────────
# nook-control builds ONE integration binary, so a per-test-only rule would have
# no way to say "this whole module is Postgres-only" — `nook-control::it::
# multi_instance` is exactly that statement, and it is honest because every test
# under it is excused for the one reason.
allow 'nook-control::bad_one  # the whole binary'
check "an entry covers every test beneath it" pass

# The cost of that rule, stated as a test so nobody has to rediscover it: a
# coarse entry excuses the PASSING tests under it too. `bad_one::z` passes and
# `bad_one::y` fails, so the entry is not stale and `z` leaves the leg for free.
# Nothing mechanical can tell an honest module-wide exemption from a lazy one —
# what keeps entries at the finest grain that describes the failure is the
# convention in the lists' own headers, and review.
allow 'nook-control::bad_one::y  # only the failure (MAIN-268)'
check "a per-test entry leaves its passing siblings covered" pass

# ── direction 1: a COVERED test fails ───────────────────────────────────────
# The whole point of a required leg. Without this the job is decorative.
allow '# nothing excluded'
check "red when a covered test fails" fail
grep -q 'nook-control::bad_one::y' "$TMP/out" || { echo "  ✗ the regression is not named" >&2; fail=1; }

# ── direction 2: an EXCLUDED test passes (a stale entry) ────────────────────
# Without this the list never shrinks and the leg quietly over-claims.
allow 'nook-control::bad_one::y  # pending' 'nook-control::good_one::x  # pending'
check "red when an excluded test now passes" fail
grep -q 'nook-control::good_one::x.*passes now' "$TMP/out" || { echo "  ✗ the stale entry is not named" >&2; fail=1; }

# ── an allow-list entry naming nothing at all ───────────────────────────────
# A renamed or deleted test leaves an exemption behind that would never expire
# on its own.
allow 'nook-control::bad_one::y  # pending' 'nook-control::ghost::gone  # pending'
check "red when an excluded test no longer exists" fail
grep -q 'nook-control::ghost::gone.*nothing in this run matches it' "$TMP/out" \
  || { echo "  ✗ the ghost is not named" >&2; fail=1; }

# ── a MODULE-scope entry, and staleness held against it ─────────────────────
# The `it` binary's suites are modules inside one binary, so this is the form a
# whole-file exemption takes now. It must exclude the suite...
report run.xml \
  'nook-control::it|job_reaper::reaps|fail' \
  'nook-control::it|multi_instance::the_bus_carries|fail' \
  'nook-control::it|node_owner::an_owner_sees_it|ok'
allow 'nook-control::it::job_reaper::reaps  # (MAIN-564)'
engine_only 'nook-control::it::multi_instance  # pg_notify has no SQLite equivalent'
check "a module-scope entry excludes the suite under it" pass

# ...and a new failure in a COVERED suite of the same binary must still redden,
# which is the whole reason a whole-binary verdict is not good enough here.
report run.xml \
  'nook-control::it|job_reaper::reaps|fail' \
  'nook-control::it|multi_instance::the_bus_carries|fail' \
  'nook-control::it|node_owner::an_owner_sees_it|fail'
check "red when a covered test in the one binary fails" fail
grep -q 'nook-control::it::node_owner::an_owner_sees_it' "$TMP/out" \
  || { echo "  ✗ the covered test's failure is not named" >&2; fail=1; }
grep -q 'nook-control::it::job_reaper::reaps' "$TMP/out" \
  && { echo "  ✗ an EXCUSED test was reported as a regression" >&2; fail=1; }

# ...and a module entry expires like any other: nothing under it failing and
# something under it passing means the classification is over.
report run.xml \
  'nook-control::it|node_owner::an_owner_sees_it|ok' \
  'nook-control::it|node_owner::and_another|ok'
allow '# nothing pending'
engine_only 'nook-control::it::node_owner  # claimed Postgres-only'
check "red when a module-scope entry covers only passing tests" fail
grep -q 'nook-control::it::node_owner.*passes now' "$TMP/out" \
  || { echo "  ✗ the stale module entry is not named" >&2; fail=1; }
engine_only '# none'

# ── a SKIPPED test renders no verdict ───────────────────────────────────────
# So its entry is neither stale nor a ghost. Reading a skip as "fixed" would
# delete a line nobody has re-measured.
report run.xml \
  'nook-control::it|job_reaper::reaps|skip' \
  'nook-control::it|node_owner::an_owner_sees_it|ok'
allow 'nook-control::it::job_reaper::reaps  # (MAIN-564)'
check "a skipped test's entry is neither stale nor a ghost" pass

# ── a test whose PROCESS dies ───────────────────────────────────────────────
# nextest reports an abort as an <error>, not a <failure>. Reading that as "not
# failed" is how a crash would pass a required gate.
report run.xml \
  'nook-control::good_one|x|ok' \
  'nook-control::crasher|boom|died'
allow '# nothing excluded'
check "red when a test's process dies" fail
grep -q 'nook-control::crasher::boom' "$TMP/out" || { echo "  ✗ the crashed test is not named" >&2; fail=1; }

# ── two tests with the same key ─────────────────────────────────────────────
# Identity is `<binary-id>::<test-name>`; two of them would silently merge
# verdicts.
report run.xml \
  'nook-control::dup|same|ok' \
  'nook-control::dup|same|fail'
allow '# nothing excluded'
check "red on two tests sharing a key" fail
grep -q 'share a key' "$TMP/out" || { echo "  ✗ the ambiguity is not explained" >&2; fail=1; }

# ── a report that never got to testing ──────────────────────────────────────
# A run that died before the first test must not read as "nothing failed,
# therefore green".
printf '<?xml version="1.0" encoding="UTF-8"?>\n<testsuites name="nextest-run" tests="0" failures="0" errors="0" time="0.0">\n</testsuites>\n' >"$TMP/run.xml"
allow '# nothing excluded'
check "red when the report holds no tests at all" fail

# ── the parse does not depend on the writer's formatting ────────────────────
# The log parser this replaced was broken in CI by ANSI colour, because it
# matched anchored lines. Element records, not lines: a report written on ONE
# line must give the identical verdict.
run_report
tr -d '\n' <"$TMP/run.xml" >"$TMP/oneline.xml" && mv "$TMP/oneline.xml" "$TMP/run.xml"
allow 'nook-control::bad_one::y  # pending (MAIN-268)'
check "a report with no line breaks parses the same" pass

# ...and the formatting must not hide a regression either.
allow '# nothing excluded'
check "red on a one-line report when a covered test fails" fail
grep -q 'nook-control::bad_one::y' "$TMP/out" || { echo "  ✗ the regression is not named in a one-line report" >&2; fail=1; }

# ── the permanent engine-specific list excludes too ─────────────────────────
# A deliberately Postgres-only test must not read as a regression.
run_report
allow '# nothing pending'
engine_only 'nook-control::bad_one::y  # asserts Postgres behaviour on purpose'
check "a deliberately Postgres-only test is excluded, with no card" pass

# ...and it is still held to the staleness rule: passing on SQLite means the
# classification was wrong, not that the problem went away.
allow '# nothing pending'
engine_only 'nook-control::good_one::x  # claimed Postgres-only'
check "red when a supposedly Postgres-only test passes on SQLite" fail
grep -q 'nook-control::good_one::x.*passes now' "$TMP/out" || { echo "  ✗ the misclassification is not named" >&2; fail=1; }
engine_only '# none'

# ── --list seeds the allow-list ─────────────────────────────────────────────
run_report
listed="$(SQLITE_CI_ALLOWLIST="$TMP/allow.txt" ./$GUARD --list "$TMP/run.xml")"
if [ "$listed" = "nook-control::bad_one::y" ]; then
  echo "  ✓ --list prints exactly the failing tests"
else
  echo "  ✗ --list printed: $listed" >&2
  fail=1
fi

# ── the committed pending list is well-formed ───────────────────────────────
# Every line must name the upstream card it waits on, or the list stops being a
# work-list and becomes a place failures go to be forgotten. This is exactly why
# the permanent engine-specific exclusions live in their own file: they have no
# card by definition, and letting them in here would mean dropping this rule.
missing="$(sed -e 's/[[:space:]]*$//' scripts/sqlite-ci-allowlist.txt \
  | grep -v '^[[:space:]]*#' | grep -v '^$' \
  | grep -vE '#.*MAIN-[0-9]+' || true)"
if [ -z "$missing" ]; then
  echo "  ✓ every pending allow-list entry names the card it waits on"
else
  echo "  ✗ allow-list entries with no MAIN-NN reference:" >&2
  printf '%s\n' "$missing" | sed 's/^/      /' >&2
  fail=1
fi

# A static "is this a test key and not a binary id" check is deliberately NOT
# here. Nothing in an entry's SHAPE distinguishes `nook-control::job_reaper`
# from a test key, so such a check would pass exactly the entries most likely to
# be wrong while looking like it had ruled them out. The guard proper is what
# holds an over-wide entry to account: it either covers nothing in the run, or
# covers only passing tests, and both come back as stale above.

# ── the two lists must not overlap, at any scope ────────────────────────────
# The same test in both would be both "pending work" and "permanent", so
# deleting the pending line would silently change nothing.
#
# NESTING counts as overlap since MAIN-657, because an entry covers everything
# under it: `it::foo` in one file and `it::foo::a_test` in the other is the same
# trap wearing two names, and `comm` cannot see it.
entries() {
  sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$1" | grep -v '^$' | sort || true
}
entries scripts/sqlite-ci-allowlist.txt > "$TMP/pending.txt"
entries scripts/sqlite-ci-engine-specific.txt > "$TMP/permanent.txt"
both="$(awk -v other="$TMP/permanent.txt" '
  BEGIN { while ((getline l < other) > 0) if (l != "") o[++n] = l }
  { for (i = 1; i <= n; i++)
      if ($0 == o[i] || index($0, o[i] "::") == 1 || index(o[i], $0 "::") == 1)
        print $0 "  ↔  " o[i] }
' "$TMP/pending.txt")"
if [ -z "$both" ]; then
  echo "  ✓ nothing is both pending and permanently engine-specific"
else
  echo "  ✗ entries covering each other across BOTH lists:" >&2
  printf '%s\n' "$both" | sed 's/^/      /' >&2
  fail=1
fi

exit "$fail"
