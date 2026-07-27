//! Shared test-database setup — now a thin shim over `nook_testkit` (MAIN-156).
//!
//! The canonical harness — the migrated pool, the one `Config` construction,
//! `AppState`, the entity helpers (`tenant`/`user`/`node`/`workspace`), teardown,
//! and the MAIN-93 isolation doctrine — all live in `nook_testkit`. This module
//! re-exports `test_pool` so the suites that still take the bare pool keep
//! compiling; new or migrated suites use `nook_testkit::TestBed` directly.

pub use nook_testkit::test_pool;
