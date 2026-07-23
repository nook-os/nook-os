//! Guided setup.
//!
//! The install script's whole job is to put a verified binary on disk and get
//! out of the way; everything a person is actually asked lives here. That is
//! deliberate — a wizard in shell cannot be unit-tested, has to be written once
//! per platform, and drifts from the `nook setup` that already existed.

pub mod generate;
pub mod hooks;
pub mod node;
pub mod server;
pub mod service;
pub mod skills;
pub mod tmux_setup;
pub mod tty;
