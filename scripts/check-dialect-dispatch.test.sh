#!/usr/bin/env bash
# Does the dialect-dispatch guard actually catch anything? (MAIN-289 AC-2)
#
# A guard nobody checks is a comment. Asserted in both directions: red on a
# re-added hardcoded fragment, red on an exemption that has outlived its reason.
set -euo pipefail
cd "$(dirname "$0")/.."

GUARD=scripts/check-dialect-dispatch.sh
ALLOWLIST=scripts/dialect-dispatch-allowlist.txt
fail=0

restore() {
  [ -n "${PROBE:-}" ] && rm -f "$PROBE"
  [ -f "$ALLOWLIST.bak" ] && mv "$ALLOWLIST.bak" "$ALLOWLIST"
  return 0
}
trap restore EXIT

if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ green on the tree as committed"
else
  echo "  ✗ RED on an unmodified tree — the allow-list is out of date" >&2
  fail=1
fi

PROBE=crates/nook-control/src/__dialect_guard_probe.rs
# One per fragment family, because a guard that catches `now()` and misses
# `ci_match` is a guard that ships a SQLite failure.
for shape in \
  'fn f() -> String { Postgres.now().into() }' \
  'fn f() -> String { Postgres.cast("$1", "uuid") }' \
  'fn f() -> String { Postgres.ci_match("a", "b") }' \
  'fn f() -> String { Postgres.now_plus("1 day") }' \
  'fn f() -> String { Postgres.get_text("c", "k") }' \
  'fn f() -> &'"'"'static str { Postgres.claim_lock_clause() }'
do
  printf '%s\n' "$shape" > "$PROBE"
  if ./$GUARD >/dev/null 2>&1; then
    echo "  ✗ NOT caught: $shape" >&2
    fail=1
  else
    echo "  ✓ red on ${shape:0:46}…"
  fi
  rm -f "$PROBE"
done

# The dispatchers are the destination, not the offence.
printf '%s\n' 'fn f(p: &DbPool) -> String { type_mapping(p.engine()).now().into() }' > "$PROBE"
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ a dispatcher call is not a hit — that is the fix, not the fault"
else
  echo "  ✗ the guard flagged a DISPATCHER call — it would block its own remedy" >&2
  fail=1
fi
rm -f "$PROBE"

# A comment naming the arm is not a call.
printf '%s\n' '// Was Postgres.now(); now asks the engine.' > "$PROBE"
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ a comment mentioning Postgres.now() is not a hit"
else
  echo "  ✗ a comment tripped the guard — every sweep note would" >&2
  fail=1
fi
rm -f "$PROBE"

# Tests may pin the Postgres arm itself; production below them must still be seen.
cat > "$PROBE" <<'PROBE_EOF'
#[cfg(test)]
mod tests {
    #[test]
    fn pg_arm() { assert_eq!(Postgres.now(), "now()"); }
}
pub fn production() -> String { Postgres.cast("$1", "uuid") }
PROBE_EOF
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✗ production BELOW a #[cfg(test)] module was missed — the MAIN-249 bug" >&2
  fail=1
else
  echo "  ✓ #[cfg(test)] is skipped by braces, not to end-of-file"
fi
rm -f "$PROBE"

cat > "$PROBE" <<'PROBE_EOF'
#[cfg(test)]
mod tests {
    #[test]
    fn pg_arm() { assert_eq!(Postgres.now(), "now()"); }
}
PROBE_EOF
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ a test pinning the Postgres arm is ignored (that is what pins the arm)"
else
  echo "  ✗ flagged a #[cfg(test)] assertion about the Postgres arm" >&2
  fail=1
fi
rm -f "$PROBE"
PROBE=

cp "$ALLOWLIST" "$ALLOWLIST.bak"
echo "crates/nook-control/src/state.rs  # hardcodes nothing" >> "$ALLOWLIST"
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✗ a stale allow-list entry was NOT caught — the list can never shrink" >&2
  fail=1
else
  echo "  ✓ red on a stale allow-list entry (the list can only shrink)"
fi
mv "$ALLOWLIST.bak" "$ALLOWLIST"

if [ "$fail" = "0" ]; then
  echo "✓ dialect-dispatch guard: blocks drift, ignores tests and comments, permits the dispatchers"
fi
exit "$fail"
