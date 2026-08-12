# NookOS — Development Notes

## Dev loop — DOCKER FIRST

- **Everything runs in containers.** `docker compose up -d` (or `./run.sh` for a clean recreate) starts postgres, control plane, node, the **operator node** (the shared loop machine — MAIN-125/140; its first build is slow, then layer-cached; skip it with `--scale operator-node=0`), and web. Source is bind-mounted; **cargo watch runs INSIDE the control-plane and node containers** and rebuilds on save. Vite hot-reloads in the web container. Never run the services host-native.
- **A fresh worktree boots with `./scripts/dev-up.sh` and nothing hand-copied (MAIN-425).** A nook session gets the repo's TRACKED files only, and the stack needs two gitignored things: `.env` (compose refuses to start without it) and `deploy/dev-certs/agent.{crt,key}` (the control plane boots, then dies reading the key). `scripts/dev-bootstrap.sh` creates both, idempotently; `dev-up.sh` runs it and then `docker compose up -d`. `run.sh` calls the same bootstrap, so a fresh worktree and a reset primary checkout cannot drift.
  - **`dev-up.sh` only BOOTS; `run.sh` DESTROYS and reseeds.** Use `run.sh` to reset your own checkout, `dev-up.sh` for a second stack you want alongside it — `run.sh`'s `down -v` would take the shared volumes with it.
  - It derives `COMPOSE_PROJECT_NAME` from the directory, which is what stops two checkouts fighting over container names and volumes. Ports come from the environment (`.nook.toml`'s eleven), so a session's leases apply automatically and a plain clone gets the defaults.
  - **Both cert halves are generated per checkout and gitignored.** `agent.crt` used to be tracked while `agent.key` was not, which handed every fresh worktree a certificate it had no key for. Nothing pins the cert — the node computes its fingerprint at runtime — so a per-checkout pair is equivalent.
  - **Pulling that change DELETES your `agent.crt`** (MAIN-430). Git removes a tracked file an incoming commit deletes; it does not leave it behind as untracked. So an existing checkout ends up crt-gone/key-present — the mirror of the asymmetry above. **`./scripts/dev-bootstrap.sh` fixes it**, and regenerates BOTH halves whenever either is missing, so neither half can survive alone. `run.sh` and `dev-up.sh` call it, so only a bare `docker compose up -d` reproduces the boots-and-dies symptom.
- **Every service that bind-mounts the checkout runs as YOUR uid/gid (MAIN-537).** `docker-compose.yml` sets `user: "${NOOK_UID:-1000}:${NOOK_GID:-1000}"` and `dev-bootstrap.sh` records the real numbers in `.env`, so nothing a container writes into the tree is root-owned. That is not tidiness: a build worktree that collects root-owned files cannot be deleted by the node, which runs as an ordinary user, so every merged card leaked its 2–11G checkout forever. **A stack created before this change must be recreated** — `./run.sh`, or `docker compose down -v` — because its named volumes were initialized root-owned and a non-root container cannot write them. `down -v` does NOT reach the bind-mounted caches, so a checkout that ran the old stack also needs `sudo chown -R $(id -u):$(id -g) .cache .nook-secrets`; `dev-bootstrap.sh` detects that state and prints the command rather than taking the privilege itself.
- **A merged card's build worktree is pruned for you.** The card reaching Done brings its compose stack down and then removes the tree and its local branch (`git branch -d`, so unmerged work is refused by git); a node's ten-minute inventory does the same for trees whose card finished while nobody was listening. Every prune and every refusal is a comment on the card naming the reason and the space reclaimed. Nothing is pruned while a loop run is live, or for a card that has not finished — MAIN-480's persist-until-merge policy is unchanged.
- Edit → save → the container rebuilds automatically. Poll `http://localhost:8080/healthz` to know the control plane is back.
- `./scripts/dev-server.sh logs` tails the Rust services; `restart` force-restarts the control plane.
- **Dev email goes to Mailpit.** The dev control plane is wired `MAIL_PROVIDER=smtp` → `mailpit:1025`, so verification and invite emails land in a live inbox — read them at `http://localhost:8025`. Prod is unaffected (shipped default is `MAIL_PROVIDER` unset → `capture`, which delivers nothing).
- **The fleet's Claude login is a separate identity, mounted in (MAIN-238).** Loop jobs run `claude` on a node, and `nook-dispatcher` only places work on a node reporting the runtime **authorized** — so with no login a spec job sits queued forever with *"no eligible executor"*. The session lives in the gitignored `.nook-secrets/claude/`, mounted at `/nook-claude` with `CLAUDE_CONFIG_DIR` pointing at it (verified: `claude` 2.1.220 reads and writes its `.claude.json` there), on both the operator node and the dev node. `./run.sh` detects a missing session after the stack is healthy and offers the device login; `./run.sh --claude-login` runs it any time. **Subscription device-login only — never an API key.** Because the credentials are *only* in that mount, the fleet's account is separate from your own `~/.claude`: swap it with `rm -rf .nook-secrets/claude && ./run.sh --claude-login` without touching your personal login (the files used to be written by the container as root and need `sudo` on a stack that predates MAIN-537). An empty or absent dir is fine — the stack boots and the runtime simply reports `not_authorized`. Check with `docker compose exec operator-node claude auth status`.
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

## CLI shape — the top level is frozen (MAIN-157)

New `nook` commands land as `nook <plural-noun> <verb>`; the thirty existing flat
verbs are grandfathered and only shrink. Read `docs/cli-style.md` before adding
one — it also carries the skills-sweep rule (a renamed verb must update
`skills/` in the SAME ticket). `cli_surface` in `crates/nook-node/src/main.rs`
enforces it.

## Comments — the exception, not the default

Write code that does not need explaining, then explain only what code cannot say.
If a comment could be deleted without losing information, delete it.

- **Keep** — the non-obvious WHY: why this way and not the obvious way; a
  constraint you cannot see from here; the bug this shape prevents (name the
  card); an invariant a reader could otherwise break.
- **Cut** — anything the code already says: restating the line below, narrating
  the next step, section banners, doc comments that re-say the function name, a
  TODO with no owner.

A comment that can go stale is a liability — prefer a name, a type or a test,
which cannot. This tree is ~16% comment lines and much of that is narration
nobody now dares delete; do not add to it. Equally, this is not a licence to
strip existing comments: leave them unless you are changing that code.

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
- **A MODIFIED migration in a reused dev volume is a different failure, and the
  answer is to recreate the volume (MAIN-425).** A worktree directory keeps its
  pgdata across branch switches, so version N can have been applied from an
  abandoned branch whose `N_*.sql` differed; boot then dies with *"migration N
  was previously applied but has been modified"* — fatal in dev too, because the
  tolerance covers a MISSING version and never a changed one. `dev-db-heal.sh`
  now DETECTS this and refuses to touch it: deleting the row would re-apply N
  onto a schema that already has it, and the checksum is the only proof the
  schema matches the repo. `docker compose down -v && ./run.sh`, or for a second
  stack `COMPOSE_PROJECT_NAME=<its project> docker compose down -v`.
- **Heal the ledger with `scripts/dev-db-heal.sh`.** It lists ledger rows with no matching local migration file; `--fix` deletes exactly those (asks first; `--yes` skips the prompt; `--chat` targets `chat._sqlx_migrations`). It refuses when `APP_ENV=production` and refuses any `DATABASE_URL` whose host is not local or a compose service name — deliberately strict, better to refuse a legitimate dev URL than to touch prod.
- **The SQLite track's `0001` is HAND-OWNED and frozen (MAIN-236).**
  `crates/nook-control/migrations_sqlite/0001_init.sql` and nook-chat's twin were
  scaffolded once from the schema the Postgres migrations actually produce, then
  hand-corrected; the generator that made them was deleted in the same PR, on
  purpose — nothing regenerates over these files. **Forward changes are
  hand-authored SQLite deltas**: a Postgres `00NN_x.sql` gets a `migrations_sqlite/
  00NN_x.sql` twin written by hand, in the same commit. The type map is
  `docs/db-dialect-audit.md`'s (uuid/timestamptz/jsonb → `TEXT`, `now()` →
  `nook_db::sqlite_time::NOW_SQL`, `::` casts stripped, `= ANY (ARRAY[…])` →
  `IN (…)`), and `crates/nook-control/tests/sqlite_scaffold.rs` proves an empty
  SQLite database still builds from them. Boot wiring and the both-engines
  divergence guard are MAIN-196's, not here.
