#!/usr/bin/env bash
# MAIN-268 (epic AC-5): no NEW sqlx types outside the adapter.
#
# `sqlx` in a type signature is what actually pins code to one engine. A
# repository can be perfectly clean while its callers still name `PgPool`,
# `sqlx::Row` or `sqlx::Error`, and every such mention is a compile-time
# dependency on Postgres. The de-sqlx chain removed them file by file; this is
# what stops them coming back, because nothing else would notice.
#
# THE RULE IS "do not add a sqlx type to a file that has none", not "no sqlx
# anywhere". Files still holding one legitimately — the ones whose card has not
# landed, and the ones that will never convert — are named in the allow-list
# beside this script, each with a reason. The list shrinks as cards land; it
# must never grow silently.
#
# nook-db is the sanctioned adapter and is out of scope entirely: sqlx is its
# whole job. So is the `sqlx` dependency itself — this bans the types from
# SIGNATURES, not the crate from the build.
set -euo pipefail
cd "$(dirname "$0")/.."

ALLOWLIST="scripts/sqlx-signature-allowlist.txt"

# `--list` prints the offending files and exits; used to (re)generate the
# allow-list, and by the self-test.
mode="${1:-check}"

# The concrete-Postgres forms. `sqlx::query*` is deliberately NOT here: an
# inline query is MAIN-260's guard's problem, and a file can hold one without
# naming a sqlx TYPE. What this catches is the pin: a pool, a connection, a row,
# an error, the driver generic.
PATTERN='PgPool|PgConnection|PgPoolOptions|PgConnectOptions|PgRow|sqlx::Row|sqlx::Error|sqlx::Postgres|sqlx::Sqlite|SqliteRow|sqlx::pool'

# Files whose PRODUCTION code names one, as `path:line` per hit.
#
# `#[cfg(test)]` modules are skipped by tracking BRACES to find where each one
# ends — not by splitting at the first `#[cfg(test)]`. That naive form is not a
# theoretical concern: it misreported 7 production sites in routes/boards.rs as
# test-only, and the wrong number survived a PR review because it was presented
# as evidence (MAIN-249). A file with production code BELOW an inline test
# module is all it takes.
#
# Test code keeps its raw sqlx by standing policy (the chain's NG-4), so a
# `#[cfg(test)]` mention is not drift. Integration tests under `tests/` are a
# different matter and ARE scanned — that is the surface the testkit conversion
# is burning down.
#
# The `$0`/`$1` inside the awk program are awk's fields, not shell parameters —
# single quotes are deliberate so the shell leaves the program alone.
# shellcheck disable=SC2016
hits() {
  find crates -name '*.rs' -not -path 'crates/nook-db/*' -print0 |
    sort -z |
    xargs -0 awk -v pat="$PATTERN" '
      FNR == 1 { depth = 0; intest = 0; pending = 0 }

      {
        line = $0
        opens = gsub(/[{]/, "{", line)
        line = $0
        closes = gsub(/[}]/, "}", line)

        if (!intest && $0 ~ /^[ \t]*#\[cfg\(test\)\]/) pending = 1

        # Classify BEFORE moving depth, so the `mod tests {` line itself is
        # still outside the module.
        #
        # A line whose only mention is inside a comment is not a signature.
        # Stripping `//` to end-of-line is crude but exact enough: a `//` inside
        # a string literal on a line that also names a sqlx type does not occur
        # in this tree, and the alternative — parsing Rust — is not worth it for
        # a guard.
        code = $0
        sub(/\/\/.*/, "", code)
        if (!intest && code ~ pat)
          print FILENAME ":" FNR

        depth += opens - closes

        if (pending && opens > 0) { intest = 1; testdepth = depth - 1; pending = 0 }
        else if (intest && depth <= testdepth) { intest = 0 }
      }
    '
}

# Scan ONCE. Calling `hits` per allow-list entry re-ran the whole walk dozens of
# times and, worse, `grep -q` closed the pipe early — awk died on SIGPIPE and the
# truncated output made every file look stale (MAIN-260's lesson, inherited).
ALL_HITS="$(hits)"
HIT_FILES="$(printf '%s\n' "$ALL_HITS" | cut -d: -f1 | sort -u | grep -v '^$' || true)"

if [ "$mode" = "--list" ]; then
  printf '%s\n' "$HIT_FILES"
  exit 0
fi

[ -f "$ALLOWLIST" ] || {
  echo "✗ $ALLOWLIST is missing — the guard cannot tell pending work from drift." >&2
  exit 1
}

# Entries are `path` or `path  # reason`; blank lines and full-line comments are
# ignored. Per FILE, never per directory: a directory exemption would let an
# already-converted file regress because a sibling has not been done yet.
allowed="$(sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$ALLOWLIST" | grep -v '^$' || true)"

fail=0

# (1) A sqlx type in a file nobody said could have one.
offenders=""
while IFS= read -r hit; do
  [ -n "$hit" ] || continue
  file="${hit%%:*}"
  if ! printf '%s\n' "$allowed" | grep -Fxq "$file"; then
    offenders="${offenders}${hit}"$'\n'
  fi
done < <(printf '%s\n' "$ALL_HITS")

if [ -n "$offenders" ]; then
  echo "✗ sqlx types outside the adapter (crates/nook-db):" >&2
  printf '%s' "$offenders" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'

  Two different situations, and the fix is different for each:

  IF YOU ARE CONVERTING THIS FILE — take the engine-agnostic type instead:
  `nook_db::DbPool` for a pool, `nook_db::DbError` for an error, and the `Db`
  trait's methods rather than `sqlx::query*`. Then remove this file from
  scripts/sqlx-signature-allowlist.txt.

  IF YOU ARE NOT — this file has no sqlx type today and should not gain one.
  Naming `PgPool` or `sqlx::Error` here pins it to Postgres at compile time,
  which is the thing the whole de-sqlx chain exists to remove.
MSG
  fail=1
fi

# (2) A stale allow-list entry. Without this the list never shrinks: a card can
# convert its files and forget the removal, and the exemption outlives its
# reason.
stale=""
while IFS= read -r file; do
  [ -n "$file" ] || continue
  if [ ! -f "$file" ]; then
    stale="${stale}${file}  (no such file)"$'\n'
  elif ! printf '%s\n' "$HIT_FILES" | grep -Fxq "$file"; then
    stale="${stale}${file}"$'\n'
  fi
done < <(printf '%s\n' "$allowed")

if [ -n "$stale" ]; then
  echo "✗ stale entries in $ALLOWLIST — these files name no sqlx type any more:" >&2
  printf '%s' "$stale" | sed 's/^/    /' >&2
  echo "  Delete those lines. An exemption that outlives its reason is how the list stops shrinking." >&2
  fail=1
fi

if [ "$fail" = "0" ]; then
  n="$(printf '%s\n' "$allowed" | grep -c . || true)"
  echo "✓ no sqlx types outside crates/nook-db ($n file(s) still allow-listed, pending their card)"
fi
exit "$fail"
