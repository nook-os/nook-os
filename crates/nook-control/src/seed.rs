//! Idempotent dev seeds. `docker compose down -v` destroys everything; this
//! brings the same predictable environment back on every reboot.

use anyhow::Result;
use nook_db::{params, Db, DbPool};
use nook_types::*;

use crate::config::Config;

/// The token hash lives in `nook-auth` now, so the control plane and chat hash
/// identically (MAIN-48). Re-exported here because 28 call sites import it as
/// `crate::seed::hash_token`.
pub use nook_auth::hash_token;

/// Built-in amber CRT theme: hacker-terminal mission control. Seeded with
/// tenant NULL so every tenant sees it.
pub fn amber_crt_tokens() -> serde_json::Value {
    serde_json::json!({
        "colors": {
            "bg": "#0a0705",
            "bg-panel": "#120d08",
            "bg-raised": "#1a130b",
            "fg": "#ffb000",
            "fg-bright": "#ffd75f",
            "fg-dim": "#8a6a1f",
            "fg-faint": "#4a3a12",
            "accent": "#ffb000",
            "border": "#33260e",
            "border-bright": "#5a4415",
            "ok": "#4be36e",
            "warn": "#ffcc00",
            "err": "#ff4d4d",
            "info": "#4dc3ff",
            "selection": "#3a2c0c",
            "terminal-bg": "#0a0705",
            "terminal-cursor": "#ffb000"
        },
        "fonts": {
            "mono": "'JetBrains Mono', 'IBM Plex Mono', 'Fira Code', ui-monospace, monospace",
            "ui": "'JetBrains Mono', 'IBM Plex Mono', ui-monospace, monospace"
        },
        "spacing": {
            "unit": "4px",
            "panel-gap": "1px",
            "radius": "3px"
        },
        "effects": {
            "glow": "0 0 6px rgba(255, 176, 0, 0.35)",
            "glow-strong": "0 0 10px rgba(255, 176, 0, 0.55)",
            "scanlines": "off"
        }
    })
}

/// Built-in "Charcoal Gold" theme: charcoal surfaces, golden pills, teal
/// accents, rounded corners — a coherent mission-control look that keeps a
/// terminal soul (monospace UI, prompt-style chrome).
pub fn charcoal_gold_tokens() -> serde_json::Value {
    serde_json::json!({
        "colors": {
            "bg": "#0e1012",
            "bg-panel": "#16181b",
            "bg-raised": "#1d2024",
            "fg": "#d8d5cf",
            "fg-bright": "#ffffff",
            "fg-dim": "#8a8f98",
            "fg-faint": "#4b5058",
            "accent": "#f5b301",
            "border": "#26292e",
            "border-bright": "#3a3f46",
            "ok": "#2dd4a7",
            "warn": "#f5b301",
            "err": "#ff5c5c",
            "info": "#58a6ff",
            "selection": "#2c2f35",
            "terminal-bg": "#101214",
            "terminal-cursor": "#f5b301"
        },
        "fonts": {
            "mono": "'JetBrains Mono', 'IBM Plex Mono', 'Fira Code', ui-monospace, monospace",
            "ui": "'JetBrains Mono', 'IBM Plex Mono', ui-monospace, monospace"
        },
        "spacing": {
            "unit": "4px",
            "panel-gap": "1px",
            "radius": "8px"
        },
        "effects": {
            "glow": "0 0 8px rgba(245, 179, 1, 0.22)",
            "glow-strong": "0 0 12px rgba(245, 179, 1, 0.4)",
            "scanlines": "off"
        }
    })
}

/// A built-in palette. Every theme keeps the same dense, monospace,
/// terminal-native design — only the colors differ.
struct Palette {
    bg: &'static str,
    bg_panel: &'static str,
    bg_raised: &'static str,
    border: &'static str,
    border_bright: &'static str,
    fg: &'static str,
    fg_bright: &'static str,
    fg_dim: &'static str,
    fg_faint: &'static str,
    accent: &'static str,
    accent_dim: &'static str,
    ok: &'static str,
    warn: &'static str,
    err: &'static str,
    info: &'static str,
    selection: &'static str,
    terminal_bg: &'static str,
    radius: &'static str,
    glow: &'static str,
}

