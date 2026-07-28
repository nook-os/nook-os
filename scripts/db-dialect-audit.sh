#!/usr/bin/env bash
#
# DB dialect audit (MAIN-192) — a re-runnable inventory of every Postgres-specific
# construct in the four database-touching crates, categorized by what it takes to
# port each to SQLite. This is the MAP for the SQLite/Postgres campaign (MAIN-189):
# it does NOT change any query (NG-1) — it only finds and classifies them.
#
#   Usage:   scripts/db-dialect-audit.sh            # prints the report to stdout
#   Baseline: scripts/db-dialect-audit.sh > docs/db-dialect-audit.md
#   Verify:  scripts/db-dialect-audit.sh | diff -u docs/db-dialect-audit.md -
#
# Output is DETERMINISTIC (fixed pattern order; findings sorted by category, label,
# path, then line) so re-runs diff cleanly against the committed baseline (AC-5).
#
# Method & caveats: this is a grep-based lexical scan of source + migrations, not a
# SQL parser. Pure comment lines (//, ///, //!, *, --) are dropped so prose about a
# construct is not counted as a use of it, but an inline trailing comment can still
# match. A few patterns are scoped to migrations only (uuid/timestamptz column
# types) because the bare token is the `uuid` crate path in Rust; SQL casts to those
# types are caught separately. Over-inclusion is deliberate — a sweep would rather
# see a false positive than miss a real one.
set -euo pipefail

# Byte-order collation, so `sort` (and the character classes in grep) order the
# report identically regardless of the caller's locale — a report generated under
# en_US.UTF-8 must byte-match one generated under C, or "re-runs diff cleanly"
# (AC-5) fails across machines. Fixed to C here rather than trusting the env.
export LC_ALL=C

cd "$(dirname "$0")/.."

# Scan roots: the four crates' source, plus the two migration sets that exist.
ROOTS=(
  crates/nook-control/src
  crates/nook-chat/src
  crates/nook-worker/src
  crates/nook-infra/src
  crates/nook-control/migrations
  crates/nook-chat/migrations
)

