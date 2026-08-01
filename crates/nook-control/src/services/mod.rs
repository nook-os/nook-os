//! Shared service layer: REST handlers and MCP tools both call into here so
//! the two surfaces can never drift apart.

pub mod activity_queries;
pub mod claim_reaper;
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
pub mod notebook_queries;
pub mod notify;
pub mod operator_queries;
pub mod overview_queries;
pub mod policy;
pub mod port_leases;
pub mod queue;
pub mod repo_settings;
pub mod runtime_auth;
pub mod runtime_auth_flow;
pub mod schedule;
pub mod secrets;
pub mod session_queries;
pub mod session_reconcile;
pub mod tasks;
pub mod taskwork;
pub mod triggers;
pub mod workspace_queries;
pub mod workspace_reaper;
