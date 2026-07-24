#!/usr/bin/env bash
# Run the tests. No environment variables to remember.
#
#   ./test.sh              everything: fmt, clippy, tests, typecheck, linters
#   ./test.sh rust         just the Rust tests
#   ./test.sh rust ca      Rust tests matching "ca"
#   ./test.sh lint         fmt + clippy + actionlint + shellcheck
#   ./test.sh web          tsc + vitest across the frontend
#   ./test.sh desktop      fmt, clippy and tests for the Tauri shell
#   ./test.sh k8s          live Helm chart bring-up on a kind cluster
#   ./test.sh --host       run Rust on the host instead of in the container
#
# Runs inside the control-plane container by default. That container already
# holds DATABASE_URL, can reach Postgres by name, and shares the cargo target
# volume with cargo-watch — so it is both correctly configured and already
# warm. Falling back to the host means passing DATABASE_URL by hand, which is
# exactly the thing this script exists to stop doing.
set -euo pipefail
cd "$(dirname "$0")"

HOST=0
[ "${1:-}" = "--host" ] && { HOST=1; shift; }

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  # A real escape byte: `printf '%s'` does not interpret backslash escapes in
  # its arguments, so a variable holding "\033[..." would print literally.
  esc=$(printf '\033')
  A="${esc}[38;5;214m"; G="${esc}[38;5;43m"; R="${esc}[31m"; Z="${esc}[0m"
else
  A=''; G=''; R=''; Z=''
fi
# Colours travel as arguments, never inside the format string: a `%` arriving
# through a variable would be read as a conversion.
say()  { printf '%s▸%s %s\n' "$A" "$Z" "$*"; }
pass() { printf '%s✓%s %s\n' "$G" "$Z" "$*"; }
die()  { printf '%s✗%s %s\n' "$R" "$Z" "$*" >&2; exit 1; }

# Is the dev stack up and usable?
container_ready() {
  docker compose ps --status running --format '{{.Service}}' 2>/dev/null \
    | grep -qx control-plane
}

# Run a command where the Rust tests should run.
rust() {
  if [ "$HOST" = "1" ]; then
    # The host reaches Postgres on the published port rather than by service
    # name. NOOK_REQUIRE_DB makes a missing database a failure instead of a
    # suite that silently skips its database half and still reports success.
    DATABASE_URL="${DATABASE_URL:-postgres://nook:nook@localhost:5432/nook}" \
    NOOK_REQUIRE_DB=1 "$@"
  else
    container_ready || die "the dev stack is not running — 'docker compose up -d', or use ./test.sh --host"
    docker compose exec -T -e NOOK_REQUIRE_DB=1 control-plane "$@"
  fi
}

# Linters that need a container image, skipped rather than failed when Docker
# is unavailable — a missing linter must not look like a passing one.
lint_in() {
  local image=$1; shift
  if ! docker info >/dev/null 2>&1; then
    say "docker unavailable — skipping $image"
    return 0
  fi
  docker run --rm -v "$PWD:/mnt" -w /mnt "$image" "$@"
}

run_lint() {
  say "cargo fmt --check"
  cargo fmt --all --check || die "formatting: run 'cargo fmt --all'"
  pass "formatted"

  say "cargo clippy"
  # Warnings are the point: this project keeps clippy at zero, so let any
  # warning fail rather than scroll past.
  rust cargo clippy --workspace --all-targets -- -D warnings || die "clippy"
  pass "clippy clean"

  say "actionlint"
  # A workflow with a bad expression does not fail a job, it fails to parse —
  # so nothing runs and you find out after pushing a tag.
  lint_in rhysd/actionlint:latest -color || die "actionlint"
  pass "workflows lint clean"

  say "shellcheck"
  lint_in koalaman/shellcheck:stable install/install.sh deploy/enable-agent-mtls.sh test.sh \
    charts/nook-control/ci/validate.sh scripts/k8s-e2e.sh \
    || die "shellcheck"
  pass "shell scripts clean"
}

run_rust() {
  say "cargo test${1:+ (filter: $1)}"
  rust cargo test --workspace ${1:+"$1"} || die "tests"
  pass "tests passed"
}

run_web() {
  say "tsc"
  (cd frontend && pnpm -r typecheck) || die "typecheck"
  pass "frontend typechecks"

  say "vitest"
  (cd frontend && pnpm -r test) || die "frontend tests"
  pass "frontend tests passed"
}

# The desktop shell is deliberately OUTSIDE the cargo workspace — its toolchain
# would slow every backend build — which also meant nothing ever checked it.
# The Tauri app shipped a broken device sign-in and an unformatted source file
# because `cargo fmt --all` and `cargo test --workspace` cannot see it. It gets
# its own target rather than being folded into the workspace.
desktop_deps_ok() {
  case "$(uname -s)" in
    # Tauri builds against the system WebKit; no pkg-config to consult.
    Darwin) return 0 ;;
    Linux) pkg-config --exists webkit2gtk-4.1 2>/dev/null ;;
    *) return 1 ;;
  esac
}

run_desktop() {
  if ! desktop_deps_ok; then
    # Loudly, and never as a pass: a check that quietly did not run is worse
    # than one that fails, because it looks the same as one that succeeded.
    say "webkit2gtk-4.1 not installed — SKIPPING the desktop shell (CI still checks it)"
    return 0
  fi
  local d=frontend/apps/desktop/src-tauri
  say "desktop: cargo fmt --check"
  (cd "$d" && cargo fmt --check) || die "desktop formatting: run 'cargo fmt' in $d"
  say "desktop: cargo clippy"
  (cd "$d" && cargo clippy --all-targets -- -D warnings) || die "desktop clippy"
  say "desktop: cargo test"
  (cd "$d" && cargo test) || die "desktop tests"
  pass "desktop shell passed"
}

# The live chart bring-up needs a real cluster toolchain. Absent kind/helm it
# SKIPS loudly rather than failing — the same "never a silent pass" rule as the
# desktop shell. CI runs it on charts/ changes regardless. It is NOT part of
# `all`: nobody wants `./test.sh` spinning up kind on every run.
run_k8s() {
  if ! command -v kind >/dev/null 2>&1 || ! command -v helm >/dev/null 2>&1; then
    say "kind/helm not installed — SKIPPING the k8s e2e (CI runs it on charts/ changes)"
    return 0
  fi
  say "k8s: Helm chart end-to-end on kind"
  ./scripts/k8s-e2e.sh "$@" || die "k8s e2e"
  pass "k8s e2e passed"
}

case "${1:-all}" in
  rust) run_rust "${2:-}" ;;
  lint) run_lint ;;
  web)  run_web ;;
  desktop) run_desktop ;;
  k8s)  shift; run_k8s "$@" ;;
  all)
    run_lint
    run_rust
    run_web
    run_desktop
    printf '\n%s✓%s everything passed\n' "$G" "$Z"
    ;;
  -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//' ;;
  *) die "unknown target '$1' — try: all, rust, lint, web, desktop, k8s" ;;
esac