- **A number is claimed on BOTH tracks at once: the next one free is the highest
  either set uses, plus one.** Counting from Postgres alone is what breaks.
  MAIN-502 took the next free Postgres number (0056) for both halves, exactly as
  the rule above reads, and died on `UNIQUE constraint failed:
  _sqlx_migrations.version` — SQLite's 0056 was already taken. When one engine
  needs a **solo** migration the other **skips** that number rather than
  shifting its own twins: that is what 0038 did (`settings_null_equating_unique`
  is SQLite-only and there is no Postgres 0038), and why 0039–0054 are still
  paired number-for-number. 0055 departed from it — a SQLite-only change took
  0055 on its own track while the shared twin landed at pg-0055 / sqlite-0056 —
  which is the one-number offset MAIN-502 hit. Nothing already applied is ever
  renumbered to correct it; lockstep returns as soon as a paired migration takes
  the same free-on-both number again, which is **0057** as of 2026-08-10.
- **A timestamp on the SQLite track has ONE form, and it is not
  `CURRENT_TIMESTAMP` (MAIN-442).** A `timestamptz` is TEXT there, so text
  comparison IS the comparison: `CURRENT_TIMESTAMP`'s
  `2026-08-06 13:28:36` neither equalled nor ordered against the RFC 3339 a
  bound `DateTime<Utc>` used to encode as, which made every optimistic-
  concurrency guard match 0 rows and made two rows written in one second
  indistinguishable. Both halves now render `nook_db::sqlite_time`'s
  `strftime('%Y-%m-%d %H:%M:%f','now')`; `0055_sqlite_timestamp_form.sql` put it
  on every column default, and `sqlite_boot.rs` fails the build if a new one
  arrives spelled any other way. Milliseconds is SQLite's clock resolution, and
  both halves must render the same width — so a bound instant is truncated to
  ms on this engine, and only on this engine.
