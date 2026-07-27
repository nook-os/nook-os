//! Shared infrastructure for the NookOS control plane and any future worker:
//! configuration parsing plus the pluggable cache, artifact-storage, and mail
//! providers. These moved out of `nook-control` (MAIN-146) so a second binary
//! can link the providers without linking the whole control plane. Migrations
//! stay owned by `nook-control`.

pub mod cache;
pub mod config;
pub mod mailer;
pub mod queue;
pub mod storage;

pub use config::Config;
