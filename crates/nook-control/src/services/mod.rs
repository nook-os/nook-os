//! Shared service layer: REST handlers and MCP tools both call into here so
//! the two surfaces can never drift apart.

pub mod core;
pub mod discovery;
pub mod identity;
pub mod kanban;
pub mod local_auth;
pub mod schedule;
pub mod secrets;
pub mod taskwork;
