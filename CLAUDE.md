# NookOS — Development Notes

## Dev loop — DOCKER FIRST

- **Everything runs in containers.** `docker compose up -d` (or `./run.sh` for a clean recreate) starts postgres, control plane, node, and web. Source is bind-mounted; **cargo watch runs INSIDE the control-plane and node containers** and rebuilds on save. Vite hot-reloads in the web container. Never run the services host-native.
- Edit → save → the container rebuilds automatically. Poll `http://localhost:8080/healthz` to know the control plane is back.
- `./scripts/dev-server.sh logs` tails the Rust services; `restart` force-restarts the control plane.
- Host-side `cargo check` is fine for fast compile feedback; running the stack is not.
- `nook join` from the host (against http://localhost:8080) is the "second node" demo path.

## Running the tests

`./test.sh` — no environment variables to remember.

```
./test.sh          fmt, clippy, tests, typecheck, actionlint, shellcheck
./test.sh rust     just the Rust tests      ./test.sh rust ca   filtered
./test.sh lint     linters only             ./test.sh web       tsc
./test.sh --host   run Rust on the host instead of the container
```

Rust runs **inside the control-plane container** by default: it already holds
`DATABASE_URL`, reaches Postgres by service name, and shares the cargo target
volume with cargo-watch, so it is both correctly configured and already warm.

`NOOK_REQUIRE_DB=1` is set for you. Without it, every test needing Postgres
returns early and the suite reports success having executed almost nothing.

## Database workflow (bootstrap phase)

- **Bootstrap is over: migrations are append-only.** `0001_init.sql` is frozen — it has been applied to databases that hold real data and cannot be recreated with `down -v`. Schema changes are NEW numbered files: `0002_user_tokens.sql`, `0003_…`.
- **Never edit an applied migration and re-record its checksum.** The checksum is what proves the schema in front of you is the schema the repo describes; rewriting it makes that proof say "verified" without anything having been verified. If sqlx says *"migration N was previously applied but has been modified"*, the fix is to restore that file and add a new one — not to patch the ledger.
- Write new migrations idempotently (`CREATE TABLE IF NOT EXISTS`) so a database that already got the change by other means converges instead of failing.
- **`sqlx::migrate!` embeds migrations at compile time.** Adding a `.sql` file does not by itself trigger a rebuild — touch `crates/nook-control/src/lib.rs` (where `MIGRATOR` lives) or the container will keep running the old set and silently skip your migration.
- The dev reboot loop still works on a *local* database: `docker compose down -v` destroys everything, `./run.sh` recreates it (migrations + seeds). It is not available for prod.

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
