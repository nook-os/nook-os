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

pub async fn run(db: &PgPool, cfg: &Config) -> Result<()> {
    // Built-in themes (always seeded, all environments).
    for (name, slug, tokens) in [
        ("Charcoal Gold", "charcoal-gold", charcoal_gold_tokens()),
        ("Amber CRT", "amber-crt", amber_crt_tokens()),
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
