#!/usr/bin/env bash
# Can the image reporter fail a build? (MAIN-604 AC-3, AC-5, AC-6)
#
# The publisher's entire contract is negative — it must publish when it can and
# stay out of the way when it cannot — and every "cannot" is a live network
# condition nobody reproduces by hand. So `gh` and `curl` are replaced with
# fakes on PATH and the real script is run against each of them: no pull
# request, no `Closes` line, a key naming no card, a control plane that does not
# answer. Every case asserts the same two things: the exit code is 0, and the
# request either went out correctly or did not go out at all.
set -euo pipefail
cd "$(dirname "$0")/.."

PUBLISH=scripts/publish-image-report.sh
TMP="$(mktemp -d)"
BIN="$TMP/bin"
mkdir -p "$BIN"
fail=0
trap 'rm -rf "$TMP"' EXIT

TOKEN='nook_user_secret_value'
SHA=1111222233334444555566667777888899990000
ROWS="ghcr.io/nook-os/nook-control rc-11112222 sha256:aaaa
ghcr.io/nook-os/nook-web rc-11112222 sha256:bbbb"

# The one call the publisher makes, already reduced by `--jq` to "<number>\n<body>".
cat > "$BIN/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh $*" >> "$FAKE_LOG"
if [ "${FAKE_GH_FAIL:-0}" = 1 ]; then
  echo "gh: HTTP 503" >&2
  exit 1
fi
printf '%s' "${FAKE_PR:-}"
EOF

# Enough of curl's surface to record what was sent and answer with a code.
cat > "$BIN/curl" <<'EOF'
#!/usr/bin/env bash
out=""; data=""; url=""; args=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    --data-binary) data="$2"; shift 2 ;;
    --config) sed 's/^/curl config: /' "$2" >> "$FAKE_LOG"; args="$args --config"; shift 2 ;;
    -w|-H|--max-time|-X) args="$args $1 $2"; shift 2 ;;
    -*) args="$args $1"; shift ;;
    *) url="$1"; shift ;;
  esac
done
{
  echo "curl args:$args"
  echo "curl url: $url"
  echo "curl data: $data"
} >> "$FAKE_LOG"
[ -z "$out" ] || printf '%s' "${FAKE_BODY:-}" > "$out"
printf '%s' "${FAKE_HTTP_CODE:-200}"
exit "${FAKE_CURL_EXIT:-0}"
EOF
chmod +x "$BIN/gh" "$BIN/curl"

# Run the real script with the fakes in front, and remember how it ended.
run() {
  : > "$TMP/log"
  set +e
  PATH="$BIN:$PATH" FAKE_LOG="$TMP/log" \
    NOOK_URL="${NOOK_URL_OVERRIDE-https://nook.example}" \
    NOOK_TOKEN="${NOOK_TOKEN_OVERRIDE-$TOKEN}" \
    GITHUB_REPOSITORY=nook-os/nook-os \
    ./$PUBLISH "$SHA" <<<"${ROWS_OVERRIDE-$ROWS}" >"$TMP/out" 2>&1
  status=$?
  set -e
}

# Every knob the fakes read, back to nothing. In bash an assignment prefixing a
# FUNCTION call outlives the call, so one case's 404 would otherwise still be in
# force three cases later — and every case here asserts an exit code of 0, which
# is what a leaked knob is least likely to disturb.
reset() {
  unset FAKE_PR FAKE_GH_FAIL FAKE_HTTP_CODE FAKE_BODY FAKE_CURL_EXIT \
        ROWS_OVERRIDE NOOK_URL_OVERRIDE NOOK_TOKEN_OVERRIDE
}

# A tick only when nothing failed since the last one — otherwise a case prints
# its ✗ and its ✓, and the reassuring line is the last thing on screen.
last_fail=0
ok() {
  [ "$fail" = "$last_fail" ] && echo "  ✓ $1"
  last_fail=$fail
  return 0
}
bad() { echo "  ✗ $1" >&2; sed 's/^/      /' "$TMP/out" >&2; fail=1; }

