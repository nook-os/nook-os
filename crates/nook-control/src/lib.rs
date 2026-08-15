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
pub mod tunnels;
pub mod ws;

// Config parsing and the cache / storage / mail / queue providers now live in
// `nook-infra` (MAIN-146, MAIN-147). They are re-exported at their original
// module paths so every `crate::config` / `crate::cache` / `crate::storage` /
// `crate::mailer` / `crate::queue` call site inside this crate resolves.
pub use nook_infra::{cache, config, mailer, queue, storage};

pub use nook_infra::{Config, OidcSetup};
pub use state::AppState;

// `sqlx::migrate!` embeds the migration set at COMPILE time, so adding a new
// `.sql` file does not by itself force a rebuild — this file has to change too
// for the new migration to be embedded and applied. TOUCH IT IN THE SAME COMMIT
// as the migration, or the container keeps running the old set and silently
// skips yours.
//
// It used to enumerate every embedded version here, which is what the line
// below replaces: the list froze at 0035 while the directory went on to 0069,
// so a reader checking whether their file was embedded was reading a comment
// that had been wrong for thirty-four migrations. The directory is the list.
// Head of the set: 0076_task_reports (MAIN-603).
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