impl Palette {
    fn tokens(&self) -> serde_json::Value {
        serde_json::json!({
            "colors": {
                "bg": self.bg, "bg-panel": self.bg_panel, "bg-raised": self.bg_raised,
                "border": self.border, "border-bright": self.border_bright,
                "fg": self.fg, "fg-bright": self.fg_bright,
                "fg-dim": self.fg_dim, "fg-faint": self.fg_faint,
                "accent": self.accent, "accent-dim": self.accent_dim,
                "ok": self.ok, "warn": self.warn, "err": self.err, "info": self.info,
                "selection": self.selection,
                "terminal-bg": self.terminal_bg, "terminal-cursor": self.accent
            },
            "radius": self.radius,
            "effects": { "glow-strong": self.glow, "scanlines": "off" }
        })
    }
}

/// Nord-inspired cool blues — calm, low-contrast, easy for long sessions.
fn nordic_tokens() -> serde_json::Value {
    Palette {
        bg: "#2e3440",
        bg_panel: "#2b303b",
        bg_raised: "#3b4252",
        border: "#3b4252",
        border_bright: "#4c566a",
        fg: "#d8dee9",
        fg_bright: "#eceff4",
        fg_dim: "#a9b1c1",
        fg_faint: "#6b7488",
        accent: "#88c0d0",
        accent_dim: "#5e81ac",
        ok: "#a3be8c",
        warn: "#ebcb8b",
        err: "#bf616a",
        info: "#81a1c1",
        selection: "#3b4252",
        terminal_bg: "#272c36",
        radius: "5px",
        glow: "0 0 12px rgba(136, 192, 208, 0.35)",
    }
    .tokens()
}

/// Solarized-inspired warm dark — muted, high legibility.
fn deep_teal_tokens() -> serde_json::Value {
    Palette {
        bg: "#002b36",
        bg_panel: "#01313d",
        bg_raised: "#073642",
        border: "#073642",
        border_bright: "#0f4b58",
        fg: "#93a1a1",
        fg_bright: "#eee8d5",
        fg_dim: "#839496",
        fg_faint: "#586e75",
        accent: "#2aa198",
        accent_dim: "#268bd2",
        ok: "#859900",
        warn: "#b58900",
        err: "#dc322f",
        info: "#268bd2",
        selection: "#073642",
        terminal_bg: "#00252e",
        radius: "5px",
        glow: "0 0 12px rgba(42, 161, 152, 0.35)",
    }
    .tokens()
}

/// Near-black with a synthwave magenta accent.
fn synth_magenta_tokens() -> serde_json::Value {
    Palette {
        bg: "#0d0b14",
        bg_panel: "#141020",
        bg_raised: "#1e1830",
        border: "#241d38",
        border_bright: "#3a2f57",
        fg: "#d7d0e8",
        fg_bright: "#ffffff",
        fg_dim: "#a79dbd",
        fg_faint: "#6f6688",
        accent: "#ff5fd2",
        accent_dim: "#c93fa6",
        ok: "#4ade80",
        warn: "#fbbf24",
        err: "#fb7185",
        info: "#7dd3fc",
        selection: "#3a2f57",
        terminal_bg: "#0a0810",
        radius: "6px",
        glow: "0 0 14px rgba(255, 95, 210, 0.4)",
    }
    .tokens()
}

/// Matrix green on black — the classic.
fn phosphor_green_tokens() -> serde_json::Value {
    Palette {
        bg: "#050705",
        bg_panel: "#0a0f0a",
        bg_raised: "#0f170f",
        border: "#142014",
        border_bright: "#1d3320",
        fg: "#8fdf8f",
        fg_bright: "#d6ffd6",
        fg_dim: "#6fae6f",
        fg_faint: "#3f6b43",
        accent: "#39ff5f",
        accent_dim: "#22c04a",
        ok: "#39ff5f",
        warn: "#e3d24a",
        err: "#ff5555",
        info: "#5fd7ff",
        selection: "#14301a",
        terminal_bg: "#020402",
        radius: "2px",
        glow: "0 0 12px rgba(57, 255, 95, 0.4)",
    }
    .tokens()
}

