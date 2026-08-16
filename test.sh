#!/usr/bin/env bash
# Run the tests. No environment variables to remember.
#
#   ./test.sh              everything: fmt, clippy, tests, typecheck, linters
#   ./test.sh rust         just the Rust tests (Postgres, the default engine)
#   ./test.sh rust ca      Rust tests matching "ca"
#   ./test.sh rust --sqlite  the SQLite leg — same verdict CI's sqlite job gives
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

# The SAME project name `run.sh` and `dev-up.sh` use. Without it a bare
# `docker compose` here addresses compose's default project — the bare
# directory name — and finds none of the containers the stack actually started,
# so every in-container run reported "the dev stack is not running" while it
# was running perfectly well.
# shellcheck source=scripts/compose-project.sh
. ./scripts/compose-project.sh

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
    # The host reaches Postgres on the PUBLISHED port rather than by service
    # name — and that port is leased now (MAIN-376), so it follows
    # NOOK_PG_PORT with compose's own default. A literal 5432 here reaches
    # whichever checkout happens to hold it, which under --host means running
    # this suite against another instance's database.
    # NOOK_REQUIRE_DB makes a missing database a failure instead of a suite that
    # silently skips its database half and still reports success.
    DATABASE_URL="${DATABASE_URL:-postgres://nook:nook@localhost:${NOOK_PG_PORT:-5432}/nook}" \
    NOOK_REQUIRE_DB=1 "$@"
  else
    container_ready || die "the dev stack is not running — 'docker compose up -d', or use ./test.sh --host"
    docker compose exec -T -e NOOK_REQUIRE_DB=1 control-plane "$@"
  fi
}

# Linters that need a container image, skipped rather than failed when the image
# cannot be obtained — a missing linter must not look like a passing one, and it
# must not look like a FAILING one either.
#
# The `docker info` check was too narrow (MAIN-345 AC-5). Docker can be running
# perfectly while `docker pull` still fails — a credential helper that is not on
# PATH, a rate limit, no network. That surfaced as a hard lint failure, which
# stopped `run_lint` before every source guard below it, which is how
# `check-sqlx-signatures.sh` stayed red on main without anybody seeing it.
#
# So: try to ensure the image, and if it cannot be had, SAY so and carry on. The
# checks that follow are the ones that catch real drift; losing a linter to an
# environment problem must not cost you all of them. CI pulls fine and runs
# every one.
lint_in() {
  local image=$1; shift
  if ! docker info >/dev/null 2>&1; then
    say "docker unavailable — skipping $image"
    # LINT_RAN, on THIS branch too. It is the older and more common skip — a
    # machine with no daemon at all — and leaving it unset let `pass_if_ran`
    # fall back to 1 and print the green tick anyway. Fixing the new branch and
    # not this one would have left the bug alive on the path most people hit.
    LINT_RAN=0
    return 0
  fi
  if ! docker image inspect "$image" >/dev/null 2>&1 &&
     ! docker pull -q "$image" >/dev/null 2>&1; then
    warn_skip "$image could not be pulled — skipping it (the guards below still run)"
    LINT_RAN=0
    return 0
  fi
  LINT_RAN=1
  docker run --rm -v "$PWD:/mnt" -w /mnt "$image" "$@"
}

# `pass`, but only if the linter actually ran. Without this a skipped image
# still printed its green tick, which is the "a skip is not a pass" mistake in
# its most convincing form: the reassuring line is the last thing on screen.
pass_if_ran() { [ "${LINT_RAN:-1}" = "1" ] && pass "$@"; return 0; }

# A skip is not a pass. Loud enough to notice, not fatal.
warn_skip() { printf '\033[31m▲ %s\033[0m\n' "$*" >&2; }

