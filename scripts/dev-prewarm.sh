#!/usr/bin/env bash
# Populate the shared cargo registry with ONE process before the three that
# share it start (MAIN-425). control-plane, chat and worker each mount
# `cargo-registry` and each run `cargo watch`, so on a COLD volume all three
# race to unpack the same crates and lose:
#
#   failed to open .../aws-sigv4-1.5.1/.cargo-ok — File exists (os error 17)
#
# and the loser exits 101 with no file change to make cargo-watch retry — a
# control plane that is "Up" and never serves.
#
# It lives here rather than inline in dev-up.sh because run.sh needs the same
# guard: `down -v` keeps the bind-mounted caches, but nothing stops a checkout
# arriving at run.sh cold — a fresh worktree, or a hand-cleaned `.cache/` —
# and run.sh used to start the three writers straight into the race above.
# One implementation, called by both, so they cannot drift (the MAIN-425
# pattern).
#
# The gate is a MARKER INSIDE the volume, not the volume's existence (MAIN-430).
# Existence cannot work: the `docker compose run` that fetches is itself what
# creates the volume, so a failed fetch left an existing-but-empty volume that
# the next run skipped — one transient failure permanently disarming the guard
# for that project, on exactly the cold-volume case it exists to protect. The
# marker is written only after a fetch that succeeded, so a failure simply
# leaves the next run to try again.
#
# One container start does the check and the work together: warm exits
# immediately, cold fetches and marks.
set -euo pipefail

cd "$(dirname "$0")/.."

# shellcheck source=scripts/compose-project.sh
. ./scripts/compose-project.sh

if ! docker compose run --rm --no-deps --entrypoint "" control-plane sh -c '
      marker=/usr/local/cargo/registry/.nook-prewarmed
      if [ -f "$marker" ]; then exit 0; fi
      echo "▸ cold registry — fetching dependencies once before the workers start"
      cargo fetch && touch "$marker"
    '; then
  # Fatal, deliberately. Proceeding would start the three writers against a
  # registry that is cold precisely because the fetch failed — the silent race
  # above, which presents as a healthy container that never answers. A loud
  # stop here is recoverable; that is not.
  echo "✗ could not prewarm the cargo registry" >&2
  echo "  Starting now would run the three cargo-watch services against a cold" >&2
  echo "  shared registry, whose loser exits 101 and never retries." >&2
  echo "  Fix the cause (network, disk, registry auth) and re-run — nothing was" >&2
  echo "  marked as warmed, so the next attempt fetches again." >&2
  exit 1
fi