/// Paper-light for bright rooms.
fn daylight_tokens() -> serde_json::Value {
    Palette {
        bg: "#f6f6f2",
        bg_panel: "#eeeee8",
        bg_raised: "#e4e4dc",
        border: "#dadad0",
        border_bright: "#c2c2b4",
        fg: "#33332e",
        fg_bright: "#111110",
        fg_dim: "#5a5a52",
        fg_faint: "#8c8c80",
        accent: "#b06500",
        accent_dim: "#8a4f00",
        ok: "#2f7d32",
        warn: "#a86800",
        err: "#c62828",
        info: "#0369a1",
        selection: "#e6dcc8",
        terminal_bg: "#f2f1ea",
        radius: "5px",
        glow: "0 0 10px rgba(176, 101, 0, 0.18)",
    }
    .tokens()
}

/// Where the dogfood repo lives on the operator node (MAIN-341).
///
/// `run.sh` creates the bare repo at exactly this path inside the operator
/// container, and the seeded workspace points at it. The two MUST agree, so the
/// path is written once here and referenced from the script by this name.
pub const DOGFOOD_REPO_PATH: &str = "/workspace/nook-dogfood.git";

pub async fn run(db: &DbPool, cfg: &Config) -> Result<()> {
    // Seeding has no `AppState`, so it builds the one repository it needs from
    // the pool it was handed (MAIN-304). `seed.rs` is a permanent inline-SQL
    // exemption for its own bootstrap rows; the activity events it records are
    // not bootstrap data, so they go through the read model like everyone
    // else's.
    let events_repo = crate::repo::read_model::DbReadModelRepository::new(db.clone());

    // Built-in themes (always seeded, all environments).
    for (name, slug, tokens) in [
        ("Charcoal Gold", "charcoal-gold", charcoal_gold_tokens()),
        ("Amber CRT", "amber-crt", amber_crt_tokens()),
        ("Nordic", "nordic", nordic_tokens()),
        ("Deep Teal", "deep-teal", deep_teal_tokens()),
        ("Synth Magenta", "synth-magenta", synth_magenta_tokens()),
        ("Phosphor Green", "phosphor-green", phosphor_green_tokens()),
        ("Daylight", "daylight", daylight_tokens()),
    ] {
        db.exec(
            "INSERT INTO themes (id, tenant_id, name, slug, tokens)
             VALUES ($1, NULL, $2, $3, $4)
             ON CONFLICT (slug) DO UPDATE SET tokens = EXCLUDED.tokens",
            params![ThemeId::new(), name, slug, tokens],
        )
        .await?;
    }

    // Managed fleet content — the `nookos` skill and hook set (MAIN-78). Built-in
    // content, so seeded in every environment (like themes) and before the dev
    // gate. Idempotent: refreshes only when the shipped default changed, never
    // clobbering an operator edit.
    crate::routes::managed::seed(&crate::repo::admin::DbManagedContentRepository::new(
        db.clone(),
    ))
    .await?;

    if cfg.is_production() {
        tracing::info!("seed: built-in themes only (production)");
        return Ok(());
    }

    // Dev tenant — adopted (as owner) by the first identity that logs in.
    let slug = crate::services::identity::slugify(&cfg.default_tenant_name);
    let tenant: Tenant = match db
        .query_opt::<Tenant>("SELECT * FROM tenants WHERE slug = $1", params![&slug])
        .await?
    {
        Some(t) => t,
        None => {
            db.query_one(
                "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3) RETURNING *",
                params![TenantId::new(), &cfg.default_tenant_name, &slug],
            )
            .await?
        }
    };

    // Well-known join token so the compose node can auto-join on boot.
    if let Some(token) = &cfg.dev_join_token {
        db.exec(
            &format!(
                "INSERT INTO join_tokens (id, tenant_id, token_hash, name, expires_at)
             VALUES ($1, $2, $3, 'dev auto-join', {expiry})
             ON CONFLICT (token_hash) DO NOTHING",
                // Engine-selected (MAIN-196): SQLite has no `now() + interval`.
                expiry = nook_db::dialect::time_math(db.engine()).now_plus("10 years")
            ),
            params![JoinTokenId::new(), tenant.id, hash_token(token)],
        )
        .await?;
    }

    // The two labels the agent loop is built around, seeded per tenant.
    //
    // `agent-ready` is the human approval gate — the signal that a person has
    // looked at a task and is willing for an agent to take it. It exists here
    // rather than being created on first use so that the gate is present and
    // visible from the very first board, instead of appearing only once
    // somebody already needed it.
    for (name, color) in [("agent-ready", "#48c78e"), ("blocked", "#f14668")] {
        db.exec(
            "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, name) DO NOTHING",
            params![uuid::Uuid::now_v7(), tenant.id, name, color],
        )
        .await?;
    }

    // Sample local board with a few tasks.
    let existing_board: Option<BoardId> = db
        .query_scalar_opt(
            "SELECT id FROM boards WHERE tenant_id = $1 AND name = 'NookOS Bootstrap'",
            params![tenant.id],
        )
        .await?;
    if existing_board.is_none() {
        let board: Board = db
            .query_one(
                "INSERT INTO boards (id, tenant_id, name, key, provider)
             VALUES ($1, $2, 'NookOS Bootstrap', 'NOOK', 'local') RETURNING *",
                params![BoardId::new(), tenant.id],
            )
            .await?;

        // Name and TYPE together. The name is what a person reads and may
        // rename freely; the type is what automation targets, and seeding it
        // here is what stops the very first board from being one an agent
        // cannot navigate.
        let mut column_ids = Vec::new();
        for (i, (name, kind)) in [
            ("Triage", "backlog"),
            ("Todo", "unstarted"),
            ("In Progress", "started"),
            ("Done", "completed"),
        ]
        .iter()
        .enumerate()
        {
            let id: ColumnId = db
                .query_scalar(
                    "INSERT INTO board_columns (id, board_id, name, position, type)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
                    params![ColumnId::new(), board.id, *name, i as i32, *kind],
                )
                .await?;
            column_ids.push(id);
        }

        let tasks: [(&str, &str, usize); 6] = [
            (
                "Wire a second node",
                "Run `nook join` on another machine and watch it appear.",
                0,
            ),
            (
                "Try a Claude session",
                "Start a claude runtime session from a workspace.",
                0,
            ),
            (
                "Theme the terminal",
                "Tweak the amber-crt tokens in Settings.",
                0,
            ),
            (
                "Connect a real board",
                "Jira/GitHub/Linear/Trello providers land post-M1.",
                0,
            ),
            (
                "Watch the activity feed",
                "Every action lands in the timeline.",
                1,
            ),
            (
                "Boot the stack",
                "docker compose up — you already did this one.",
                3,
            ),
        ];
        for (i, (title, desc, col)) in tasks.iter().enumerate() {
            db.exec(
                "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description, position)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                params![
                    TaskId::new(),
                    tenant.id,
                    board.id,
                    column_ids[*col],
                    *title,
                    *desc,
                    i as i32
                ],
            )
            .await?;
        }
    }

    // A populated Mission Control on a fresh stack (MAIN-226): a demo repo on a
    // demo node with a clone + a worktree, a session bound to the clone, a loose
    // $HOME terminal, and a tombstoned checkout so the ghosting is visible.
    // Dev-only, idempotent (keyed on the demo workspace slug), and clearly
    // synthetic — the fake remote never matches the real compose node's
    // discovered repos, and the demo node never reports, so reconcile never
    // touches these rows.
    let demo_slug = "mission-demo";
    let demo_exists: Option<WorkspaceId> = db
        .query_scalar_opt(
            "SELECT id FROM workspaces WHERE tenant_id = $1 AND slug = $2",
            params![tenant.id, demo_slug],
        )
        .await?;
    if demo_exists.is_none() {
        let node_id = NodeId::new();
        db.exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
             VALUES ($1, $2, 'demo-box', $3, 'online')",
            params![node_id, tenant.id, format!("demo-{}", node_id.0.simple())],
        )
        .await?;

        let ws_id = WorkspaceId::new();
        db.exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_url, git_remote_normalized)
             VALUES ($1, $2, 'example/widgets', $3,
                     'git@github.com:example/widgets.git', 'github.com/example/widgets')",
            params![ws_id, tenant.id, demo_slug],
        )
        .await?;

        // A primary clone (clean) and a worktree (dirty) of the same repo.
        let clone_id = NodeWorkspaceId::new();
        db.exec(
            "INSERT INTO node_workspaces
                 (id, tenant_id, node_id, workspace_id, path, git_remote_url,
                  git_remote_normalized, git_branch, git_status, kind)
             VALUES ($1, $2, $3, $4, '/srv/example/widgets',
                     'git@github.com:example/widgets.git', 'github.com/example/widgets',
                     'main', $5, 'clone')",
            params![
                clone_id,
                tenant.id,
                node_id,
                ws_id,
                serde_json::json!({ "dirty": false })
            ],
        )
        .await?;
        db.exec(
            "INSERT INTO node_workspaces
                 (id, tenant_id, node_id, workspace_id, path, git_branch, git_status, kind)
             VALUES ($1, $2, $3, $4, '/srv/example/widgets__login', 'feature/login', $5, 'worktree')",
            params![
                NodeWorkspaceId::new(),
                tenant.id,
                node_id,
                ws_id,
                serde_json::json!({ "dirty": true })
            ],
        )
        .await?;
        // A tombstoned checkout — shown ghosted, not hidden (MAIN-220).
        db.exec(
            &format!(
                "INSERT INTO node_workspaces
                     (id, tenant_id, node_id, workspace_id, path, git_branch, git_status, kind, missing_at)
                 VALUES ($1, $2, $3, $4, '/srv/example/widgets__old', 'old', $5, 'worktree', {})",
                nook_db::dialect::type_mapping(db.engine()).now()
            ),
            params![
                NodeWorkspaceId::new(),
                tenant.id,
                node_id,
                ws_id,
                serde_json::json!({ "dirty": false })
            ],
        )
        .await?;

        // A session bound to the clone, and a loose $HOME terminal (no workspace).
        db.exec(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, checkout_id)
             VALUES ($1, $2, $3, $4, 'widgets · claude', 'claude', 'running', $5)",
            params![SessionId::new(), tenant.id, ws_id, node_id, clone_id],
        )
        .await?;
        db.exec(
            "INSERT INTO sessions (id, tenant_id, node_id, name, runtime, status)
             VALUES ($1, $2, $3, 'demo-box · shell', 'bash', 'running')",
            params![SessionId::new(), tenant.id, node_id],
        )
        .await?;
    }

    // ── The dogfood workspace (MAIN-341) ────────────────────────────────────
    //
    // Deliberately NOT the `mission-demo` block above. That one is synthetic on
    // purpose — a fake `git@` remote on a node that never reports, so Mission
    // Control looks populated and reconcile never touches it. This one is the
    // opposite: everything here is real, in the tenant the operator node joins,
    // pointing at a repo that actually exists on that node's disk. The two live
    // side by side because they answer different questions — "does the UI look
    // right" and "does the loop actually run".
    //
    // **No credential is seeded, here or anywhere.** The claude device login
    // stays the one human step (`./run.sh --claude-login`), and the dev CLI
    // token is minted by a login in `run.sh`, never written here. What this
    // block provisions is scaffolding: an identity, a workspace, a switch, and
    // a ticket.
    let dogfood_slug = "nook-dogfood";
    let dogfood_exists: Option<WorkspaceId> = db
        .query_scalar_opt(
            "SELECT id FROM workspaces WHERE tenant_id = $1 AND slug = $2",
            params![tenant.id, dogfood_slug],
        )
        .await?;

    // The dev browser identity, in the SAME tenant the join token puts the
    // operator node in (AC-4). `dev_login` becomes an EXISTING user when the
    // email matches one, so seeding this row is what makes "sign in as dev"
    // land in the operator's tenant rather than minting a fresh personal one —
    // which is the cross-tenant mismatch MAIN-340 is about, sidestepped rather
    // than fixed (NG-1).
    //
    // An identity, not a credential: no password hash, no token. Signing in
    // still goes through the dev-login hatch, which is itself gated on
    // AUTH_DEV_MODE and refused outright in production.
    let dev_email = "dev@nookos.local";
    let dev_user: Option<UserId> = db
        .query_scalar_opt(
            "SELECT id FROM users WHERE tenant_id = $1 AND email = $2",
            params![tenant.id, dev_email],
        )
        .await?;
    let dev_user = match dev_user {
        Some(id) => id,
        None => {
            let id = UserId::new();
            db.exec(
                "INSERT INTO users (id, tenant_id, display_name, email, role)
                 VALUES ($1, $2, 'Dev', $3, 'owner')",
                params![id, tenant.id, dev_email],
            )
            .await?;
            id
        }
    };
    // Without the membership grant the session resolves and then 403s — a
    // memberless session (MAIN-98). Idempotent, and never elevating: the role
    // is the one the user row already carries.
    db.exec(
        "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, 'owner')
         ON CONFLICT DO NOTHING",
        params![uuid::Uuid::now_v7(), tenant.id, dev_user],
    )
    .await?;

    // Loops ON for this tenant (AC-3). Off is the shipped default and stays the
    // default everywhere else (MAIN-239) — this is the one tenant where the
    // whole point is that a job placed against the seeded workspace runs.
    db.exec(
        "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
         VALUES ($1, $2, 'tenant', NULL, $3, $4)
         ON CONFLICT (tenant_id, scope, user_id, key) DO UPDATE SET value = EXCLUDED.value",
        params![
            uuid::Uuid::now_v7(),
            tenant.id,
            crate::services::loops::KEY,
            serde_json::Value::Bool(true)
        ],
    )
    .await?;

    if dogfood_exists.is_none() {
        // A LOCAL path, not a `git@` remote (AC-2): the operator node clones it
        // with no ssh key, no credential, and no network. `run.sh` creates the
        // bare repo at this exact path inside the operator container — the two
        // have to agree, which is why the path is spelled out in both places
        // and nowhere else.
        let ws_id = WorkspaceId::new();
        db.exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_url, git_remote_normalized)
             VALUES ($1, $2, 'nook-dogfood', $3, $4, $4)",
            params![ws_id, tenant.id, dogfood_slug, DOGFOOD_REPO_PATH],
        )
        .await?;

        // A ticket ready to open at /loop/<key> and draft a spec against (AC-6).
        // Todo, not Triage: the loop's pick query excludes backlog columns
        // (MAIN-80), so a card left in Triage would look ready and never be
        // takeable.
        if let Some(board_id) = db
            .query_scalar_opt::<BoardId>(
                "SELECT id FROM boards WHERE tenant_id = $1 AND name = 'NookOS Bootstrap'",
                params![tenant.id],
            )
            .await?
        {
            let todo: Option<ColumnId> = db
                .query_scalar_opt(
                    "SELECT id FROM board_columns WHERE board_id = $1 AND type = 'unstarted'
                     ORDER BY position LIMIT 1",
                    params![board_id],
                )
                .await?;
            if let Some(column_id) = todo {
                db.exec(
                    "INSERT INTO tasks
                         (id, tenant_id, board_id, column_id, workspace_id, title,
                          description, type, position)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, 'task', 100)",
                    params![
                        TaskId::new(),
                        tenant.id,
                        board_id,
                        column_id,
                        ws_id,
                        "Add a greeting command to the dogfood repo",
                        "The one-click test of the whole loop.\n\n                         Open this card, press **Draft a spec**, and the operator node                          clones the local `nook-dogfood` repo and runs `/nook-spec`                          against it. Nothing here needs ssh keys or a network: the repo                          is a real bare git repo on the node's own disk.\n\n                         If the job sits queued saying *no eligible executor*, the fleet                          has no Claude session — run `./run.sh --claude-login` once."
                    ],
                )
                .await?;
            }
        }
    }

    // A few historical events so the timeline isn't empty on first login.
    let event_count: i64 = db
        .query_scalar(
            "SELECT count(*) FROM events WHERE tenant_id = $1",
            params![tenant.id],
        )
        .await?;
    if event_count == 0 {
        for (kind, payload) in [
            (
                "system.seeded",
                serde_json::json!({ "detail": "dev environment created" }),
            ),
            (
                "system.migrated",
                serde_json::json!({ "migration": "0001_init" }),
            ),
        ] {
            crate::events::insert(
                &events_repo,
                tenant.id,
                crate::events::EventDraft::new(kind).payload(payload),
            )
            .await;
        }
    }

    bootstrap_operator(db).await;

    tracing::info!(tenant = %tenant.slug, "seed complete");
    Ok(())
}