run_lint() {
  say "cargo fmt --check"
  cargo fmt --all --check || die "formatting: run 'cargo fmt --all'"
  pass "formatted"

  say "cargo clippy"
  # Warnings are the point: this project keeps clippy at zero, so let any
  # warning fail rather than scroll past.
  rust cargo clippy --workspace --all-targets -- -D warnings || die "clippy"
  pass "clippy clean"

  say "shellcheck"
  lint_in koalaman/shellcheck:stable install/install.sh deploy/enable-agent-mtls.sh test.sh \
    charts/nook-control/ci/validate.sh scripts/k8s-e2e.sh scripts/dev-db-heal.sh \
    scripts/check-release-version.sh scripts/check-release-version.test.sh \
    scripts/check-secrets-untracked.test.sh run.sh \
    scripts/check-inline-sql.sh scripts/check-inline-sql.test.sh \
    scripts/check-sqlx-signatures.sh scripts/check-sqlx-signatures.test.sh \
    scripts/check-dialect-dispatch.sh scripts/check-dialect-dispatch.test.sh \
    scripts/check-nested-dialect.sh scripts/check-nested-dialect.test.sh \
    scripts/check-sqlite-ci.sh scripts/check-sqlite-ci.test.sh \
    scripts/publish-image-report.sh scripts/publish-image-report.test.sh \
    scripts/squash-migrations.sh \
    scripts/dev-bootstrap.sh scripts/dev-up.sh scripts/compose-project.sh \
    scripts/dev-prewarm.sh scripts/dev-wait-healthy.sh \
    scripts/build-job-sandbox.sh deploy/docker/job-sandbox-entrypoint.sh \
    || die "shellcheck"
  pass_if_ran "shell scripts clean"

  say "release version guard"
  ./scripts/check-release-version.test.sh || die "release version guard"
  pass "release version guard"

  say "credentials untracked"
  ./scripts/check-secrets-untracked.test.sh || die "credentials untracked"
  pass "credentials untracked"

  # MAIN-260: new SQL may not appear outside repo/. The allow-list names every
  # aggregate still awaiting its card, so this is green today and shrinks as
  # MAIN-250..258 land.
  say "inline SQL"
  ./scripts/check-inline-sql.sh || die "inline SQL"
  ./scripts/check-inline-sql.test.sh || die "inline SQL guard self-test"
  pass "inline SQL guarded"

  # MAIN-268 (epic AC-5): a sqlx TYPE may not appear outside the adapter. Same
  # shape as the guard above and green for the same reason — its allow-list
  # names every file whose card is still open, and shrinks as they land.
  say "sqlx signatures"
  ./scripts/check-sqlx-signatures.sh || die "sqlx signatures"
  ./scripts/check-sqlx-signatures.test.sh || die "sqlx signature guard self-test"
  pass "sqlx signatures guarded"

  # MAIN-354: and the fragment a seam is HANDED must not itself be Postgres.
  # Swapping `Postgres.x()` for `time_math(engine).x()` while leaving
  # `$1::bigint` in the argument ships Postgres SQL through the seam that exists
  # to stop it — invisible to this leg, which is why the check reads the source.
  say "nested dialect literals"
  ./scripts/check-nested-dialect.sh || die "nested dialect literals"
  ./scripts/check-nested-dialect.test.sh || die "nested-dialect guard self-test"
  pass "nested dialect literals guarded"

  # MAIN-289: production asks the ENGINE for its SQL fragments rather than
  # assuming Postgres. Green for the same reason as the guard above — the
  # allow-list names every file whose sweep is still open, and shrinks as they
  # land.
  say "dialect dispatch"
  # FATAL again since MAIN-421. It spent weeks as `warn_skip "…pre-existing, its
  # own card"` while the card it named had been archived, so nothing owned the
  # tail and nothing could find it — a guard that is permanently amber protects
  # nothing, and this is the line that made it amber.
  ./scripts/check-dialect-dispatch.sh || die "dialect dispatch"
  ./scripts/check-dialect-dispatch.test.sh || die "dialect-dispatch self-test"
  pass_if_ran "no hardcoded dialect outside nook-db"

  # MAIN-270 (epic AC-6): only the guard's SELF-TEST runs here. The guard itself
  # needs a SQLite run to judge, which is `./test.sh rust --sqlite` (minutes),
  # not something to fold into every lint pass.
  say "sqlite leg guard"
  ./scripts/check-sqlite-ci.test.sh || die "sqlite leg guard self-test"
  pass "sqlite leg guard self-tested"

  # MAIN-604: the image reporter's whole contract is that it cannot fail a
  # build, and every way it might is a network condition nobody reproduces by
  # hand. The self-test drives the real script against fakes for `gh` and
  # `curl`, so the guarantee is checked here rather than discovered on a
  # release.
  say "image report publisher"
  ./scripts/publish-image-report.test.sh || die "image report publisher self-test"
  pass "image report publisher self-tested"

  # LAST on purpose (MAIN-345 AC-5). This is the only check here that needs to
  # pull a Docker image, and in an environment that cannot, it exited before
  # every guard below it ever ran — which is how `check-sqlx-signatures.sh`
  # stayed red on main without anybody seeing it. Ordering it last means a
  # missing image costs you actionlint, not the whole of lint.
  say "actionlint"
  # A workflow with a bad expression does not fail a job, it fails to parse —
  # so nothing runs and you find out after pushing a tag.
  lint_in rhysd/actionlint:latest -color || die "actionlint"
  pass_if_ran "workflows lint clean"
}

run_rust() {
  say "cargo test${1:+ (filter: $1)}"
  rust cargo test --workspace ${1:+"$1"} || die "tests"
  pass "tests passed"
}

# The SQLite leg, same verdict CI gives (MAIN-270).
#
# `cargo test` exits non-zero here BY DESIGN — the binaries named in
# scripts/sqlite-ci-allowlist.txt are expected to fail until their upstream card
# lands — so the exit code is ignored and the guard decides. It answers both
# halves: did anything COVERED break, and has anything EXCLUDED started passing
# (which means a line is now deletable).
#
# DATABASE_URL only has to name the engine: every bed creates its own private
# file and drops it, so this path is never actually opened.
run_rust_sqlite() {
  local log
  log="$(mktemp)"
  say "cargo test on sqlite:// (non-zero exit is expected — the guard decides)"
  if [ "$HOST" = "1" ]; then
    DATABASE_URL="sqlite:///tmp/nook-local-ci.db" NOOK_REQUIRE_DB=1 \
      cargo test --workspace --no-fail-fast 2>&1 | tee "$log" || true
  else
    container_ready || die "the dev stack is not running — 'docker compose up -d', or use ./test.sh --host"
    docker compose exec -T -e NOOK_REQUIRE_DB=1 -e DATABASE_URL="sqlite:///tmp/nook-local-ci.db" \
      control-plane cargo test --workspace --no-fail-fast 2>&1 | tee "$log" || true
  fi
  ./scripts/check-sqlite-ci.sh "$log" || { rm -f "$log"; die "sqlite leg"; }
  rm -f "$log"
  pass "sqlite leg passed"
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
  # `rust --sqlite` is the SQLite leg; `rust [filter]` stays the Postgres one.
  rust)
    if [ "${2:-}" = "--sqlite" ]; then run_rust_sqlite; else run_rust "${2:-}"; fi ;;
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
