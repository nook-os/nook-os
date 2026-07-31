#!/usr/bin/env bash
# Does the sqlx-signature guard actually catch anything? (MAIN-268 AC-4)
#
# A guard nobody checks is a comment. MAIN-260's inline-SQL guard shipped with
# two regex holes that a green run hid for weeks — both found only by
# cross-checking counters — so this asserts the guard in BOTH directions:
# it goes red on a re-added sqlx type, and red on an exemption that has outlived
# its reason.
set -euo pipefail
cd "$(dirname "$0")/.."

GUARD=scripts/check-sqlx-signatures.sh
ALLOWLIST=scripts/sqlx-signature-allowlist.txt
fail=0

# Everything is done on copies and restored, so a failed assertion cannot leave
# the tree modified.
restore() {
  [ -n "${PROBE:-}" ] && rm -f "$PROBE"
  [ -f "$ALLOWLIST.bak" ] && mv "$ALLOWLIST.bak" "$ALLOWLIST"
  return 0
}
trap restore EXIT

# ── green as it stands ──────────────────────────────────────────────────────
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ green on the tree as committed"
else
  echo "  ✗ the guard is RED on an unmodified tree — the allow-list is out of date" >&2
  fail=1
fi

# ── red on a re-added sqlx type, in each shape that matters ─────────────────
#
# A file NOT in the allow-list, so a hit in it is drift by definition. Written
# into a real crate because the guard walks `crates/`.
PROBE=crates/nook-control/src/__sqlx_guard_probe.rs
for shape in \
  'pub fn f(p: &sqlx::PgPool) {}' \
  'pub fn f(c: &PgConnection) {}' \
  'pub fn f() -> Result<(), sqlx::Error> { Ok(()) }' \
  'use sqlx::Row;' \
  'pub fn f(o: PgPoolOptions) {}'
do
  printf '%s\n' "$shape" > "$PROBE"
  if ./$GUARD >/dev/null 2>&1; then
    echo "  ✗ NOT caught: $shape" >&2
    fail=1
  else
    echo "  ✓ red on $shape"
  fi
  rm -f "$PROBE"
done

# ── a mention inside a comment is not a signature ───────────────────────────
#
# The guard strips `//` before matching. Without that, every doc comment
# explaining why a file does NOT use sqlx would trip it — which is exactly the
# comment the conversion leaves behind.
printf '%s\n' '// This deliberately does not use sqlx::Error or PgPool.' > "$PROBE"
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ a comment mentioning sqlx::Error is not a hit"
else
  echo "  ✗ a comment tripped the guard — every 'why we do not use sqlx' note would" >&2
  fail=1
fi
rm -f "$PROBE"

# ── #[cfg(test)] is skipped, and only to its closing brace ─────────────────
#
# The MAIN-249 lesson: a naive split at the first `#[cfg(test)]` marks every
# later line test-only. Production code BELOW an inline test module is all it
# takes, so the probe puts the real hit there.
cat > "$PROBE" <<'PROBE_EOF'
#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    fn helper(_p: &PgPool) {}
}
pub fn production(_p: &sqlx::PgPool) {}
PROBE_EOF
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✗ a production sqlx type BELOW a #[cfg(test)] module was missed — the MAIN-249 bug" >&2
  fail=1
else
  echo "  ✓ #[cfg(test)] is skipped by braces, not to end-of-file"
fi
rm -f "$PROBE"

# ── a BRACE-LESS #[cfg(test)] item does not blind the rest of the file ──────
#
# MAIN-345. `#[cfg(test)] mod testdb;` has no body, so the old "latch on the
# next line containing a `{`" rule kept waiting — and latched on an unrelated
# `use a::{B, C};` further down, with a test depth nothing ever falls back to.
# Everything after that point read as test code. In nook-chat/src/main.rs that
# hid a real `PgConnectOptions`, and the guard then reported the file as a STALE
# exemption while it plainly held a sqlx type: a wrong answer that reads as an
# instruction to delete a legitimate entry.
#
# The probe reproduces the shape exactly: brace-less item, then a `use` with
# braces, then the production hit.
cat > "$PROBE" <<'PROBE_EOF'
#[cfg(test)]
mod testdb;
mod ws;

use std::sync::Arc;
use axum::extract::{FromRequestParts, State};
use sqlx::postgres::PgConnectOptions;
PROBE_EOF
# Asserted against `--list` for THIS file, not against the guard's overall exit
# code. A guard that is red for some unrelated reason would make a coarse
# "is it red?" check pass while the bug is still there — which is exactly what
# happened when this case was first written.
if ./$GUARD --list | grep -qx "$PROBE"; then
  echo "  ✓ a brace-less #[cfg(test)] item does not swallow the rest of the file"
else
  echo "  ✗ a sqlx type after a BRACE-LESS #[cfg(test)] item was missed — the MAIN-345 bug" >&2
  fail=1
fi
rm -f "$PROBE"

# …and the attribute on a brace-less item still hides nothing it should hide:
# there is no body, so there is nothing to skip.
cat > "$PROBE" <<'PROBE_EOF'
#[cfg(test)]
mod testdb;
pub fn production(_p: &sqlx::PgPool) {}
PROBE_EOF
if ./$GUARD --list | grep -qx "$PROBE"; then
  echo "  ✓ nothing after a brace-less #[cfg(test)] item is treated as test code"
else
  echo "  ✗ production code directly after a brace-less #[cfg(test)] item was missed" >&2
  fail=1
fi
rm -f "$PROBE"

# …and the test module alone really is ignored (tests keep raw sqlx, NG-4).
cat > "$PROBE" <<'PROBE_EOF'
#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    fn helper(_p: &PgPool) {}
}
PROBE_EOF
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✓ sqlx inside #[cfg(test)] is ignored (the chain's NG-4 holds)"
else
  echo "  ✗ the guard flagged a #[cfg(test)] module — tests keep their raw sqlx" >&2
  fail=1
fi
rm -f "$PROBE"
PROBE=

# ── red on a stale allow-list entry ─────────────────────────────────────────
#
# This is what makes the list shrink. Without it a card converts its files,
# forgets the removal, and the exemption outlives its cause.
cp "$ALLOWLIST" "$ALLOWLIST.bak"
echo "crates/nook-control/src/state.rs  # a file with no sqlx type" >> "$ALLOWLIST"
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✗ a stale allow-list entry was NOT caught — the list can never shrink" >&2
  fail=1
else
  echo "  ✓ red on a stale allow-list entry (the list can only shrink)"
fi
mv "$ALLOWLIST.bak" "$ALLOWLIST"

# ── and an entry naming a file that no longer exists ────────────────────────
cp "$ALLOWLIST" "$ALLOWLIST.bak"
echo "crates/nook-control/src/gone.rs  # deleted long ago" >> "$ALLOWLIST"
if ./$GUARD >/dev/null 2>&1; then
  echo "  ✗ an entry for a deleted file was NOT caught" >&2
  fail=1
else
  echo "  ✓ red on an entry whose file is gone"
fi
mv "$ALLOWLIST.bak" "$ALLOWLIST"

if [ "$fail" = "0" ]; then
  echo "✓ sqlx-signature guard: blocks drift, ignores tests and comments, does not stall the epic"
fi
exit "$fail"
