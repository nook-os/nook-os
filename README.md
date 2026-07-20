# NookOS

> **The open workspace operating system for modern software teams.**

NookOS is an open source control plane for software development. It doesn't replace your editor, Git, or your AI — it coordinates everything around them: machines, workspaces, tmux-backed sessions, AI runtimes, kanban, and activity, in one place. See [PLAN.md](PLAN.md) for the full vision.

## Quickstart (Docker first)

```bash
cp .env.example .env    # point OIDC_* at your IdP, or keep AUTH_DEV_MODE=true
./run.sh                # destroys and recreates the entire dev environment
```

Open **http://localhost:5173** — sign in with your identity provider (or the dev sign-in). The stack comes up with a containerized node (`crimson`) that has demo workspaces discovered and ready.

Everything runs in containers with the source bind-mounted: `cargo watch` rebuilds the control plane and node on save; Vite hot-reloads the web app. `docker compose down -v` destroys everything; `./run.sh` brings it all back identically (single bootstrap migration + idempotent seeds).

## Starting work

Hit **+ New Work** (top bar): clone a repo (GitHub/GitLab/Bitbucket/raw, optional SSH key for private), add a git worktree, spin up a new empty project, or open an existing workspace — then pick the machine and the **runtime** (`bash`/`zsh`/`claude`/`hermes`/`codex` — a session runs whatever you choose, not just a shell) and go. The work is the unit; the machine is where it runs.

**Kanban drives work** (control-plane authoritative). A task flows **Triage → Todo → In Progress → Done**: *dispatch* lets the resource-aware scheduler pick the best online node (or you pick), *start work* creates a worktree + session, *submit PR* records the PR, *prune* removes the worktree. Nodes report live CPU/memory/load/session capacity (bars on the Nodes page) so you can see what can take the workload. The whole surface is drivable from AI over MCP.

**Known limitation:** multiple worktrees of the same app on one machine can collide on ports (443/3000); no automatic fix yet (macOS `lo0` aliases work; Linux/WSL reverse-proxy is future). See the in-app **Docs** tab.

## Add a real machine

In the UI: **Nodes → + add node**, then on any machine:

```bash
nook join --server http://localhost:8080 --token nook_join_...
nook run
```

Nodes connect **outbound** over WebSocket — no inbound SSH, no public ports. The node reports its own capabilities (CPU, GPU, docker, tmux, git, installed runtimes like `claude`) and discovers git repositories under its workspace roots. Workspaces — not machines — are the unit you think in; one workspace can exist on many nodes.

## What works today (milestone 1)

- **Generic OIDC login** (any standards-compliant IdP; authorization code + PKCE; configured only via `.env`)
- **Multi-tenant control plane** — Rust / Axum / SQLx / Postgres
- **Node agent** with capability detection, join-token enrollment, reconnect/backoff, workspace discovery
- **Persistent terminal sessions** — tmux-backed, streamed to xterm.js in the browser; survive refreshes, reconnects, and node restarts
- **Local kanban** (drag & drop) behind a federation trait (Jira/GitHub/Linear/Trello providers slot in post-M1)
- **Activity timeline** — everything produces events, streamed live over WebSocket
- **Rolling notes** per workspace
- **Theme engine** with the built-in amber-CRT mission-control theme
- **AI dispatcher** (rule-based; recommends, never acts) and an **MCP server** at `/mcp` so any MCP client can observe and drive NookOS
- **Tauri desktop shell** wrapping the same app
- **Rust owns the types**: OpenAPI is generated from the code, TypeScript is generated from OpenAPI (`./scripts/gen-types.sh`)

## MCP

```text
endpoint:  http://localhost:8080/mcp   (streamable HTTP)
auth:      Authorization: Bearer $MCP_TOKEN   (from .env)
tools:     list_workspaces · list_nodes · list_sessions · start_session ·
           send_to_session · get_activity · get_notes · append_note · create_task
```

## Layout

```
crates/
  nook-types        domain types (single source of truth → OpenAPI → TS)
  nook-proto        node ⇄ control-plane WebSocket protocol
  nook-control      control plane: auth, REST, WS, seeds, MCP mount
  nook-node         the `nook` agent: join/run, tmux/PTY, discovery
  nook-dispatcher   AI dispatcher trait + rule-based backend
  nook-mcp          MCP tool surface (backend trait keeps deps acyclic)
  nook-openapi-gen  emits openapi.json without starting a server
frontend/
  packages/api      generated TS types + typed fetch/WS client
  packages/ui       theme engine, terminal view, components
  packages/app      routes/pages (render-target agnostic)
  apps/web          Vite host  ·  apps/desktop  Tauri 2 shell
```

## Development notes

- Schema changes during bootstrap are edits to `crates/nook-control/migrations/0001_init.sql` — wipe (`docker compose down -v`) and reboot (`./run.sh`); don't accumulate migrations yet.
- After changing API types: `./scripts/gen-types.sh` (CI fails on drift).
- `scripts/dev-server.sh logs|restart` tails or bounces the containerized services.

## License

Apache-2.0