# EVERY case asserts this. A build is never failed by a remark about it.
green() { [ "$status" -eq 0 ] || bad "$1 — exited $status"; }
published() { grep -q '^curl url: ' "$TMP/log" || bad "$1 — nothing was published"; }
silent() { ! grep -q '^curl url: ' "$TMP/log" || bad "$1 — it published anyway"; }
logged() { grep -q "$2" "$TMP/log" || { bad "$1 — the log has no $2"; }; }
said() { grep -q "$2" "$TMP/out" || bad "$1 — it never said $2"; }

# ── the happy path ──────────────────────────────────────────────────────────
reset
export FAKE_PR="7
What changed

Closes MAIN-604

Risk: Low"
run
green "a PR with a Closes line"
published "a PR with a Closes line"
logged "a PR with a Closes line" 'curl url: https://nook.example/api/v1/tasks/MAIN-604/reports/images'
logged "a PR with a Closes line" 'X PUT'
ok "a PR with a Closes line publishes to that card"

# AC-2: the body is a Markdown table, one row per image built.
data="$(grep '^curl data: ' "$TMP/log")"
# shellcheck disable=SC2016  # the backticks are Markdown, not a subshell
for want in '| Image | Tag | Digest |' '| --- | --- | --- |' \
            '| `ghcr.io/nook-os/nook-control` | `rc-11112222` | `sha256:aaaa` |' \
            '| `ghcr.io/nook-os/nook-web` | `rc-11112222` | `sha256:bbbb` |'; do
  case "$data" in *"$want"*) ;; *) echo "  ✗ the table has no row: $want" >&2; fail=1 ;; esac
done
case "$data" in
  *'"title":"Images"'*) ;;
  *) echo "  ✗ the report is not titled Images" >&2; fail=1 ;;
esac
ok "the body is a table of image, tag and digest"

# The publisher writes that JSON by hand — it holds no jq and no python, so a
# runner needs nothing but `gh` and `curl` — which makes "is it still JSON"
# worth asking of the real output rather than of the code that shapes it.
if command -v python3 >/dev/null; then
  printf '%s' "${data#curl data: }" > "$TMP/payload.json"
  python3 - "$TMP/payload.json" <<'JSONCHECK' || fail=1
import json, sys
d = json.load(open(sys.argv[1]))
assert sorted(d) == ["body_md", "title"], d
assert d["body_md"].count("\n") == 3, d["body_md"]
JSONCHECK
  ok "the request body is valid JSON holding a four-line table"
fi

# The credential reaches curl through a 0600 config file, never an argument that
# anything able to list processes could read.
grep -q "curl config: header = \"Authorization: Bearer $TOKEN\"" "$TMP/log" \
  || { echo "  ✗ the token never reached curl" >&2; fail=1; }
if grep -E '^curl (args|url|data):' "$TMP/log" | grep -q "$TOKEN"; then
  echo "  ✗ the token appears in curl's arguments" >&2
  fail=1
fi
ok "the token is passed by config file, not on the command line"

# ── AC-3: a second build of the same PR replaces, it does not add ───────────
first_url="$(grep '^curl url: ' "$TMP/log")"
export ROWS_OVERRIDE="ghcr.io/nook-os/nook-control rc-99998888 sha256:cccc"
run
green "a second build"
[ "$(grep '^curl url: ' "$TMP/log")" = "$first_url" ] \
  || bad "a second build addressed a different report"
logged "a second build" 'sha256:cccc'
ok "a rebuild PUTs the same key — one report, new content"

# ── AC-5: no card to report to ─────────────────────────────────────────────
reset
export FAKE_PR=""
run
green "a commit on no pull request"
silent "a commit on no pull request"
said "a commit on no pull request" 'no pull request'
ok "a commit belonging to no pull request publishes nothing, and passes"

