pub mod auth;
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

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
