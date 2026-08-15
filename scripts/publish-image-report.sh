#!/usr/bin/env bash
# What a build produced, as a report on the card its pull request closes (MAIN-604).
#
# Reads image rows on stdin — `<image ref> <tag> <digest>`, whitespace separated,
# one per line — and PUTs them as a Markdown table to
# `<NOOK_URL>/api/v1/tasks/<KEY>/reports/images`. `<KEY>` comes from the
# `Closes KEY` line of the pull request the built commit belongs to, read by the
# same literal contract `pr_hygiene.rs::closes_key` uses, so a PR the reviewer
# accepts is one this finds a card for.
#
# The report key is a constant, and that IS the mechanism: `PUT` at a key creates
# or replaces, so building the same pull request again updates the one report
# rather than adding a second.
#
# NOTHING HERE MAY FAIL THE BUILD (AC-5, AC-6). No pull request, no `Closes`
# line, a key naming no card, an unreachable control plane, a revoked token —
# each of them says so and exits 0. A report is a remark ABOUT a build, and a
# remark that cannot be delivered is not a reason to throw the images away.
#
# `set -e` is absent for that same reason: errexit turns any unchecked command
# into exactly the failure this must never produce. Every fallible command is
# checked by hand instead.
set -uo pipefail

# Fixed rather than configurable. The key is the address the upsert depends on,
# and a title taken from the environment would be a second place for a caller to
# put something this then has to escape into JSON.
REPORT_KEY=images
REPORT_TITLE=Images

NOOK_URL="${NOOK_URL:-}"
NOOK_TOKEN="${NOOK_TOKEN:-}"
REPO="${GITHUB_REPOSITORY:-}"

note() { printf '::notice::%s\n' "$*"; }
warn() { printf '::warning::%s\n' "$*"; }

# `Closes KEY`, by the same rule the control plane applies: the prefix on a
# trimmed line, first token, `<prefix>-<digits>`. Narrower in one way — the key
# must also look like a key — because this one is spliced into a URL path, and a
# body is written by whoever opened the pull request.
closes_key() {
  awk '
    {
      line = $0
      sub(/^[[:space:]]+/, "", line)
      sub(/[[:space:]]+$/, "", line)
    }
    line ~ /^Closes / {
      key = substr(line, 8)
      sub(/[[:space:]].*$/, "", key)
      if (key ~ /^[A-Za-z][A-Za-z0-9-]*-[0-9]+$/) { print key; exit }
    }'
}

# A field that can be written into a Markdown table and a JSON string as itself.
safe() { printf '%s' "$1" | grep -qE '^[A-Za-z0-9._:/@-]+$'; }

sha="${1:-}"
if [ -z "$sha" ]; then
  warn "publish-image-report: no commit given — no image report"
  exit 0
fi

rows="$(cat)"

if [ -z "$NOOK_URL" ] || [ -z "$NOOK_TOKEN" ]; then
  note "NOOK_URL or NOOK_TOKEN is unset — no image report for ${sha}"
  exit 0
fi
if [ -z "$REPO" ]; then
  warn "GITHUB_REPOSITORY is unset — cannot find the pull request for ${sha}"
  exit 0
fi

# Every field is registry output, so it is checked before it reaches a Markdown
# table and a JSON body. A row that cannot be written as itself is dropped and
# named, never escaped into something else.
body_rows=""
while read -r image tag digest _; do
  [ -n "${image:-}" ] || continue
  if [ -z "${tag:-}" ] || [ -z "${digest:-}" ]; then
    warn "skipping an incomplete image row: ${image} ${tag:-} ${digest:-}"
    continue
  fi
  if safe "$image" && safe "$tag" && safe "$digest"; then
    body_rows+="| \`${image}\` | \`${tag}\` | \`${digest}\` |"$'\n'
  else
    warn "skipping an image row with unexpected characters: ${image} ${tag} ${digest}"
  fi
done <<<"$rows"

if [ -z "$body_rows" ]; then
  note "no images to report for ${sha}"
  exit 0
fi

errf="$(mktemp)"
outf="$(mktemp)"
cfgf="$(mktemp)"
trap 'rm -f "$errf" "$outf" "$cfgf"' EXIT

# One call, reduced by `--jq` to the two things this needs: an OPEN pull request
# for the commit if there is one, else whichever the API lists first.
if ! pr="$(gh api "repos/${REPO}/commits/${sha}/pulls" \
  --jq '([.[] | select(.state == "open")] + .) | .[0] | select(. != null) | "\(.number)\n\(.body // "")"' \
  2>"$errf")"; then
  warn "could not ask GitHub which pull request ${sha} belongs to: $(tr '\n' ' ' <"$errf")"
  exit 0
fi
if [ -z "$pr" ]; then
  note "no pull request for ${sha} — no image report"
  exit 0
fi

number="$(printf '%s\n' "$pr" | head -1)"
key="$(printf '%s\n' "$pr" | tail -n +2 | closes_key)"
if [ -z "$key" ]; then
  note "pull request #${number} has no 'Closes KEY' line — no image report"
  exit 0
fi

table="| Image | Tag | Digest |"$'\n'"| --- | --- | --- |"$'\n'"${body_rows}"
# The fields are already known to hold nothing JSON has to escape, so the only
# encoding left is the newline.
encoded="$(printf '%s' "$table" | awk 'BEGIN { ORS = "" } { print sep $0; sep = "\\n" }')"

# The credential goes in a config file rather than an argument: `mktemp` makes it
# 0600, and an argument is readable by anything that can list processes.
printf 'header = "Authorization: Bearer %s"\n' "$NOOK_TOKEN" >"$cfgf"
url="${NOOK_URL%/}/api/v1/tasks/${key}/reports/${REPORT_KEY}"
if ! code="$(curl -sS -X PUT --max-time 30 --config "$cfgf" \
  -H 'content-type: application/json' \
  --data-binary "{\"title\":\"${REPORT_TITLE}\",\"body_md\":\"${encoded}\"}" \
  -o "$outf" -w '%{http_code}' "$url" 2>"$errf")"; then
  warn "could not reach ${NOOK_URL} to report images on ${key}: $(tr '\n' ' ' <"$errf")"
  exit 0
fi

case "$code" in
  2*)
    note "reported $(printf '%s' "$body_rows" | grep -c .) image(s) on ${key} (pull request #${number})"
    ;;
  404)
    warn "${key} names no card this token can reach — no image report"
    ;;
  *)
    warn "the control plane refused the image report on ${key} (HTTP ${code}): $(head -c 300 "$outf" | tr '\n' ' ')"
    ;;
esac
exit 0
