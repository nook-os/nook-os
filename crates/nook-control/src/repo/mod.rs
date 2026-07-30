//! Repositories: data access behind intent-named traits (the repository chain).
//!
//! A service that reads `repo.count_children(task)` says what it wants; the same
//! service holding a `SELECT count(*) FROM tasks WHERE parent_task_id = $1` says
//! how a table is shaped, and says it in dozens of places. Three things follow
//! from the trait that did not follow from inline SQL:
//!
//! - callers unit-test against an in-memory fake, with no database at all;
//! - the query for an aggregate lives in exactly one file, so a schema change
//!   has one blast radius;
//! - a per-engine implementation becomes a swap rather than a rewrite, if a
//!   hotspot ever proves it needs one (deliberately not needed yet).

pub mod tasks;