- The dev reboot loop still works on a *local* database: `docker compose down -v` destroys everything, `./run.sh` recreates it (migrations + seeds). It is not available for prod.

## Panics are caught, logged, and never drop a connection (MAIN-273)

- Both services wrap their router in `tower_http`'s `CatchPanicLayer`, **inside**
  the trace layer, from the shared `nook-errors` crate — one implementation, so
  the two services cannot drift (the reason `nook-auth` exists). `nook-errors`
  is named for the whole job: MAIN-274 hoists the shared `ApiError` there to
  retire nook-chat's `ChatError`, and the panic net belongs beside it because
  its entire contract is emitting the *same* body the error mapping does. A panic in a handler or extractor becomes a clean `500`
  with the ordinary `{"error":"internal error"}` body — the same one
  `ApiError::Internal` returns — so a client cannot tell a panic from any other
  internal error and no panic detail leaks.
- The detail goes to the log instead: one `tracing::error!` carrying the panic
  message, its source location, and a backtrace. Location reaches only the panic
  hook, so `panics::install_panic_hook()` runs once at boot in each service and
  stashes it for the layer; it chains to the previous hook, so a panic in a
  background task still prints exactly as it did before.
- **Backtraces are opt-in.** The capture honours `RUST_BACKTRACE`: unset, the
  record reads `backtrace=disabled (set RUST_BACKTRACE=1)` and costs nothing.
  Set `RUST_BACKTRACE=1` on the service (compose `environment:` or the pod spec)
  when you need frames — it is deliberately not forced on, because capturing on
  a hot panic path is expensive and that is an operator's call.

