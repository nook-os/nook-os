# NookOS

> **The open workspace operating system for modern software teams.**

NookOS is a self-hosted control plane for development work. It doesn't replace your editor, Git, or your AI — it coordinates everything around them: machines, workspaces, tmux-backed sessions, AI runtimes, kanban, and activity, in one place. See [PLAN.md](PLAN.md) for the full vision.

Apache-2.0. Self-hosted. No telemetry, no account, no phone-home.

---

## The part worth reading first

**A remote AI session is just a shell you can type into.**

From any machine, start Claude Code (or Codex, or Hermes, or plain `bash`) on *another* machine and hold a conversation with it. No ssh, no hostname, no port forwarding, no tmux incantation:

```bash
nook start my-repo --node buildbox --runtime claude --name refactor-auth
nook exec refactor-auth 'add retries to the HTTP client, then run the tests'
```

Real output, one machine driving Claude on another:

```
── fleet-proof · runtime=claude · status=running ──
▐▛███▜▌   Claude Code v2.1.216
▝▜█████▛▘  Opus 4.8 (1M context)

❯ In one short sentence: what machine are you running on and what is this repo?
● You're on a WSL2 Linux box (~/.nook/workspace/nook-os/nook-os), and this repo
  is NookOS — a Rust + React system for managing git workspaces, worktrees, and
  tmux-backed terminal sessions across multiple nodes.
```

Four verbs are the whole protocol:

| | |
|---|---|
| `nook start <workspace> --runtime claude` | open a session on any machine in the fleet |
| `nook send <session> '…'` | type into it |
| `nook read <session>` | see what it's showing right now |
| `nook exec <session> '…'` | type, wait for the reply, print it |

Sessions are **persistent** — tmux-backed on the node. They survive your laptop closing, the network dropping, and the node agent restarting. Come back hours later and `nook read` still works.

`nook read` prints `runtime=claude` in its header, so a script (or an agent) knows whether it's talking to an AI or a shell before it types.

### Agents can do this too

NookOS ships as a skill, so an AI agent on one machine can drive Claude Code on another:

```bash
./skills/install.sh --host <machine>    # installs into ~/.hermes/skills + every agent profile
```

See [`skills/nookos/SKILL.md`](skills/nookos/SKILL.md) — every command and error message in it was executed against a live fleet and pasted verbatim. This is what makes cross-machine agent work ordinary: one agent, one CLI, any machine you own.

---

## Quickstart (Docker first)

```bash
cp .env.example .env    # point OIDC_* at your IdP, or keep AUTH_DEV_MODE=true
./run.sh                # destroys and recreates the entire dev environment
```

Open **http://localhost:5173** and sign in with your identity provider (or the dev sign-in). The stack comes up with a containerized node (`dev-node`) that already has demo workspaces discovered.

Everything runs in containers with the source bind-mounted: `cargo watch` rebuilds the control plane and node on save; Vite hot-reloads the web app. `docker compose down -v` destroys everything; `./run.sh` brings it all back identically.

## Starting work

Hit **+ New Work** (top bar): clone a repo (GitHub/GitLab/Bitbucket/raw, optional SSH key for private), add a git worktree, spin up a new empty project, or open an existing workspace — then pick the machine and the **runtime** (`bash`/`zsh`/`claude`/`hermes`/`codex` — a session runs whatever you choose, not just a shell) and go. The work is the unit; the machine is where it runs.

**Kanban drives work**, control-plane authoritative. A task flows **Triage → Todo → In Progress → Done**: *dispatch* lets the resource-aware scheduler pick the best online node (or you pick), *start work* creates a worktree + session, *submit PR* records the PR, *prune* removes the worktree. Nodes report live CPU/memory/load/session capacity (bars on the Nodes page) so you can see what can take the workload.

## Add a machine

In the UI: **Nodes → + add node**, then on that machine:

```bash
curl -fsSL https://<your-nook-host>/install.sh | sh -s -- --token nook_join_…
```

Nodes connect **outbound** over WebSocket — no inbound SSH, no public ports, nothing to expose. The node reports its own capabilities (CPU, GPU, docker, tmux, git, installed runtimes like `claude`) and discovers git repositories under its workspace roots. Workspaces — not machines — are the unit you think in; one workspace can exist on many nodes.

## For AI clients (MCP)

```text
endpoint:  http://localhost:8080/mcp   (streamable HTTP)
auth:      Authorization: Bearer $MCP_TOKEN   (from .env)
tools:     list_workspaces · list_nodes · list_sessions · start_session ·
           send_to_session · get_activity · get_notes · append_note · create_task
```

Git and kanban management are fully drivable from MCP: clone, create project, add worktree, dispatch, start work, move, submit PR. Joining new machines stays human-only, deliberately.

---

## How it's built (and why)

The decisions that aren't obvious from the file tree:

