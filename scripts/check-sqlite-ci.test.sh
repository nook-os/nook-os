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

# ── an exemption is exactly as wide as the failure (MAIN-656) ──────────────
# `bad_one::z` PASSES in the same binary as the failing `bad_one::y`. Under the
# old binary-keyed lists excluding `bad_one` took `z` out of the leg for free;
# test keys mean `z` stays covered, and this is the case that proves it.
allow 'nook-control::bad_one  # the whole binary, the way the lists used to read'
check "excluding the binary no longer excludes its failing test" fail
grep -q 'nook-control::bad_one::y' "$TMP/out" || { echo "  ✗ the failing test is not named" >&2; fail=1; }

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
grep -q 'nook-control::ghost::gone.*no such test' "$TMP/out" || { echo "  ✗ the ghost is not named" >&2; fail=1; }

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
# be wrong while looking like it had ruled them out. The guard proper catches
# every one of them for real — a binary-keyed line matches no test in the run
# and comes back "no such test", which is the case above.

# ── the two lists must not overlap ──────────────────────────────────────────
# The same test in both would be both "pending work" and "permanent", so
# deleting the pending line would silently change nothing.
both="$(comm -12 \
  <(sed -e 's/#.*//' -e 's/[[:space:]]*$//' scripts/sqlite-ci-allowlist.txt | grep -v '^$' | sort) \
  <(sed -e 's/#.*//' -e 's/[[:space:]]*$//' scripts/sqlite-ci-engine-specific.txt | grep -v '^$' | sort) || true)"
if [ -z "$both" ]; then
  echo "  ✓ no test is both pending and permanently engine-specific"
else
  echo "  ✗ tests in BOTH lists:" >&2
  printf '%s\n' "$both" | sed 's/^/      /' >&2
  fail=1
fi

exit "$fail"
