//! Repositories: data access behind intent-named traits (the repository chain).
//!
//! A service that reads `repo.has_active_membership(user, tenant)` says what it
//! wants; the same service holding a `SELECT 1 FROM tenant_members …` says how a
//! table is shaped, and says it in fifty places. Three things follow from the
//! trait that did not follow from inline SQL:
//!
//! - callers unit-test against an in-memory fake, with no database at all;
//! - the query for an aggregate lives in exactly one file, so a schema change
//!   has one blast radius;
//! - a per-engine implementation becomes a swap rather than a rewrite, if a
//!   hotspot ever proves it needs one (deliberately not needed yet — NG-3).

pub mod identity;
pub mod invites;
pub mod tasks;
pub mod workspaces;