/// Give the first user `operator @ deployment`, once.
///
/// Somebody has to be able to run the deployment, and on a self-hosted instance
/// that is whoever set it up. Granted here rather than by a flag, so a fresh
/// install is usable without anybody reading documentation about bindings.
///
/// Idempotent by "only when NO deployment binding exists" rather than by
/// upsert. That distinction is the whole safety of it: an upsert keyed on the
/// user would silently re-grant after a revocation, and would hand the role to
/// whoever happened to be first if the users table were ever rebuilt. A second
/// operator has to be a deliberate act.
pub async fn bootstrap_operator(db: &DbPool) {
    // As in `run`: no AppState here, so the read model is built from the pool.
    let events_repo = crate::repo::read_model::DbReadModelRepository::new(db.clone());
    let existing: Result<Option<uuid::Uuid>, _> = db
        .query_scalar_opt(
            "SELECT id FROM role_bindings WHERE scope_type = 'deployment' LIMIT 1",
            params![],
        )
        .await;
    match existing {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "could not check for a deployment operator");
            return;
        }
    }

    let first: Option<(uuid::Uuid, TenantId)> = match db
        .query_opt(
            "SELECT id, tenant_id FROM users ORDER BY created_at LIMIT 1",
            params![],
        )
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, "could not find a first user");
            return;
        }
    };
    // No users yet. A fresh instance has nobody to appoint, which is why this
    // is ALSO called when the first user is created — seeding runs before
    // anybody has signed in, and "it will happen on the next boot" is not true
    // of a control plane nobody restarts. A deployment with no operator has no
    // way to grow one.
    let Some((user_id, tenant_id)) = first else {
        return;
    };

    let done = db
        .exec(
            "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
         VALUES ($1, 'user', $2, 'operator', 'deployment', NULL)
         ON CONFLICT DO NOTHING",
            params![uuid::Uuid::now_v7(), user_id],
        )
        .await;

    match done {
        Ok(r) if r > 0 => {
            tracing::info!(user = %user_id, "granted operator @ deployment (bootstrap)");
            // Recorded, because "who became the operator, and when" is the
            // first question anybody audits.
            crate::events::insert(
                &events_repo,
                tenant_id,
                crate::events::EventDraft::new("rbac.bootstrap")
                    .actor("user", user_id)
                    .payload(serde_json::json!({
                        "role": "operator",
                        "scope": "deployment",
                        "reason": "first user on a deployment with no operator",
                    })),
            )
            .await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "could not grant the bootstrap operator"),
    }
}
