#!/usr/bin/env bash
#
# Tiny test for check-release-version.sh (MAIN-88): a matching tag/version passes,
# a mismatch fails with the AC-3 message, and the version is read from the
# [workspace.package] table — not a decoy `version =` under another section.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
guard="${here}/check-release-version.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# A Cargo.toml whose [workspace.package] version is 0.4.14, with a DECOY version
# under [dependencies] first (AC-5: the decoy must never be picked up).
cargo="${tmp}/Cargo.toml"
cat >"$cargo" <<'TOML'
[workspace]
members = ["crates/a"]

[dependencies]
serde = { version = "1.9.9" }

[workspace.package]
version = "0.4.14"
edition = "2021"
TOML

# ── matching tag → exit 0 ────────────────────────────────────────────────────
if ! "$guard" v0.4.14 "$cargo" >/dev/null; then
  fail "a matching tag (v0.4.14) should pass"
fi
# The leading `v` is optional.
if ! "$guard" 0.4.14 "$cargo" >/dev/null; then
  fail "a matching tag without the v prefix should pass"
fi

# ── mismatching tag → non-zero + the AC-3 message naming both values ─────────
if out="$("$guard" v0.4.15 "$cargo" 2>&1)"; then
  fail "a mismatching tag (v0.4.15) should fail"
fi
case "$out" in
  *"release tag v0.4.15 does not match Cargo.toml workspace version 0.4.14"*"bump [workspace.package] version and re-tag"*) : ;;
  *) fail "the mismatch message must name both versions and the remedy; got: $out" ;;
esac

# ── AC-5: the decoy [dependencies] version (1.9.9) is never used ─────────────
# Tagging v1.9.9 (the decoy) must FAIL — the guard reads 0.4.14 from
# [workspace.package], not the dependency line above it.
if "$guard" v1.9.9 "$cargo" >/dev/null 2>&1; then
  fail "the decoy [dependencies] version must not be treated as the workspace version"
fi

echo "check-release-version.test.sh: ok"
