pub mod agent_tls;
pub mod auth;
pub mod ca;
pub mod config;
pub mod crypto;
pub mod error;
pub mod events;
pub mod mcp_backend;
pub mod openapi;
pub mod routes;
pub mod seed;
pub mod services;
pub mod state;
pub mod storage;
pub mod ws;

pub use config::Config;
pub use state::AppState;

// `sqlx::migrate!` embeds the migration set at COMPILE time, so adding a new
// `.sql` file does not by itself force a rebuild — this file has to change too
// for the new migration to be embedded and applied. Migrations embedded:
// 0001_init, 0002_add_person_id, 0003_backfill_tenant_members,
// 0004_add_task_archived_at, 0006_add_email_verified.
// (0005_invites is reserved by the in-flight invites PR; the numbers are
// applied in version order, so the gap here is harmless until it lands.)
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