- **Two credential types, and the machine one is deliberately weak.** A node token authenticates the machine it sits on, and the control plane *confines it to that machine* — one compromised box must not become every box. Driving other machines requires a user token (`nook login`). Enforced on every session route, covered by tests.
- **Tenant is the isolation boundary.** Every new sign-in provisions its own tenant, and there is no configuration flag that puts two people in one; a setting that can silently expose one person's machines to another is a leak waiting for a bad afternoon. Sharing is an explicit membership grant.
- **Nodes dial out.** The agent holds a WebSocket to the control plane, so adding a machine never means opening a port or managing inbound SSH.
- **Rust owns the types.** Domain types live in `nook-types`, OpenAPI is generated from the code, TypeScript is generated from OpenAPI (`./scripts/gen-types.sh`). CI fails on drift, so the client cannot quietly disagree with the server.
- **Migrations are append-only.** Once applied to a database holding real data, a migration is frozen. The recorded checksum is what proves the schema in front of you is the schema the repo describes; rewriting it would make that proof say "verified" without anything having been verified.
- **`exec` waits for quiet, not for a timer.** Polling until the screen stops changing is the only honest way to know an agent has finished — thinking time is unpredictable, so a fixed `sleep` either truncates the answer or wastes minutes.
- **Encryption at rest is real.** Workspace secrets are sealed under a user-held app password (PBKDF2 → AES-256-GCM) *before* they reach the database, so a dump — even with the server's own key — reveals nothing. NookOS cannot recover it for you, and says so plainly before you set it.
- **Multi-instance from the start.** Node ownership is a lease in Postgres and cross-instance traffic rides LISTEN/NOTIFY, so a second control plane is a scaling decision rather than a rewrite.

## Known limitations

Written down because failure modes matter more than a feature list:

- **Port collisions between worktrees.** Several worktrees of the same app on one machine contend for ports (443/3000). No automatic fix yet — macOS `lo0` aliases work; a Linux/WSL reverse proxy is future work.
- **Kanban is local-first.** Jira/GitHub/Linear/Trello federation sits behind a trait but isn't implemented.
- **The AI dispatcher recommends, never acts.** It picks a node; it does not decide what work to do.
- **macOS binaries must be built on a Mac.** Apple's SDK licence forbids cross-compiling from Linux, so ARM Mac builds are published with `nook publish` rather than produced by CI.
- **Single-region assumptions.** The bus is Postgres LISTEN/NOTIFY — fine for a fleet, not for a globe.

## What works today (milestone 1)

- **Generic OIDC login** (any standards-compliant IdP; authorization code + PKCE; configured only via `.env`)
- **Multi-tenant control plane** — Rust / Axum / SQLx / Postgres
- **Node agent** with capability detection, join-token enrollment, reconnect/backoff, workspace discovery
- **Persistent terminal sessions** — tmux-backed, streamed to xterm.js; survive refreshes, reconnects and node restarts
- **Cross-machine session control** — `start` / `send` / `read` / `exec` from any machine, plus personal access tokens
- **Local kanban** (drag & drop) behind a federation trait
- **Activity timeline** — everything produces events, streamed live over WebSocket
- **Encrypted workspace secrets** with passkey unlock
- **Theme engine** with the built-in amber-CRT mission-control theme
- **AI dispatcher** (rule-based) and an **MCP server** at `/mcp`
- **Tauri desktop shell** wrapping the same app

## Layout

```
crates/
  nook-types        domain types (single source of truth → OpenAPI → TS)
  nook-proto        node ⇄ control-plane WebSocket protocol
  nook-control      control plane: auth, REST, WS, seeds, MCP mount
  nook-node         the `nook` agent + CLI: join/run, tmux/PTY, discovery
  nook-dispatcher   AI dispatcher trait + rule-based backend
  nook-mcp          MCP tool surface (backend trait keeps deps acyclic)
  nook-openapi-gen  emits openapi.json without starting a server
frontend/
  packages/api      generated TS types + typed fetch/WS client
  packages/ui       theme engine, terminal view, components
  packages/app      routes/pages (render-target agnostic)
  apps/web          Vite host  ·  apps/desktop  Tauri 2 shell
skills/             agent skills (Hermes format, readable by any agent)
```

## Development notes

- Schema changes are **new** numbered migrations; `0001_init.sql` is frozen.
- After changing API types: `./scripts/gen-types.sh` (CI fails on drift).
- `scripts/dev-server.sh logs|restart` tails or bounces the containerized services.
- CI runs `cargo fmt --all --check && cargo clippy --workspace --all-targets && cargo test --workspace` against a real Postgres; database-backed tests fail rather than skip when `NOOK_REQUIRE_DB` is set.

## License

Apache-2.0.

---

## Who built this

**Ryan Hein** — 25 years building software, most of it on the parts nobody demos: distributed systems, identity, and the infrastructure other engineers quietly depend on.

NookOS came out of a practical problem. Work spreads across machines — a laptop, a workstation, a build box, a server under a desk — and AI agents made that worse rather than better: every agent is stranded on whichever machine you happened to start it on. The available answer was ssh, tmux, and remembering hostnames. This is the answer I wanted instead.

It is free and open source and will stay that way. A managed service is coming for people who would rather not run the control plane themselves.

Issues and PRs welcome — particularly from anyone who has felt the port-collision problem above.
