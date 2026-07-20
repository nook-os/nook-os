# NookOS — Development Notes

## Dev loop — DOCKER FIRST

- **Everything runs in containers.** `docker compose up -d` (or `./run.sh` for a clean recreate) starts postgres, control plane, node, and web. Source is bind-mounted; **cargo watch runs INSIDE the control-plane and node containers** and rebuilds on save. Vite hot-reloads in the web container. Never run the services host-native.
- Edit → save → the container rebuilds automatically. Poll `http://localhost:8080/healthz` to know the control plane is back.
- `./scripts/dev-server.sh logs` tails the Rust services; `restart` force-restarts the control plane.
- Host-side `cargo check` is fine for fast compile feedback; running the stack is not.
- `nook join` from the host (against http://localhost:8080) is the "second node" demo path.

## Database workflow (bootstrap phase)

- There is exactly **one migration**: `crates/nook-control/migrations/0001_init.sql`. Schema changes are edits to that file — do NOT add incremental migration files during bootstrap.
- The reboot loop is the workflow: `docker compose down -v` destroys everything, `./run.sh` recreates it (migration + seeds). Rebooting must stay predictable.

## Ports

- Postgres: 5432. Control plane: 8080. Web (Vite): 5173, proxies `/api` to 8080.

## Work model (Git-driven)

- **Git → Workspaces → Projects → Sessions.** A workspace is a repo; it can live on many nodes; each checkout (primary clone or **git worktree**) is a *location*. A **session** is a tmux-backed terminal running a chosen **runtime** in one checkout — runtimes are `bash`/`zsh`/`claude`/`hermes`/`codex`/… (sessions are NOT bash-only).
- **"+ New Work"** (top bar) is the unified entry: clone / new worktree / new empty project / existing workspace → pick node → pick runtime → session. Node selection is explicit.
- **Kanban drives work, control-plane authoritative.** Columns: Triage · Todo · In Progress · Done. Triage `dispatch` uses the resource-aware scheduler (`nook-dispatcher::pick_node`) to place work on the best node; start-work makes a worktree+session; submit-pr records a PR; prune removes the worktree. Endpoints: `/tasks/{id}/{dispatch,start-work,submit-pr,prune-worktree,move}` (`services/taskwork.rs`).
- **Nodes report live resources** each heartbeat (`NodeResources`: cpu/mem/load/sessions) → `nodes.resources` + `UiEvent::NodeResources` → capacity bars in the UI; feeds triage.
- **MCP parity**: git + kanban management is fully drivable from `/mcp` (clone/create_project/add_worktree/dispatch/start_work/move/submit_pr). Joining nodes stays human-only.
- **Known limitation — port collisions**: multiple worktrees of one app on a machine contend for ports (443/3000). No auto-fix yet; macOS `lo0` aliases work, Linux/WSL → future reverse-proxy / node-advertised ports. Documented in the in-app Docs page.

## UI direction

- **Full-screen application, not a web page.** Use ALL the real estate: edge-to-edge panels, dense information layout, split panes, thin borders. No hero sections, no max-width containers, no generous padding/whitespace.
- Feels **native to the OS** it runs on — window chrome-style top bar with app-section tabs (Dashboard, Workspaces, Board, Activity, …), persistent status strip, panel-based layout like a terminal multiplexer / mission-control console.
- Amber-CRT default theme (PLAN.md): near-black background, amber primary, monospace, subtle glow. Compact type scale (12–13px base in panels).

- Rust owns the types (`nook-types`); regen TS with `./scripts/gen-types.sh` after changing API types. Generated `schema.d.ts` is committed.
- `.env` values with spaces must be quoted (dotenvy stops parsing at the first malformed line).
- No provider-specific auth code — OIDC config lives only in `.env`.
