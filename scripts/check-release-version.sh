#!/usr/bin/env bash
#
# Release version guard (MAIN-88).
#
# The committed `[workspace.package] version` in Cargo.toml is the source of
# truth: it drives CARGO_PKG_VERSION -> the control plane's advertised
# `expected_agent_version` -> node self-update. The release is tag-triggered, but
# nothing forced the tag to agree with that version — tagging `v0.4.13` without
# the version-bump commit shipped images that ran the right code yet self-reported
# `0.4.12`, so no node ever rolled forward. This guard makes that mismatch fail
# the release loudly, before anything is built or published.
#
# Usage: check-release-version.sh <tag> [cargo_toml]
#   <tag>        e.g. v0.4.14 or 0.4.14 (a leading `v` is stripped)
#   cargo_toml   defaults to ./Cargo.toml; a parameter so the test can feed a fixture
set -euo pipefail

tag="${1:?usage: check-release-version.sh <tag> [cargo_toml]}"
cargo="${2:-Cargo.toml}"

# v0.4.14 -> 0.4.14
tag_version="${tag#v}"

# Read the version from the [workspace.package] table SPECIFICALLY (AC-5): the
# awk stays inside that table, so a `version = "…"` under [dependencies] or any
# other section can never be mistaken for the workspace version.
workspace_version="$(
  awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/                     { in_section = 0 }
    in_section && /^[[:space:]]*version[[:space:]]*=/ {
      gsub(/^[^"]*"/, ""); gsub(/".*$/, ""); print; exit
    }
  ' "$cargo"
)"

if [ -z "$workspace_version" ]; then
  echo "release guard: no [workspace.package] version found in ${cargo}" >&2
  exit 1
fi

if [ "$tag_version" != "$workspace_version" ]; then
  echo "release tag v${tag_version} does not match Cargo.toml workspace version ${workspace_version} — bump [workspace.package] version and re-tag" >&2
  exit 1
fi

echo "release guard: tag v${tag_version} matches Cargo.toml workspace version ${workspace_version}"
