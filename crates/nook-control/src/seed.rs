//! Idempotent dev seeds. `docker compose down -v` destroys everything; this
//! brings the same predictable environment back on every reboot.

use anyhow::Result;
use nook_types::*;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::config::Config;

pub fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

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

pub async fn run(db: &PgPool, cfg: &Config) -> Result<()> {
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
        sqlx::query(
            "INSERT INTO themes (id, tenant_id, name, slug, tokens)
             VALUES ($1, NULL, $2, $3, $4)
             ON CONFLICT (slug) DO UPDATE SET tokens = EXCLUDED.tokens",
        )
        .bind(ThemeId::new())
        .bind(name)
        .bind(slug)
        .bind(tokens)
        .execute(db)
        .await?;
    }

    if cfg.is_production() {
        tracing::info!("seed: built-in themes only (production)");
        return Ok(());
    }

    // Dev tenant — adopted (as owner) by the first identity that logs in.
    let slug = crate::services::identity::slugify(&cfg.default_tenant_name);
    let tenant: Tenant = match sqlx::query_as::<_, Tenant>("SELECT * FROM tenants WHERE slug = $1")
        .bind(&slug)
        .fetch_optional(db)
        .await?
    {
        Some(t) => t,
        None => {
            sqlx::query_as("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3) RETURNING *")
                .bind(TenantId::new())
                .bind(&cfg.default_tenant_name)
                .bind(&slug)
                .fetch_one(db)
                .await?
        }
    };

    // Well-known join token so the compose node can auto-join on boot.
    if let Some(token) = &cfg.dev_join_token {
        sqlx::query(
            "INSERT INTO join_tokens (id, tenant_id, token_hash, name, expires_at)
             VALUES ($1, $2, $3, 'dev auto-join', now() + interval '10 years')
             ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(JoinTokenId::new())
        .bind(tenant.id)
        .bind(hash_token(token))
        .execute(db)
        .await?;
    }

    // Sample local board with a few tasks.
    let existing_board: Option<(BoardId,)> =
        sqlx::query_as("SELECT id FROM boards WHERE tenant_id = $1 AND name = 'NookOS Bootstrap'")
            .bind(tenant.id)
            .fetch_optional(db)
            .await?;
    if existing_board.is_none() {
        let board: Board = sqlx::query_as(
            "INSERT INTO boards (id, tenant_id, name, provider) VALUES ($1, $2, 'NookOS Bootstrap', 'local') RETURNING *",
        )
        .bind(BoardId::new())
        .bind(tenant.id)
        .fetch_one(db)
        .await?;

        let mut column_ids = Vec::new();
        for (i, name) in ["Triage", "Todo", "In Progress", "Done"].iter().enumerate() {
            let (id,): (ColumnId,) = sqlx::query_as(
                "INSERT INTO board_columns (id, board_id, name, position) VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(ColumnId::new())
            .bind(board.id)
            .bind(name)
            .bind(i as i32)
            .fetch_one(db)
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
            sqlx::query(
                "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description, position)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(TaskId::new())
            .bind(tenant.id)
            .bind(board.id)
            .bind(column_ids[*col])
            .bind(title)
            .bind(desc)
            .bind(i as i32)
            .execute(db)
            .await?;
        }
    }

    // A few historical events so the timeline isn't empty on first login.
    let (event_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM events WHERE tenant_id = $1")
        .bind(tenant.id)
        .fetch_one(db)
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
                db,
                tenant.id,
                crate::events::EventDraft::new(kind).payload(payload),
            )
            .await;
        }
    }

    tracing::info!(tenant = %tenant.slug, "seed complete");
    Ok(())
}
