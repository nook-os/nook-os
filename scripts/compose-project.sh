# shellcheck shell=sh
# Sourced, not executed. One definition of this checkout's compose project name.
#
# A second stack on one machine needs its own project name, or two checkouts
# fight over the same container names and volumes. Deriving it from the
# directory makes it stable per worktree and needs no configuration.
#
# It lives here because run.sh and dev-up.sh BOTH need it and only dev-up.sh
# had it: run.sh fell back to compose's own default (the bare directory name),
# so `./run.sh` and `./scripts/dev-up.sh` addressed two different projects in
# the same checkout — two sets of volumes, two cold build caches, and a
# `down -v` that missed the stack the other one had started.
if [ -z "${COMPOSE_PROJECT_NAME:-}" ]; then
  # `tr -c` would convert the trailing newline too, leaving a stray hyphen
  # (MAIN-430 AC-5) — drop it before translating rather than after.
  COMPOSE_PROJECT_NAME="nook-$(basename "$PWD" | tr -d '\n' | tr -c '[:alnum:]_-' '-' | tr '[:upper:]' '[:lower:]')"
  export COMPOSE_PROJECT_NAME
fi
