#!/usr/bin/env bash
# MAIN-260 AC-5: the guard's own self-test, proving it BOTH ways.
#
# A guard that only fails is useless — it would stall every card in the
# repository chain on the day it lands. A guard that only passes catches
# nothing. So this asserts both halves, and the green-when-untouched half is
# the load-bearing one: it is the evidence that MAIN-250..258 can proceed.
#
# Everything happens in a scratch copy of the tree. Nothing under test is
# modified, so a failed run cannot leave a stray `.query_as(` in a route.
set -euo pipefail
cd "$(dirname "$0")/.."

GUARD="scripts/check-inline-sql.sh"
ALLOWLIST="scripts/inline-sql-allowlist.txt"
fail=0

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/scripts"
cp "$GUARD" "$ALLOWLIST" "$work/scripts/"
# Only the crate the guard scans; keeping the copy small keeps the test quick.
mkdir -p "$work/crates/nook-control"
cp -r crates/nook-control/src "$work/crates/nook-control/src"

check() { (cd "$work" && ./scripts/check-inline-sql.sh >/dev/null 2>&1); }

# ── (a) green when every allow-listed file is left untouched ────────────────
# The half that proves the guard does not block the epic it exists to protect.
if check; then
  echo "  ✓ green on an untouched tree (the pending aggregates are not blocked)"
else
  echo "✗ the guard is RED on an untouched tree — it would stall MAIN-250..258" >&2
  (cd "$work" && ./scripts/check-inline-sql.sh) >&2 || true
  fail=1
fi

# ── (b) red when SQL appears in a file that had none ────────────────────────
# `routes/boards.rs` is deliberately the victim: it is migrated (not
# allow-listed) AND it has production code below an inline `#[cfg(test)]`
# module, so a naive first-`#[cfg(test)]` split would call the new line
# test-only and pass. This is the AC-2 hazard, tested rather than asserted.
victim="$work/crates/nook-control/src/routes/boards.rs"
cp "$victim" "$victim.orig"
cat >> "$victim" <<'RS'

async fn drifted(db: &nook_db::DbPool) -> Result<i64, sqlx::Error> {
    db.query_scalar("SELECT count(*) FROM tasks", nook_db::params![]).await
}
RS
if check; then
  echo "✗ the guard stayed GREEN with fresh SQL appended to routes/boards.rs" >&2
  echo "  (that file has production code BELOW its test module — the AC-2 hazard)" >&2
  fail=1
else
  out="$( (cd "$work" && ./scripts/check-inline-sql.sh) 2>&1 || true)"
  echo "  ✓ red when SQL is added to a clean file"
  # AC-4: the message has to say WHICH of the two situations you are in, or the
  # message is itself what blocks you.
  for want in "routes/boards.rs" "IF YOU ARE MIGRATING" "IF YOU ARE NOT" "inline-sql-allowlist.txt"; do
    if ! printf '%s' "$out" | grep -qF "$want"; then
      echo "✗ the failure message never mentions '$want'" >&2
      fail=1
    fi
  done
  # …and it must name the line, not just the file.
  if ! printf '%s' "$out" | grep -qE 'routes/boards\.rs:[0-9]+'; then
    echo "✗ the failure message does not name a file:line" >&2
    fail=1
  fi
fi
mv "$victim.orig" "$victim"

# ── (c) a test-only query is NOT an offence (the chain's NG-4) ──────────────
# Tests keep raw DB access. If the guard flagged them it would pressure exactly
# the change the epic says not to make.
cat >> "$victim" <<'RS'

#[cfg(test)]
mod guard_selftest_tests {
    #[tokio::test]
    async fn a_test_may_hold_sql(db: &nook_db::DbPool) {
        let _ = db.query_scalar("SELECT 1", nook_db::params![]).await;
    }
}
RS
if check; then
  echo "  ✓ SQL inside #[cfg(test)] is ignored (the chain's NG-4 holds)"
else
  echo "✗ the guard flagged SQL inside a #[cfg(test)] module" >&2
  fail=1
fi

# ── (d) a stale allow-list entry fails ──────────────────────────────────────
# Without this the list never shrinks — a card migrates its files, forgets the
# removal, and the exemption outlives its reason.
printf '\ncrates/nook-control/src/routes/health.rs  # dup\ncrates/nook-control/src/repo/mod.rs  # holds no SQL\n' \
  >> "$work/scripts/inline-sql-allowlist.txt"
if check; then
  echo "✗ the guard accepted an allow-list entry for a file with no SQL" >&2
  fail=1
else
  echo "  ✓ red on a stale allow-list entry (the list can only shrink)"
fi

[ "$fail" = "0" ] && echo "✓ inline-SQL guard: blocks drift, ignores tests, does not stall the epic"
exit "$fail"
