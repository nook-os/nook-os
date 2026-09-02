#!/usr/bin/env bash
#
# check-chart-appversion.sh — refuse a chart in this repo whose appVersion names
# a release that was never published (MAIN-652).
#
# WHY THIS EXISTS. Every chart here defaults its image tag to `.Chart.AppVersion`,
# so `helm install charts/nook-control` from a clone asks ghcr for
# `nook-control:<appVersion>`. That value sat at 0.4.10 while the fleet released
# 0.6.13, so both Deployments went ImagePullBackOff — silently, because helm
# reports the release as deployed until the rollout times out and only the pods
# say why. The published chart was fine: the release workflow stamps it from the
# tag, so only the person installing from source was hit.
#
# WHY THE LATEST TAG AND NOT Cargo.toml. The workspace version is the version
# the NEXT release will carry — the "Bump workspace version to X for release"
# commit lands right after a release is cut, so main sits at an UNPUBLISHED
# number for its whole life. Pinning the chart to it would name an image that
# does not exist yet, which is the bug with a smaller gap. The last `v*` tag is
# the newest version whose images actually exist, which is what a from-source
# install has to be given.
#
# So this goes RED on main once a release is tagged, until the chart is bumped
# to it. That is deliberate, and it is the check the card asked for: the bump
# belongs in the same commit that opens the next version.
#
# Deliberately NOT part of charts/*/ci/validate.sh — the release job runs that
# BEFORE stamping the chart from the tag it is publishing, so this check would
# fail every release for being one version behind the tag it is cutting.
#
# Usage: check-chart-appversion.sh [--require] [--released VER] [--repo DIR] [CHART.yaml...]
#   --require      an undeterminable released version is a failure, not a skip
#   --released VER the released version, instead of reading it from the tags
#   --repo DIR     the git repository to read tags from (default: this one)
#
# Exit: 0 ok, 1 drift (or --require with nothing to compare against), 2 skipped.
set -euo pipefail
export LC_ALL=C

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "${here}/.." && pwd)"

require=0
released=""
repo="$root"
charts=()
while [ $# -gt 0 ]; do
  case "$1" in
    --require)  require=1; shift ;;
    --released) released="${2:?--released needs a version}"; shift 2 ;;
    --repo)     repo="${2:?--repo needs a directory}"; shift 2 ;;
    -*)         echo "unknown flag: $1" >&2; exit 64 ;;
    *)          charts+=("$1"); shift ;;
  esac
done

# Every chart in the tree, so a chart added later is covered without an edit
# here — the defect this guards against is not specific to nook-control.
if [ "${#charts[@]}" -eq 0 ]; then
  for c in "$root"/charts/*/Chart.yaml; do
    [ -f "$c" ] && charts+=("$c")
  done
fi

# The newest published release. Local tags first; a shallow or tagless clone
# falls back to the remote, and an environment with neither skips rather than
# failing — a guard that cannot reach its input has not found a defect.
if [ -z "$released" ]; then
  tags="$(git -C "$repo" tag --list 'v[0-9]*' 2>/dev/null || true)"
  if [ -z "$tags" ]; then
    tags="$(git -C "$repo" ls-remote --tags --refs origin 'v[0-9]*' 2>/dev/null \
              | sed 's#.*refs/tags/##' || true)"
  fi
  released="$(printf '%s\n' "$tags" \
                | sed 's/^v//' \
                | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' \
                | sort -V | tail -1 || true)"
fi

if [ -z "$released" ]; then
  msg="chart appVersion: no v* release tag found, nothing to compare against"
  if [ "$require" = "1" ]; then
    echo "${msg} — the checkout needs its tags (actions/checkout: fetch-tags)" >&2
    exit 1
  fi
  printf '\033[31m▲ %s (skipped)\033[0m\n' "$msg" >&2
  exit 2
fi

fail=0
for chart in "${charts[@]}"; do
  app="$(awk -F: '/^appVersion:/ { v=$2; gsub(/[[:space:]"]/, "", v); print v; exit }' "$chart")"
  rel="${chart#"$root"/}"
  if [ -z "$app" ]; then
    echo "  FAIL: ${rel} has no appVersion — its images would deploy at :latest by accident" >&2
    fail=1
  elif [ "$app" != "$released" ]; then
    echo "  FAIL: ${rel} appVersion ${app}, but the newest published release is ${released}" >&2
    fail=1
  else
    echo "  ok:   ${rel} appVersion ${app}"
  fi
done

if [ "$fail" -ne 0 ]; then
  cat >&2 <<EOF

A chart's default image tag IS its appVersion, so installing from this repo asks
ghcr for an image only a published release has. Point them at the release:

  sed -i 's/^appVersion:.*/appVersion: "${released}"/' charts/*/Chart.yaml
EOF
  exit 1
fi

echo "chart appVersion: every chart tracks the published release ${released}"
