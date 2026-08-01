#!/usr/bin/env bash
# MAIN-354 AC-4: the guard's own self-test, proving it BOTH ways.
#
# "A guard nobody has seen fail is decorative" is the card's phrasing and it is
# the whole reason this file exists. The green-on-an-untouched-tree half matters
# just as much: a guard that fires on the tree as committed would have to be
# disabled on the day it lands, which is how `check-dialect-dispatch.sh` ended
# up excluded from CI.
#
# Everything happens in a scratch copy. Nothing under test is modified, so a
# failed run cannot leave an injected `$1::bigint` in a repository.
set -euo pipefail
cd "$(dirname "$0")/.."

GUARD="scripts/check-nested-dialect.sh"
ALLOWLIST="scripts/nested-dialect-allowlist.txt"
fail=0

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/scripts"
cp "$GUARD" "$ALLOWLIST" "$work/scripts/"
# The guard scans `crates/`; copy only the sources, which keeps this well under
# a second even though the tree is large.
mkdir -p "$work/crates"
for c in crates/*/; do
  [ -d "$c/src" ] || continue
  mkdir -p "$work/$c"
  cp -r "$c/src" "$work/$c/src"
done

check() { (cd "$work" && ./scripts/check-nested-dialect.sh >/dev/null 2>&1); }

# ── (a) green on the tree as committed ──────────────────────────────────────
if check; then
  echo "  ✓ green on an untouched tree (the guard can actually land)"
else
  echo "✗ the guard is RED on an untouched tree — it cannot be added to CI" >&2
  (cd "$work" && ./scripts/check-nested-dialect.sh) >&2 || true
  fail=1
fi

# ── (b) red on the exact shape MAIN-350 shipped ─────────────────────────────
# The card's own verification step, mechanised: a cast nested in a seam call.
victim="$work/crates/nook-control/src/repo/jobs.rs"
cp "$victim" "$victim.orig"
cat >> "$victim" <<'RS'

fn injected_by_the_self_test(engine: nook_db::Engine) -> String {
    nook_db::dialect::time_math(engine).now_minus_scaled("$1::bigint", "1 second")
}
RS
if check; then
  echo "✗ the guard MISSED a cast nested in a seam call — the defect it exists for" >&2
  fail=1
else
  echo "  ✓ red on now_minus_scaled(\"\$1::bigint\", …) — the MAIN-350 shape"
fi
mv "$victim.orig" "$victim"

# ── (c) red on the other three literal families ─────────────────────────────
# One assertion each, because they fail differently on SQLite and a guard that
# only knew about casts would have let the other three through.
for probe in 'cast("now()", "timestamptz")' \
  'get_text("gen_random_uuid()", "k")' \
  "now_plus(\"interval '7 days'\")"; do
  cat > "$work/crates/nook-control/src/probe.rs" <<RS
fn p(e: nook_db::Engine) -> String { nook_db::dialect::type_mapping(e).$probe }
RS
  if check; then
    echo "✗ the guard MISSED: $probe" >&2
    fail=1
  else
    echo "  ✓ red on $probe"
  fi
  rm -f "$work/crates/nook-control/src/probe.rs"
done

# ── (d) NOT red on a Rust path in a `str::contains` ─────────────────────────
# `contains` is both a seam method and `str::contains`, and the tree really does
# hold `body.contains("notify::raise")`. A bare `::` pattern reports four such
# lines, and a guard that cries wolf on the tree as committed is one people
# learn to ignore. This is the assertion that keeps the pattern honest.
cat > "$work/crates/nook-control/src/probe.rs" <<'RS'
fn p(body: &str) -> bool {
    body.contains("notify::raise") && body.contains("Permission::CaRotate")
}
RS
if check; then
  echo "  ✓ quiet on a Rust path inside str::contains (no false positive)"
else
  echo "✗ the guard fires on a Rust path — it would be noise, not a signal" >&2
  (cd "$work" && ./scripts/check-nested-dialect.sh) >&2 || true
  fail=1
fi
rm -f "$work/crates/nook-control/src/probe.rs"

# ── (e) an allow-listed file is tolerated, and a stale entry is not ─────────
# Both directions of the list's contract, since "the list can only shrink" is
# the only thing keeping it from becoming a permanent excuse.
cp "$victim" "$victim.orig"
cat >> "$victim" <<'RS'

fn injected_again(engine: nook_db::Engine) -> String {
    nook_db::dialect::time_math(engine).now_minus_scaled("$1::bigint", "1 second")
}
RS
echo "crates/nook-control/src/repo/jobs.rs  # self-test" >> "$work/$ALLOWLIST"
if check; then
  echo "  ✓ an allow-listed file is tolerated"
else
  echo "✗ the allow-list does not exempt its own entry" >&2
  fail=1
fi
# Restore the victim and leave the entry behind: that IS a stale entry, and the
# guard has to say so or the list would only ever grow.
mv "$victim.orig" "$victim"
if check; then
  echo "✗ a stale allow-list entry passed — the list would never shrink" >&2
  fail=1
else
  echo "  ✓ red on a stale allow-list entry"
fi

if [ "$fail" = "0" ]; then
  echo "✓ nested-dialect guard: fires on all four literal families, ignores Rust paths, list shrinks"
fi
exit "$fail"
