# NookOS — Development Notes

## Dev loop — DOCKER FIRST

- **Everything runs in containers.** `docker compose up -d` (or `./run.sh` for a clean recreate) starts postgres, control plane, node, the **operator node** (the shared loop machine — MAIN-125/140; its first build is slow, then layer-cached; skip it with `--scale operator-node=0`), and web. Source is bind-mounted; **cargo watch runs INSIDE the control-plane and node containers** and rebuilds on save. Vite hot-reloads in the web container. Never run the services host-native.
- Edit → save → the container rebuilds automatically. Poll `http://localhost:8080/healthz` to know the control plane is back.
- `./scripts/dev-server.sh logs` tails the Rust services; `restart` force-restarts the control plane.
- **Dev email goes to Mailpit.** The dev control plane is wired `MAIL_PROVIDER=smtp` → `mailpit:1025`, so verification and invite emails land in a live inbox — read them at `http://localhost:8025`. Prod is unaffected (shipped default is `MAIL_PROVIDER` unset → `capture`, which delivers nothing).
- **The fleet's Claude login is a separate identity, mounted in (MAIN-238).** Loop jobs run `claude` on a node, and `nook-dispatcher` only places work on a node reporting the runtime **authorized** — so with no login a spec job sits queued forever with *"no eligible executor"*. The session lives in the gitignored `.nook-secrets/claude/`, mounted at `/nook-claude` with `CLAUDE_CONFIG_DIR` pointing at it (verified: `claude` 2.1.220 reads and writes its `.claude.json` there), on both the operator node and the dev node. `./run.sh` detects a missing session after the stack is healthy and offers the device login; `./run.sh --claude-login` runs it any time. **Subscription device-login only — never an API key.** Because the credentials are *only* in that mount, the fleet's account is separate from your own `~/.claude`: swap it with `rm -rf .nook-secrets/claude && ./run.sh --claude-login` (the files are written by the container as root, so that `rm` may need `sudo`) without touching your personal login. An empty or absent dir is fine — the stack boots and the runtime simply reports `not_authorized`. Check with `docker compose exec operator-node claude auth status`.
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

**Isolation model: a private database per test (`nook_testkit::TestBed`).** Every
DB-backed test opens `let Some(mut bed) = TestBed::new().await else { return };`,
which creates a fresh uniquely-named database, migrates and seeds it, and drops
it whole at `bed.teardown().await` (Drop is a safety-net on panic). Use
`bed.pool`, `bed.app_state().await`, and the entity helpers (`bed.tenant`/`user`/
`node`/`workspace`); `NOOK_KEEP_TEST_DATA=1` keeps a database for debugging.
Because each test owns its database, they run in parallel with no contention and
a test may add a migration freely — nothing a test does ever touches the shared
dev DB's ledger (MAIN-166 retired the old shared-`test_pool` path, so this is
enforced by compilation: there is no API to migrate or write the shared dev DB
from a test). The shared `DATABASE_URL` database serves only the running dev
stack. (Global-count assertions still make no sense — but that's ordinary test
hygiene now, not a shared-DB workaround.)

## Database workflow (bootstrap phase)

- **Migrations are append-only.** `0001_init.sql` is the whole schema and is frozen. Schema changes are NEW numbered files starting at `0002_…`.
- **Never edit an applied migration and re-record its checksum.** The checksum is what proves the schema in front of you is the schema the repo describes; rewriting it makes that proof say "verified" without anything having been verified. If sqlx says *"migration N was previously applied but has been modified"*, the fix is to restore that file and add a new one — not to patch the ledger.
- **Squashing the set is a supported, mechanized operation — use `scripts/squash-migrations.sh` (MAIN-235).** It is still a deliberate exception to the append-only rule, never a licence to edit an applied file, but it is no longer a hand-run one-off. The script applies every current migration to a virgin database, `pg_dump`s what they actually produced as the new `0001`, and refuses to write anything unless three checks pass: schema diff against the pre-squash database is empty, seed-row counts match table by table, and the diff itself is proven able to detect an injected difference. Verification (c) is `./test.sh` after it writes. Run it in the compose Postgres container, which has `psql`/`pg_dump`:
  ```
  docker compose run --rm -v "$PWD:/repo" -w /repo --entrypoint bash postgres \
    scripts/squash-migrations.sh --set control          # dry run: verify only
  ...same with --apply                                   # then: touch crates/nook-control/src/lib.rs && ./test.sh
  ```
  `--set chat` does nook-chat's set the same way. **The first squash (19→1, 2026-07-23) was done by hand; this replaces that method, not the file.**
