#!/usr/bin/env bash
# Does the SQLite-leg guard actually catch anything? (MAIN-270 AC-2)
#
# This guard is the only thing standing between "the SQLite leg is required" and
# "the SQLite leg is decorative", and it is asserted in BOTH directions, because
# each direction fails differently and silently:
#
#   * blind to a covered binary failing  → the leg is required but proves nothing
#   * blind to an excluded binary passing → the allow-list never shrinks, and the
#     leg keeps claiming coverage it no longer needs to exclude
#
# The guard is a pure function of a cargo log, so every case here is a real run
# of the real script against a synthetic log — no mocking of its internals.
set -euo pipefail
cd "$(dirname "$0")/.."

GUARD=scripts/check-sqlite-ci.sh
TMP="$(mktemp -d)"
fail=0
trap 'rm -rf "$TMP"' EXIT

# A log in cargo's actual shape: one passing binary, one failing one.
log() {
  cat > "$TMP/$1" <<'EOF'
   Compiling nook-control v0.4.22 (/app/crates/nook-control)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1m 02s
     Running unittests src/lib.rs (/app/target/debug/deps/nook_control-1a2b3c4d)

running 2 tests
test a ... ok
test b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s

     Running tests/good_one.rs (/app/target/debug/deps/good_one-aaaa1111)

running 1 test
test x ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s

     Running tests/bad_one.rs (/app/target/debug/deps/bad_one-bbbb2222)

running 2 tests
test y ... FAILED
test z ... ok

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
EOF
}
log run.txt

allow() { printf '%s\n' "$@" > "$TMP/allow.txt"; }
engine_only() { printf '%s\n' "$@" > "$TMP/engine.txt"; }
engine_only '# none'
# The guard cd's to the repo root, so logs are named absolutely.
guard() {
  SQLITE_CI_ALLOWLIST="$TMP/allow.txt" SQLITE_CI_ENGINE_ONLY="$TMP/engine.txt" \
    ./$GUARD "$TMP/run.txt"
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
allow 'bad_one  # bed.pool on a SQLite bed (MAIN-268)'
check "green when the only failure is allow-listed" pass

# ── direction 1: a COVERED binary fails ─────────────────────────────────────
# The whole point of a required leg. Without this the job is decorative.
allow '# nothing excluded'
check "red when a covered binary fails" fail
grep -q 'bad_one' "$TMP/out" || { echo "  ✗ the regression is not named" >&2; fail=1; }

# ── direction 2: an EXCLUDED binary passes (a stale entry) ──────────────────
# Without this the list never shrinks and the leg quietly over-claims.
allow 'bad_one  # pending' 'good_one  # pending'
check "red when an excluded binary now passes" fail
grep -q 'good_one.*passes now' "$TMP/out" || { echo "  ✗ the stale entry is not named" >&2; fail=1; }

# ── an allow-list entry naming nothing at all ───────────────────────────────
# A renamed or deleted test file leaves an exemption behind that would never
# expire on its own.
allow 'bad_one  # pending' 'ghost_binary  # pending'
check "red when an excluded binary no longer exists" fail
grep -q 'ghost_binary.*no such test binary' "$TMP/out" || { echo "  ✗ the ghost is not named" >&2; fail=1; }

# ── a binary that CRASHES before reporting ──────────────────────────────────
# A panic in a fixture prints no `test result:` line at all. Reading that as
# "not failed" is how a crash would pass a required gate.
cat > "$TMP/run.txt" <<'EOF'
     Running tests/good_one.rs (/app/target/debug/deps/good_one-aaaa1111)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s

     Running tests/crasher.rs (/app/target/debug/deps/crasher-cccc3333)

running 1 test
error: test failed, to rerun pass `-p nook-control --test crasher`
EOF
allow '# nothing excluded'
check "red when a binary dies before printing a result" fail
grep -q 'crasher' "$TMP/out" || { echo "  ✗ the crashed binary is not named" >&2; fail=1; }

# ── doc-tests must not be attributed to the binary above them ──────────────
# `Doc-tests` emits its own `test result:` line. Attributing it to the previous
# binary would let a failing binary inherit the doc-tests' "ok".
cat > "$TMP/run.txt" <<'EOF'
     Running tests/bad_one.rs (/app/target/debug/deps/bad_one-bbbb2222)

running 1 test
error: test failed, to rerun pass `-p nook-control --test bad_one`

   Doc-tests nook_control

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
EOF
allow '# nothing excluded'
check "red when a crashed binary is followed by passing doc-tests" fail

# ── two binaries with the same name ────────────────────────────────────────
# Identity is the binary name; two of them would silently merge verdicts.
cat > "$TMP/run.txt" <<'EOF'
     Running tests/dup.rs (/app/target/debug/deps/dup-1111aaaa)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
     Running tests/dup.rs (/app/target/debug/deps/dup-2222bbbb)
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
EOF
allow '# nothing excluded'
check "red on two test binaries sharing a name" fail
grep -q 'share a name' "$TMP/out" || { echo "  ✗ the ambiguity is not explained" >&2; fail=1; }

# ── a log that never got to testing ────────────────────────────────────────
# A compile failure must not read as "nothing failed, therefore green".
printf 'error[E0432]: unresolved import\n' > "$TMP/run.txt"
allow '# nothing excluded'
check "red when the run never reached any test" fail

# ── the permanent engine-specific list excludes too ────────────────────────
# A deliberately Postgres-only test must not read as a regression.
log run.txt
allow '# nothing pending'
engine_only 'bad_one  # asserts Postgres behaviour on purpose'
check "a deliberately Postgres-only binary is excluded, with no card" pass

# ...and it is still held to the staleness rule: passing on SQLite means the
# classification was wrong, not that the problem went away.
allow '# nothing pending'
engine_only 'good_one  # claimed Postgres-only'
check "red when a supposedly Postgres-only binary passes on SQLite" fail
grep -q 'good_one.*passes now' "$TMP/out" || { echo "  ✗ the misclassification is not named" >&2; fail=1; }
engine_only '# none'

# ── --list seeds the allow-list ────────────────────────────────────────────
log run.txt
listed="$(SQLITE_CI_ALLOWLIST="$TMP/allow.txt" ./$GUARD --list "$TMP/run.txt")"
if [ "$listed" = "bad_one" ]; then
  echo "  ✓ --list prints exactly the failing binaries"
else
  echo "  ✗ --list printed: $listed" >&2
  fail=1
fi

# ── the committed pending list is well-formed ──────────────────────────────
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

# ── the two lists must not overlap ─────────────────────────────────────────
# The same binary in both would be both "pending work" and "permanent", so
# deleting the pending line would silently change nothing.
both="$(comm -12 \
  <(sed -e 's/#.*//' -e 's/[[:space:]]*$//' scripts/sqlite-ci-allowlist.txt | grep -v '^$' | sort) \
  <(sed -e 's/#.*//' -e 's/[[:space:]]*$//' scripts/sqlite-ci-engine-specific.txt | grep -v '^$' | sort) || true)"
if [ -z "$both" ]; then
  echo "  ✓ no binary is both pending and permanently engine-specific"
else
  echo "  ✗ binaries in BOTH lists:" >&2
  printf '%s\n' "$both" | sed 's/^/      /' >&2
  fail=1
fi

exit "$fail"
