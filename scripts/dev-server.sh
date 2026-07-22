#!/usr/bin/env bash
# Docker-first dev loop. cargo watch runs INSIDE the control-plane container
# and rebuilds on save — you normally never need this script; it exists to
# force-restart or tail the containerized services.
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-restart}" in
  restart) docker compose restart control-plane ;;
  logs) docker compose logs -f --tail=100 control-plane node ;;
  up) docker compose up -d --build ;;
  *) echo "usage: dev-server.sh [restart|logs|up]"; exit 1 ;;
esac