# Pattern table — one row per construct, fields separated by '~' (no regex here
# uses '~'). Columns: category | mechanism | label | ERE regex | scope(all|sql) | ci(1|0)
#   category: a=common-subset  b=mechanical  c=mechanism-trait  d=manual-design
#   mechanism: for category c, which of MAIN-189 item 3's four traits it belongs to.
#   ci: 1 = case-insensitive. The `::` cast pattern is case-SENSITIVE (ci=0): SQL
#       casts use lowercase type names, so this avoids matching Rust type paths
#       like `uuid::Uuid`, `axum::Json`, `serde_json::json`.
# Order here is the fixed report order (part of the determinism contract).
read -r -d '' PATTERNS <<'TABLE' || true
c~atomic-claim~FOR UPDATE SKIP LOCKED~skip[[:space:]]+locked~all~1
c~atomic-claim~advisory locks~pg_(try_)?advisory_(xact_)?lock~all~1
c~event-bus~LISTEN / NOTIFY / pg_notify~pg_notify|(^|[^_[:alnum:]])(listen|notify)([^_[:alnum:]]|$)~all~1
c~json~jsonb type & functions~jsonb~all~1
c~json~json extract/contain operators~(->>|#>>|#>|@>|<@|\?\||\?&|jsonb_|json_)~all~1
c~type-mapping~uuid / timestamptz column types~(^|[^_[:alnum:]])(uuid|timestamptz)([^_[:alnum:]]|$)~sql~1
b~mechanical~:: cast to a pg type~::[[:space:]]*(uuid|text|int4|int8|bigint|integer|smallint|boolean|bool|timestamptz|timestamp|date|numeric|bytea|inet|interval)([^_[:alnum:]]|$)~all~0
b~mechanical~ILIKE~(^|[^_[:alnum:]])ilike([^_[:alnum:]]|$)~all~1
b~mechanical~now() default/expr~(^|[^:._[:alnum:]])now\(\)~all~1
a~common-subset~ON CONFLICT upsert~on[[:space:]]+conflict~all~1
a~common-subset~RETURNING~(^|[^_[:alnum:]])returning([^_[:alnum:]]|$)~all~1
d~manual-design~array bind / array_agg / unnest~(array_agg|unnest)[[:space:]]*\(|(^|[^_[:alnum:]])any[[:space:]]*\([[:space:]]*\$[0-9]~all~1
d~manual-design~INTERVAL arithmetic~interval[[:space:]]+'~all~1
TABLE

TSV="$(mktemp)"
trap 'rm -f "$TSV"' EXIT

# Collect every match as a TSV record: category, mechanism, label, crate, path,
# line, trimmed content. Comment-only lines are dropped before recording.
while IFS='~' read -r cat mech label regex scope ci; do
  [ -n "${cat:-}" ] || continue
  targets=("${ROOTS[@]}")
  if [ "$scope" = "sql" ]; then
    targets=(crates/nook-control/migrations crates/nook-chat/migrations)
  fi
  # grep: recursive, line numbers, skip binaries, ERE; case-insensitive only when
  # the pattern asks for it (ci=1). `|| true` at the end so a construct with zero
  # hits does not trip `set -e` under pipefail.
  gflags=(-rnIE)
  [ "$ci" = "1" ] && gflags=(-rnIiE)
  grep "${gflags[@]}" -- "$regex" "${targets[@]}" 2>/dev/null \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|--|\*)' \
    | while IFS=: read -r path line content; do
        crate="$(printf '%s' "$path" | cut -d/ -f2)"
        trimmed="$(printf '%s' "$content" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//; s/`//g' | cut -c1-110)"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
          "$cat" "$mech" "$label" "$crate" "$path" "$line" "$trimmed"
      done || true
done <<< "$PATTERNS" | sort -t$'\t' -k1,1 -k3,3 -k5,5 -k6,6n > "$TSV"

total="$(wc -l < "$TSV" | tr -d ' ')"

cat <<'HEAD'
# Database dialect audit (MAIN-192)

> Generated by `scripts/db-dialect-audit.sh`. Re-run
> `scripts/db-dialect-audit.sh > docs/db-dialect-audit.md` to regenerate; the
> output is deterministic, so a clean tree means the map is current.

This is the inventory the SQLite/Postgres campaign (MAIN-189) needs before the
query sweep: every Postgres-specific construct in `nook-control`, `nook-chat`,
`nook-worker`, and `nook-infra` (source + migrations), classified by what porting
it costs. **Inventory only — no query was changed (NG-1).**

It is a grep-based lexical scan, not a SQL parser: pure comment lines are dropped,
but an inline trailing comment can still match, and over-inclusion is deliberate.
Treat a finding as "look here", not "proven bug".

## Categories

- **(a) common-subset** — valid in SQLite too (`ON CONFLICT … DO UPDATE`,
  `RETURNING`); ships as-is, no change.
- **(b) mechanical rewrite** — a local, semantics-preserving edit (`ILIKE` →
  `LIKE … COLLATE NOCASE`, `now()` → `CURRENT_TIMESTAMP`, `::type` cast → `CAST`).
- **(c) mechanism trait** — hide behind one of MAIN-189 item 3's four traits, not
  rewritten inline: **atomic-claim** (`SKIP LOCKED`, advisory locks), **event-bus**
  (`LISTEN`/`NOTIFY`), **json** (`jsonb` + its operators), **type-mapping**
  (`uuid`/`timestamptz` columns).
- **(d) manual design** — no common shape; needs a per-site decision (array binds
  / `array_agg`, `INTERVAL` arithmetic).
HEAD

echo
echo "## Summary"
echo
echo "_${total} findings across the four crates (a line with two constructs counts once per construct)._"
echo

# Table 1: category × crate counts, computed from the TSV.
CRATES="$(cut -f4 "$TSV" | sort -u)"
{
  printf '| category |'
  for c in $CRATES; do printf ' %s |' "$c"; done
  printf ' **total** |\n'
  printf '|---|'
  for _ in $CRATES; do printf '%s' '---|'; done
  printf '%s\n' '---|'
  for cat in a b c d; do
    label="$(case "$cat" in
      a) echo "(a) common-subset";; b) echo "(b) mechanical";;
      c) echo "(c) mechanism trait";; d) echo "(d) manual design";; esac)"
    printf '| %s |' "$label"
    rowtotal=0
    for c in $CRATES; do
      n="$(awk -F'\t' -v k="$cat" -v cr="$c" '$1==k && $4==cr' "$TSV" | wc -l | tr -d ' ')"
      rowtotal=$((rowtotal + n))
      printf ' %s |' "$n"
    done
    printf ' **%s** |\n' "$rowtotal"
  done
  # Column totals.
  printf '| **total** |'
  for c in $CRATES; do
    n="$(awk -F'\t' -v cr="$c" '$4==cr' "$TSV" | wc -l | tr -d ' ')"
    printf ' **%s** |' "$n"
  done
  printf ' **%s** |\n' "$total"
}

