//! Shared service layer: REST handlers and MCP tools both call into here so
//! the two surfaces can never drift apart.

pub mod core;
pub mod discovery;
pub mod identity;
pub mod interactions;
pub mod job_dispatch;
pub mod job_reaper;
pub mod jobs;
pub mod kanban;
pub mod local_auth;
pub mod loops;
pub mod notify;
pub mod policy;
pub mod queue;
pub mod schedule;
pub mod secrets;
pub mod tasks;
pub mod taskwork;
pub mod triggers;
pub mod workspace_reaper;