- **The re-stamp ships INSIDE the image — that is the whole design, and it is why the squash is safe now.** A squash emits `migrations/squash-manifest.txt` naming the exact ledger it replaced (every old version *and* checksum). At boot, before the migrator runs, `nook_db::restamp` collapses a ledger matching that manifest to the single new row **in one transaction**, then proceeds. So the image carrying the squash carries its own re-stamp: there is no second step and no ordering for an operator to get wrong. Four outcomes, no fifth — virgin database (migrator applies `0001`), already-squashed (no-op, *including* once `0002…` land on top), exact match (collapsed), **anything else (left completely untouched with a loud error)**. That last one is fatal in production on purpose: a ledger we cannot account for is not one we rewrite on a guess. In dev it is a WARN and MAIN-224's tolerance carries the boot.
- **The prod near-miss this replaces, kept because the lesson is the ordering.** Prod was re-stamped to a single row by hand while still running an image embedding all nineteen migrations — with a 1-row ledger it would have tried to re-apply `0002`–`0019` against a schema that already had them, on its next restart. Caught and reverted. Never re-stamp a ledger separately from the deploy that changes the migration set; the mechanic above exists so you never have to.
- **A squash strands every unmerged branch against the shared dev DB.** Once the ledger holds the new `0001` checksum, any checkout still carrying the old set hits a *checksum mismatch*, which is fatal everywhere (not the tolerated missing-version case). Rebase open branches onto the squash, or `./run.sh` to rebuild the dev DB. Land a squash when the tree is quiet.
- Write new migrations idempotently (`CREATE TABLE IF NOT EXISTS`) so a database that already got the change by other means converges instead of failing. Idempotency is also what makes re-apply-after-merge safe: once a branch's migration polluted the shared dev DB (below), the *merged* copy re-running against that same database must converge, not fail.
- **`sqlx::migrate!` embeds migrations at compile time.** Adding a `.sql` file does not by itself trigger a rebuild — touch `crates/nook-control/src/lib.rs` (where `MIGRATOR` lives) or the container will keep running the old set and silently skip your migration.
- **Ledger-ahead-of-tree is a dev hazard, tolerated in dev, fatal in prod (MAIN-224).** A branch carrying migration N runs against the shared dev DB — most often an inline `#[cfg(test)]` module in nook-control or nook-chat that connects straight to `DATABASE_URL` and runs `MIGRATOR.run`, so *any* `./test.sh` from a branch/worktree with a new migration records N — or a stack boot from that checkout. Afterwards every checkout *without* that `.sql` file used to fail boot with *"migration N was previously applied but is missing in the resolved migrations,"* and switching the bind-mounted tree to any branch behind the ledger bricked the control plane. Now the boot path (`nook_db::migrate::run_with_dev_tolerance`) runs both services' migrators with sqlx's `ignore_missing` **when `APP_ENV != production`**: it emits a loud WARN naming each unknown version and this failure class, then proceeds. Production keeps the strict fatal error, so real schema drift is never masked. This tolerates a *missing* version only — a *modified* migration (checksum mismatch) stays fatal everywhere.
- **Heal the ledger with `scripts/dev-db-heal.sh`.** It lists ledger rows with no matching local migration file; `--fix` deletes exactly those (asks first; `--yes` skips the prompt; `--chat` targets `chat._sqlx_migrations`). It refuses when `APP_ENV=production` and refuses any `DATABASE_URL` whose host is not local or a compose service name — deliberately strict, better to refuse a legitimate dev URL than to touch prod.
- The dev reboot loop still works on a *local* database: `docker compose down -v` destroys everything, `./run.sh` recreates it (migrations + seeds). It is not available for prod.

## Ports

- Postgres: 5432. Control plane: 8080. Web (Vite): 5173, proxies `/api` to 8080.
- Mailpit: SMTP 1025, web inbox `http://localhost:8025` (dev email).

## Work model (Git-driven)

- **Git → Workspaces → Projects → Sessions.** A workspace is a repo; it can live on many nodes; each checkout (primary clone or **git worktree**) is a *location*. A **session** is a tmux-backed terminal running a chosen **runtime** in one checkout — runtimes are `bash`/`zsh`/`claude`/`hermes`/`codex`/… (sessions are NOT bash-only).
- **"+ New Work"** (top bar) is the unified entry: clone / new worktree / new empty project / existing workspace → pick node → pick runtime → session. Node selection is explicit.
- **Kanban drives work, control-plane authoritative.** The Board page has a **Backlog** tab (the refinement queue — backlog-type columns + epics, nothing runs from it) and a **Board** kanban tab (Todo · In Progress · In Review · Done). Backlog `dispatch` uses the resource-aware scheduler (`nook-dispatcher::pick_node`) to place work on the best node; start-work makes a worktree+session; submit-pr records a PR; prune removes the worktree. Endpoints: `/tasks/{id}/{dispatch,start-work,submit-pr,prune-worktree,move}` (`services/taskwork.rs`).
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
