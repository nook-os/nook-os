//! Fixtures shared between integration test binaries.
//!
//! Each binary compiles this module separately, so anything it does not use is
//! dead code there — hence the `allow` inside each submodule rather than a
//! `#[cfg]` maze over the items.
pub mod build_ports;