## Ports

- Postgres: 5432. Control plane: 8080. Web (Vite): 5173, proxies `/api` to 8080.
- Mailpit: SMTP 1025, web inbox `http://localhost:8025` (dev email).
- **Those are DEFAULTS, not literals (MAIN-376).** Every host port compose
  publishes is `${VAR:-<the number above>}`, and `.nook.toml` declares one
  listener per variable — so a nook session leases its own set and two
  checkouts of this repo run at once instead of both grabbing 8080. Unset
  (a plain clone, no nook) is byte-for-byte the old behaviour, which is what
  keeps a local dev run working outside a session.
- Changing one by hand is `NOOK_WEB_PORT=5273 docker compose up`; the full list
  of variables is `.nook.toml`, and it is the file to edit when a service
  starts publishing a new port. A bare `- "1234:1234"` in `docker-compose.yml`
  now fails `./test.sh` — the declaration and the compose file are checked
  against each other, in both directions.
- **All eleven are `required = true`, and that caps sessions per node.** Every
  session in this repo leases eleven ports — a shell and an agent as much as the
  stack — so a node advertising 100 (the dev node's `4200-4299`) supports NINE
  concurrent sessions, and the tenth is refused by name rather than started.
  That is the deliberate trade: compose cannot distinguish "unset because plain
  clone" from "unset because the node ran out", so `required = false` let a
  half-leased session start and collide on the very literals this replaced.
  Widen the node's range if nine is tight.
- **Leased ports are baked in at session creation** (`tmux new-session -e`).
  Editing a workspace's declaration does not re-lease and does not reach a
  RUNNING session — kill it and start a new one to pick up a change.

## Boot → test the loop, in one step (MAIN-341)

A clean `./run.sh` now lands in a state the agent loop can actually be exercised
in, instead of one that needs ~10 manual steps first:

- A **real bare git repo** on the operator node (`/workspace/nook-dogfood.git`),
  created idempotently by `run.sh`. Local path, so a loop job clones it with no
  ssh key, no credential and no network.
- A seeded **`nook-dogfood` workspace** pointing at that path, in the SAME tenant
  the operator node joins — which is what stops "no eligible executor".
- A seeded **dev identity** (`dev@nookos.local`, owner) in that same tenant, so
  signing in through the dev-login hatch lands you beside the operator rather
  than in a fresh personal tenant.
- **`loops.enabled = true`** for that tenant only. Off remains the shipped
  default everywhere else.
- A **ready ticket** — *"Add a greeting command to the dogfood repo"* — in Todo,
  linked to the workspace.

So the path is: `./run.sh` → **`./run.sh --claude-login` once** → open the
ticket's `/loop` page → **Draft a spec**.

**The one manual step is the Claude login, and that is on purpose.** No
credential is seeded, baked, or automated — not the runtime auth, not the CLI
token. `run.sh` mints the dev CLI token by *logging in* through the dev-login
hatch (gated on `AUTH_DEV_MODE`, refused in production) and handing it to `nook
login`, so `contexts.toml` is valid after every reseed without a secret ever
being committed. The Claude session lives in `.nook-secrets/claude`, survives
`docker compose down -v`, and is genuinely a one-time login.

The synthetic Mission Control demo (`example/widgets` on `demo-box`) is
untouched and still separate: it exists to make the UI look populated, with a
fake remote on a node that never reports. The dogfood workspace is the opposite
— everything about it is real. Do not merge the two.

## Loops are OFF by default (MAIN-239)

- The control plane's job machinery — `job_dispatch`, `job_reaper`,
  `workspace_reaper` — is gated on one tenant-scoped setting, `loops.enabled`,
  **default off**. A fresh boot therefore does no polling, no dispatch, and no
  reaping; the operator node still starts, it just never claims loop work.
- Turn it on with `nook operator loops on` (or Settings → Loops). It is read on
  **every poll**, so the change lands within a poll interval with **no restart**;
  the log says `loops enabled — resuming` / `loops disabled — idle` once per
  transition, not once per tick.
- **Off loses nothing.** A job created while loops are off stays `queued`,
  unplaced, and is picked up when the switch flips. If a promoted ticket is not
  moving, this switch is the first thing to check.

## Work model (Git-driven)

- **Git → Workspaces → Projects → Sessions.** A workspace is a repo; it can live on many nodes; each checkout (primary clone or **git worktree**) is a *location*. A **session** is a tmux-backed terminal running a chosen **runtime** in one checkout — runtimes are `bash`/`zsh`/`claude`/`hermes`/`codex`/… (sessions are NOT bash-only).
- **"+ New Workspace"** (top bar) is the unified entry: clone / new worktree / new empty project / existing workspace → pick node → pick runtime → session. Node selection is explicit.
- **Kanban drives work, control-plane authoritative.** The Board page has a **Backlog** tab (the refinement queue — backlog-type columns + epics, nothing runs from it) and a **Board** kanban tab (Todo · In Progress · In Review · Done). Backlog `dispatch` uses the resource-aware scheduler (`nook-dispatcher::pick_node`) to place work on the best node; start-work makes a worktree+session; submit-pr records a PR; prune removes the worktree. Endpoints: `/tasks/{id}/{dispatch,start-work,submit-pr,prune-worktree,move}` (`services/taskwork.rs`).
- **Nodes report live resources** each heartbeat (`NodeResources`: cpu/mem/load/sessions) → `nodes.resources` + `UiEvent::NodeResources` → capacity bars in the UI; feeds triage.
- **MCP parity**: git + kanban management is fully drivable from `/mcp` (clone/create_project/add_worktree/dispatch/start_work/move/submit_pr). Joining nodes stays human-only.
- **Ports are LEASED, and the WORKSPACE declares what it needs (MAIN-301).** A node advertises a range with `NOOK_PORT_RANGE=start-end` (the dev stack: operator `4100-4199`, node `4200-4299`). A workspace declares zero or more named listeners — `{name, env, protocol, required}` — and the control plane leases one port per listener from the node's range, delivering each into the session as the env var **that workspace named**. **An app in a session binds `$PORT` / `$API_PORT` / whatever it declared, never a literal** — that is the convention, and it is what lets two worktrees of one app run side by side instead of fighting over 3000.
  - **The control plane never learns what `PORT` means.** It allocates numbers; the repo says which variable each number lands in. That is what lets a Next.js app, an ASP.NET service and a Rust backend all lease from one node without a change here. Set the declaration with `PUT /api/v1/workspaces/{id}/ports`.
  - **Undeclared is not the same as declaring none.** A workspace with no declaration gets one optional listener on `NOOK_PORT` — the zero-config default, held as data rather than as a branch in the allocator. A workspace declaring an EMPTY list is saying "this repo binds nothing", and gets nothing.
  - `required: true` makes an unsatisfiable listener fail the session start, loudly; `required: false` (the default) skips it. A `debug` port going unleased should not stop the app; the app's own port going unleased must not start a session that then collides on a hardcoded default.
  - **Reclaim is lazy and there is no release path.** Nothing frees a lease when a session ends, is killed or is reaped — the allocator drops the rows of non-live sessions on the node as its first step, so a dead session's ports come back at the moment somebody needs one. There is no cleanup path to forget.
  - The range and the live leases are on the Nodes page: an owner can retune the range or release a stuck lease there, without a shell on the machine.
  - Unset range means the node leases nothing, deliberately — a guessed range would hand out ports something else is already listening on. A session on such a node simply gets no variables, which is fine: not every session runs a server.
  - This supersedes the macOS `lo0`-alias stopgap on Linux/WSL. Nice URLs (a reverse proxy in front of a leased port) are still future work.
- **A node's LOOP CAPACITY is settable the same way, and the central value WINS (MAIN-508).** How many loop jobs a machine holds at once was env-only (`NOOK_MAX_LOOP_JOBS`, default 2), so changing it meant editing a unit file as root and `systemctl restart nook-node` — the one operation that strands every in-flight streaming build. It is now the port range's twin: `PUT /api/v1/nodes/{id}/capacity`, `nook set capacity azul 4`, and the Nodes page.
  - **Precedence, highest first: the host's pin → the central value → what the node advertises → `CAPACITY_WHEN_UNREPORTED`.** Central beating the env is the feature, not an accident: the machine needing a retune is exactly the one whose unit file already names a number, so an env-wins rule would have left the restart in place and changed nothing.
  - **A host that must decide locally sets `NOOK_MAX_LOOP_JOBS_PINNED=1`**, and the central write is then REFUSED by name rather than stored and quietly overruled. Deliberately a second variable: "2 jobs" and "and nobody else may say otherwise" are two statements, and every existing machine already makes the first.
  - **Nothing restarts and nothing is signalled to the node.** Placement resolves the number from the stored row on every attempt, so a change lands at the next dispatch poll with running work untouched — which is the whole point.
  - **`0` is a cordon at every level**: stop claiming, finish what you hold. It is a different statement from unset, which is why the column is nullable and has no default.
  - `nook get nodes` prints the effective number and its source, so *"why is only one thing building"* is answerable without a shell on the box.
- **WHICH queued job gets a freed executor is a rule, not a race (MAIN-509).** Capacity says how many run; this says who runs next. The rule is **card priority, then how long the job has waited** — `1` urgent … `4` low, with `0` unset sorting LAST (the board's own convention, so this cannot invert what a human set) and a review run, which has no card, counting as unset. `created_at` then job id break the remaining ties, so every replica applies one total order.
  - **A queue delivery is an OCCASION to dispatch, not an instruction to place the job it names.** It used to be the latter, and that made placement accidental LIFO: an unplaceable job is re-armed after 30s while a freshly raised one is delivered at once, so whichever item happened to be in the queue when a slot freed won — systematically the newest, because a run concluding is exactly what raises the next card. `!!` jobs sat for hours behind a `↑` one. Now a delivery runs `jobs::place_queued_in_order` over the tenant's whole queued set, so the new job's early arrival hands the slot to the job that most deserves it. Deliveries received together are ONE pass per tenant.
  - **The pass does not stop at the first refusal.** A job can be unplaceable for a reason of its own — a worktree pin on a dark node, a label nothing wears — and the jobs behind it must not inherit that wait. Each attempt re-reads the node's in-flight count, so capacity still stops the pass where it should, and one pass fills every slot that is free.
  - This is fairness, not FIFO: an urgent hotfix raised seconds ago still goes ahead of a backlog it outranks. It also does not replace MAIN-496's starvation escalation — that remains the net for a job nothing can ever place.

## UI direction

- **Full-screen application, not a web page.** Use ALL the real estate: edge-to-edge panels, dense information layout, split panes, thin borders. No hero sections, no max-width containers, no generous padding/whitespace.
- Feels **native to the OS** it runs on — window chrome-style top bar with app-section tabs (Dashboard, Workspaces, Board, Activity, …), persistent status strip, panel-based layout like a terminal multiplexer / mission-control console.
- Amber-CRT default theme (PLAN.md): near-black background, amber primary, monospace, subtle glow. Compact type scale (12–13px base in panels).

- Rust owns the types (`nook-types`); regen TS with `./scripts/gen-types.sh` after changing API types. Generated `schema.d.ts` is committed.
- `.env` values with spaces must be quoted (dotenvy stops parsing at the first malformed line).
- No provider-specific auth code — OIDC config lives only in `.env`.
