pub mod agent_tls;
pub mod auth;
pub mod ca;
pub mod client_ip;
pub mod crypto;
pub mod error;
pub mod events;
pub mod mcp_backend;
pub mod openapi;
pub mod repo;
pub mod routes;
pub mod seed;
pub mod services;
pub mod session_status;
pub mod state;
pub mod ws;

// Config parsing and the cache / storage / mail / queue providers now live in
// `nook-infra` (MAIN-146, MAIN-147). They are re-exported at their original
// module paths so every `crate::config` / `crate::cache` / `crate::storage` /
// `crate::mailer` / `crate::queue` call site inside this crate resolves.
pub use nook_infra::{cache, config, mailer, queue, storage};

pub use nook_infra::Config;
pub use state::AppState;

// `sqlx::migrate!` embeds the migration set at COMPILE time, so adding a new
// `.sql` file does not by itself force a rebuild — this file has to change too
// for the new migration to be embedded and applied. Migrations embedded:
// 0001_init, 0002_add_person_id, 0003_backfill_tenant_members,
// 0004_add_task_archived_at, 0005_invites, 0006_add_email_verified,
// 0007_email_verification_tokens, 0008_mail_sends, 0009_task_type,
// 0010_review_column, 0011_user_notebook, 0012_task_visibility,
// 0013_task_parent, 0014_managed_content, 0015_board_automation,
// 0016_node_owner, 0017_node_shared, 0018_work_queue, 0019_notebook_seal,
// 0020_loop_jobs, 0021_loop_job_queued_reason, 0022_backfill_auth_mode,
// 0023_interactions, 0024_node_workspace_missing_at,
// 0025_checkout_kind_and_session_checkout, 0026_workspace_git_remote_url,
// 0027_task_checkout_id, 0028_loop_job_seed, 0029_node_labels_taints,
// 0030_workspace_session_spec, 0031_session_managed,
// 0032_managed_session_per_checkout, 0033_task_claim_lease,
// 0034_session_port_leases, 0035_operator_tenant_manage.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The SQLite track (MAIN-236 scaffolded it; MAIN-196 boots it). A parallel set,
/// not a translation performed at runtime: the DDL differs enough that pretending
/// one file serves both engines is how the two schemas quietly diverge.
pub static MIGRATOR_SQLITE: sqlx::migrate::Migrator = sqlx::migrate!("./migrations_sqlite");

/// The migration set for the engine actually in front of us (MAIN-196 AC-2).
/// Postgres is unchanged and stays frozen; SQLite gets its own track.
pub fn migrator_for(engine: nook_db::Engine) -> &'static sqlx::migrate::Migrator {
    match engine {
        nook_db::Engine::Postgres => &MIGRATOR,
        nook_db::Engine::Sqlite => &MIGRATOR_SQLITE,
    }
}

/// The squash manifest shipped alongside those migrations (MAIN-235): which
/// pre-squash ledger the canonical `0001` replaced, so a database carrying that
/// ledger re-stamps itself at boot instead of failing on 28 "missing" versions.
/// A file with no `new` line means this build ships no squash.
pub static SQUASH_MANIFEST: &str = include_str!("../migrations/squash-manifest.txt");
