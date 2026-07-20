#!/usr/bin/env bash
# Install NookOS agent skills into a Hermes installation.
#
# Hermes reads skills from ~/.hermes/skills AND from each agent profile's own
# ~/.hermes/profiles/<profile>/skills — profiles hold copies, not symlinks, so
# "add it for all my agents" means writing it to every one of them.
#
#   ./skills/install.sh                 # this machine
#   ./skills/install.sh --host crimson  # over ssh
set -euo pipefail
cd "$(dirname "$0")"

SKILL="nookos"
HOST=""

while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST="${2:?--host needs a value}"; shift 2 ;;
    -h|--help) echo "usage: $0 [--host <ssh-host>]"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -f "$SKILL/SKILL.md" ] || { echo "no $SKILL/SKILL.md next to this script" >&2; exit 1; }

# Runs on the target machine, with the skill already at /tmp/nook-skill.md.
REMOTE_SCRIPT='
set -euo pipefail
category="autonomous-ai-agents"
skill="'"$SKILL"'"
src="/tmp/nook-skill.md"

[ -d "$HOME/.hermes" ] || { echo "no ~/.hermes on $(hostname) — is Hermes installed?" >&2; exit 1; }

install_to() {
  dir="$1/$category/$skill"
  mkdir -p "$dir"
  cp "$src" "$dir/SKILL.md"
  echo "  -> $dir/SKILL.md"
}

echo "installing $skill on $(hostname):"
install_to "$HOME/.hermes/skills"
for p in "$HOME"/.hermes/profiles/*/skills; do
  [ -d "$p" ] || continue          # no profiles is fine, not an error
  install_to "$p"
done
rm -f "$src"
'

if [ -n "$HOST" ]; then
  scp -q "$SKILL/SKILL.md" "$HOST:/tmp/nook-skill.md"
  ssh "$HOST" "bash -s" <<< "$REMOTE_SCRIPT"
else
  cp "$SKILL/SKILL.md" /tmp/nook-skill.md
  bash -c "$REMOTE_SCRIPT"
fi