reset
export FAKE_PR="12
What changed, with no join to a card at all."
run
green "a PR with no Closes line"
silent "a PR with no Closes line"
said "a PR with no Closes line" 'no .Closes KEY. line'
ok "a PR with no Closes line publishes nothing, and passes"

# `Closes #12` is GitHub's own join to an issue, not a card key.
reset
export FAKE_PR="12
Closes #12"
run
green "Closes on a GitHub issue"
silent "Closes on a GitHub issue"
ok "'Closes #12' is not a card key"

# A key shape that is not a key, and one that is trying to be a path.
reset
export FAKE_PR="12
Closes the gap
Closes MAIN-
Closes ../../admin-1"
run
green "a Closes line that is not a key"
silent "a Closes line that is not a key"
ok "a Closes line that names no key publishes nothing"

# ...and the key really is read from the line, not merely found somewhere.
reset
export FAKE_PR="12
  Closes WEB-UI-7  "
run
logged "an indented multi-part key" 'tasks/WEB-UI-7/reports/images'
ok "an indented, multi-part key resolves"

# ── AC-5: the card does not exist ──────────────────────────────────────────
reset
export FAKE_PR="7
Closes MAIN-604"
export FAKE_HTTP_CODE=404 FAKE_BODY='{"error":"not found"}'
run
green "a key naming no card"
said "a key naming no card" 'names no card'
ok "a key naming no card warns and passes"

# ── AC-6: the control plane is unreachable, or refuses ─────────────────────
reset
export FAKE_PR="7
Closes MAIN-604"
export FAKE_CURL_EXIT=7 FAKE_HTTP_CODE=000
run
green "an unreachable control plane"
said "an unreachable control plane" '::warning::'
ok "an unreachable control plane warns and passes"

reset
export FAKE_PR="7
Closes MAIN-604"
export FAKE_HTTP_CODE=403 FAKE_BODY='{"error":"forbidden"}'
run
green "a revoked token"
said "a revoked token" '403'
ok "a token the control plane refuses warns and passes"

reset
export FAKE_GH_FAIL=1
run
green "GitHub not answering"
silent "GitHub not answering"
said "GitHub not answering" '::warning::'
ok "GitHub not answering warns and passes"

# ── not configured at all: a fork, or before the secret is set ──────────────
reset
export FAKE_PR="7
Closes MAIN-604"
export NOOK_TOKEN_OVERRIDE=
run
green "no token configured"
silent "no token configured"
! grep -q '^gh ' "$TMP/log" || { echo "  ✗ it asked GitHub with nothing to publish to" >&2; fail=1; }
ok "an unconfigured repository publishes nothing, and passes"

reset
export FAKE_PR="7
Closes MAIN-604"
export NOOK_URL_OVERRIDE=
run
green "no URL configured"
silent "no URL configured"
ok "an unset NOOK_URL publishes nothing, and passes"

# ── rows that are not rows ─────────────────────────────────────────────────
reset
export FAKE_PR="7
Closes MAIN-604"
export ROWS_OVERRIDE=""
run
green "no images"
silent "no images"
ok "no images means no report"

# Registry output reaches a Markdown table and a JSON string. A field that
# cannot be written as itself is dropped and named, never escaped into
# something else.
reset
export FAKE_PR="7
Closes MAIN-604"
# shellcheck disable=SC2016  # the command substitution is the payload, unexpanded
export ROWS_OVERRIDE='ghcr.io/nook-os/nook-control rc-1 sha256:aaaa
evil"|`$(whoami)` rc-1 sha256:bbbb
ghcr.io/nook-os/nook-web rc-1'
run
green "a hostile row"
published "a hostile row"
logged "a hostile row" 'sha256:aaaa'
if grep '^curl data: ' "$TMP/log" | grep -q 'whoami'; then
  echo "  ✗ an unquotable field reached the report body" >&2
  fail=1
fi
said "a hostile row" 'unexpected characters'
said "a hostile row" 'incomplete image row'
ok "an unwritable field is dropped and named; the rest still publishes"

exit "$fail"
