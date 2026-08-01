#!/usr/bin/env bash
# MAIN-260: no NEW inline SQL outside crates/nook-control/src/repo/.
#
# The repository chain (MAIN-245..258) is moving every query behind an
# intent-named trait. The value of that is discoverability — an agent should
# find `pick_tasks()` rather than compose a fresh filter query — and it only
# holds if new SQL cannot quietly appear. Nothing else stops it; drift would be
# invisible until somebody re-audited by hand.
#
# THE RULE IS "do not add SQL to a file that had none", not "no SQL anywhere".
# Aggregates that have not been migrated yet legitimately still hold their
# queries, so every such file is named in the allow-list beside this script with
# a reason. The list shrinks as each card lands; it must never grow silently.
#
# Scope is nook-control only. nook-chat, nook-worker, seed.rs and the testkit
# legitimately hold SQL and are out of scope (MAIN-260 NG-1).
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="crates/nook-control/src"
ALLOWLIST="scripts/inline-sql-allowlist.txt"

# `--list` prints the offending files and exits; used to (re)generate the
# allow-list, and by the self-test.
mode="${1:-check}"

# Files whose production code holds a query call, one `path:line` per hit.
#
# `#[cfg(test)]` modules are skipped by tracking BRACES to find where each one
# ends — not by splitting at the first `#[cfg(test)]`. That naive form is not a
# theoretical concern: it misreported 7 production sites in routes/boards.rs as
# test-only, and the mistake survived a PR review because the wrong number was
# presented as evidence (MAIN-249). boards.rs has production functions BELOW an
# inline test module, which is all it takes.
#
# The `$0`/`$1` inside the awk program are awk's fields, not shell parameters —
# single quotes are deliberate so the shell leaves the program alone.
# shellcheck disable=SC2016
hits() {
  find "$ROOT" -name '*.rs' -not -path "$ROOT/repo/*" -print0 |
    sort -z |
    xargs -0 awk '
      FNR == 1 { depth = 0; intest = 0; pending = 0 }

      {
        line = $0
        # Count braces without consuming $0.
        opens = gsub(/[{]/, "{", line)
        line = $0
        closes = gsub(/[}]/, "}", line)

        if (!intest && $0 ~ /^[ \t]*#\[cfg\(test\)\]/) pending = 1

        # Classify this line before moving depth, so the `mod tests {` line
        # itself is still outside the module.
        # Match the method name followed by `(` OR `::<`, rather than trying to
        # spell the turbofish. Two earlier attempts each missed a real shape:
        # demanding `(` straight after the name walked past
        # `.query_scalar_opt::<String>(` (MAIN-250, two sites in
        # routes/invites.rs), and `::<[^(]*>` then walked past
        # `.query_opt::<(TenantId, TaskId)>(` — a TUPLE turbofish, whose parens
        # the character class excluded (MAIN-255, two live sites in
        # services/jobs.rs). Not spelling the type at all cannot have a third
        # hole of that shape. Both were found by cross-checking two independent
        # counters — a guard nobody checks is just a comment.
        if (!intest && $0 ~ /\.(query_[a-z_]*|exec)(\(|::<)/)
          print FILENAME ":" FNR

        depth += opens - closes

        if (pending && opens > 0) { intest = 1; testdepth = depth - 1; pending = 0 }
        else if (intest && depth <= testdepth) { intest = 0 }
      }
    '
}

# Scan ONCE. Calling `hits` per allow-list entry re-ran the whole walk 40+
# times and, worse, `grep -q` closed the pipe early — awk died on SIGPIPE and
# the truncated output made every file look stale.
ALL_HITS="$(hits)"
HIT_FILES="$(printf '%s\n' "$ALL_HITS" | cut -d: -f1 | sort -u | grep -v '^$' || true)"

if [ "$mode" = "--list" ]; then
  printf '%s\n' "$HIT_FILES"
  exit 0
fi

[ -f "$ALLOWLIST" ] || {
  echo "✗ $ALLOWLIST is missing — the guard cannot tell pending aggregates from drift." >&2
  exit 1
}

# Allow-list entries are `path` or `path  # reason`; blank lines and full-line
# comments are ignored. Per FILE, never per directory: a directory exemption
# would let an already-migrated file regress silently because a sibling is
# still pending.
allowed="$(sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$ALLOWLIST" | grep -v '^$' || true)"

fail=0

# (1) SQL in a file nobody said could have it.
offenders=""
while IFS= read -r hit; do
  [ -n "$hit" ] || continue
  file="${hit%%:*}"
  if ! printf '%s\n' "$allowed" | grep -Fxq "$file"; then
    offenders="${offenders}${hit}"$'\n'
  fi
done < <(printf '%s\n' "$ALL_HITS")

if [ -n "$offenders" ]; then
  echo "✗ inline SQL outside crates/nook-control/src/repo/:" >&2
  printf '%s' "$offenders" | sed 's/^/    /' >&2
  cat >&2 <<'MSG'

  Two different situations, and the fix is different for each:

  IF YOU ARE MIGRATING THIS AGGREGATE — add a method to its repository trait in
  crates/nook-control/src/repo/, move the query into the impl, then remove this
  file from scripts/inline-sql-allowlist.txt.

  IF YOU ARE NOT — this file is not yours to add SQL to. Use an existing
  repository method, or add one to the aggregate that owns the data. Queries
  live behind a trait so the next reader finds a named intent instead of
  composing a new query beside it.
MSG
  fail=1
fi

# (2) A stale allow-list entry. Without this the list never shrinks: a card can
# migrate its files and forget the removal, and the exemption outlives the
# reason for it.
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
  echo "✗ stale entries in $ALLOWLIST — these files hold no inline SQL any more:" >&2
  printf '%s' "$stale" | sed 's/^/    /' >&2
  echo "  Delete those lines. An exemption that outlives its reason is how the list stops shrinking." >&2
  fail=1
fi

if [ "$fail" = "0" ]; then
  n="$(printf '%s\n' "$allowed" | grep -c . || true)"
  echo "✓ no inline SQL outside repo/ ($n file(s) allow-listed as permanent exemptions)"
fi
exit "$fail"
