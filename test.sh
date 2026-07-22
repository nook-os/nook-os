#!/usr/bin/env bash
# Run the tests. No environment variables to remember.
#
#   ./test.sh              everything: fmt, clippy, tests, typecheck, linters
#   ./test.sh rust         just the Rust tests
#   ./test.sh rust ca      Rust tests matching "ca"
#   ./test.sh lint         fmt + clippy + actionlint + shellcheck
#   ./test.sh web          tsc across the frontend
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
}

case "${1:-all}" in
  rust) run_rust "${2:-}" ;;
  lint) run_lint ;;
  web)  run_web ;;
  all)
    run_lint
    run_rust
    run_web
    printf '\n%s✓%s everything passed\n' "$G" "$Z"
    ;;
  -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' ;;
  *) die "unknown target '$1' — try: all, rust, lint, web" ;;
esac