echo
echo "### By construct"
echo
echo "| construct | category | count |"
echo "|---|---|---|"
# One row per distinct (category,label) in fixed table order, with its count.
awk -F'\t' '{ key=$1"\t"$3; c[key]++; if(!(key in seen)){seen[key]=1; order[++n]=key} }
  END { for(i=1;i<=n;i++){ split(order[i],p,"\t"); printf "| %s | (%s) | %d |\n", p[2], p[1], c[order[i]] } }' "$TSV"

echo
echo "## Findings"
echo
# Group by category then label, preserving the sorted order.
awk -F'\t' '
  function catname(k){ if(k=="a")return "(a) common-subset"; if(k=="b")return "(b) mechanical rewrite";
                       if(k=="c")return "(c) mechanism trait"; return "(d) manual design" }
  {
    if ($1 != curcat) { curcat=$1; curlabel=""; printf "\n### %s\n", catname($1) }
    if ($3 != curlabel) {
      curlabel=$3
      mech = ($1=="c") ? " — trait: " $2 : ""
      printf "\n#### %s%s\n\n", $3, mech
    }
    printf "- `%s:%s` — `%s`\n", $5, $6, $7
  }
' "$TSV"

cat <<'SPLIT'

## Proposed MAIN-189 item 4 sweep split

The query sweep should be **file-disjoint** so cards run in parallel without
merge conflicts. The natural cut is by crate, then by the mechanism trait a card
introduces — the (c) findings are the load-bearing ones and each trait is a
separate card that its consumers then adopt. Grouping (files a card owns):

1. **Trait scaffolding (blocks the rest; do first, no query edits).** Define the
   four mechanism traits from MAIN-189 item 3 — `atomic-claim`, `event-bus`,
   `json`, `type-mapping` — with the Postgres impl delegating to today's SQL.
   Owns the new trait module(s) only; touches no existing query.

2. **`nook-infra` queue → atomic-claim trait.** `crates/nook-infra/src/queue/`
   (`SKIP LOCKED`, `locked_until`). Disjoint from all other crates.

3. **`nook-chat` bus → event-bus trait.** `crates/nook-chat/src/bus.rs` +
   `main.rs` LISTEN wiring (`pg_notify`, `LISTEN`). Disjoint.

4. **`nook-control` capabilities/jobs JSON → json trait.**
   `crates/nook-control/src/services/jobs.rs`, `ws/node.rs`, and the other
   `jsonb`/`->>` sites. Disjoint from the queue/bus cards.

5. **type-mapping sweep (migrations + casts).** `uuid`/`timestamptz` column types
   in `crates/nook-control/migrations/` and `crates/nook-chat/migrations/`, plus
   the `::type` casts each crate uses. Cross-cutting but touches migrations +
   isolated cast sites; run after the trait card lands the type map.

6. **mechanical rewrites (b) per crate.** `ILIKE`, `now()`, remaining `::` casts —
   split one card per crate so each edits only its own `src/`. These are
   independent of the trait cards and of each other.

7. **manual-design (d) sites.** Array binds / `array_agg` and `INTERVAL`
   arithmetic — one design-first card, listed by site above; do NOT fold into a
   mechanical card.

Cards 2–4 are fully file-disjoint and can run in parallel once card 1 lands. Card
5 owns the migration dirs; cards 6 are per-crate `src/` and disjoint from 2–4 by
construct (different lines), but to stay strictly conflict-free, schedule the
per-crate (b) card for a given crate to not overlap that crate's (c) card in time.
SPLIT
