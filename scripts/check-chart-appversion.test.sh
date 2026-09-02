#!/usr/bin/env bash
#
# Self-test for check-chart-appversion.sh (MAIN-652). Drives the guard against
# fixture charts so its verdict is checked here rather than discovered by a
# `helm install` that pulls an image nobody published.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
guard="${here}/check-chart-appversion.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

chart() { # chart <name> <appVersion-line>
  local f="${tmp}/$1.yaml"
  { echo "apiVersion: v2"; echo "name: $1"; echo "version: 0.2.0"; echo "$2"; } >"$f"
  echo "$f"
}

matching="$(chart matching 'appVersion: "1.2.3"')"
stale="$(chart stale 'appVersion: "0.4.10"')"
missing="$(chart missing '# no appVersion at all')"

# ── the chart names the released version → passes ────────────────────────────
"$guard" --released 1.2.3 "$matching" >/dev/null \
  || fail "a chart whose appVersion is the released version should pass"

# ── the released version moved on and the chart did not → fails ──────────────
# This is the card's own verification step: bump the released version without
# touching the chart, and the check fails.
if out="$("$guard" --released 1.2.4 "$matching" 2>&1)"; then
  fail "bumping the released version past the chart's appVersion should fail"
fi
case "$out" in
  *"appVersion 1.2.3, but the newest published release is 1.2.4"*"appVersion: \"1.2.4\""*) : ;;
  *) fail "the message must name both versions and the remedy; got: $out" ;;
esac

# ── the original defect: ten releases stale ──────────────────────────────────
"$guard" --released 0.6.13 "$stale" >/dev/null 2>&1 \
  && fail "a chart pinned to a superseded release should fail"

# ── one bad chart fails the run even when another is fine ────────────────────
"$guard" --released 1.2.3 "$matching" "$stale" >/dev/null 2>&1 \
  && fail "a stale chart alongside a good one should still fail"

# ── no appVersion at all → fails (the tag would silently become `latest`) ────
if out="$("$guard" --released 1.2.3 "$missing" 2>&1)"; then
  fail "a chart with no appVersion should fail"
fi
case "$out" in
  *"has no appVersion"*) : ;;
  *) fail "the message must say the appVersion is missing; got: $out" ;;
esac

# ── no tags to compare against: a skip by default, a failure under --require ─
# A tagless repository stands in for a shallow clone or an offline run; the
# guard must not invent a verdict from an input it could not read.
empty="${tmp}/norepo"
git init -q "$empty"
set +e
"$guard" --repo "$empty" "$matching" >/dev/null 2>&1; skipped=$?
"$guard" --repo "$empty" --require "$matching" >/dev/null 2>&1; required=$?
set -e
[ "$skipped" -eq 2 ]  || fail "an undeterminable release version should skip (2), got ${skipped}"
[ "$required" -eq 1 ] || fail "--require should turn that skip into a failure (1), got ${required}"

# ── the tree as committed is green ───────────────────────────────────────────
# Skips (2) where there are no tags to read, which is not a pass and says so.
set +e
"$guard" >/dev/null; tree=$?
set -e
case "$tree" in
  0|2) : ;;
  *) fail "the charts in this tree do not track the published release" ;;
esac

echo "check-chart-appversion.test.sh: ok"
