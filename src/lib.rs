//! Reusable `peq` library.
//!
//! The binary is intentionally a thin CLI adapter. Application operations live in
//! [`application`] so the CLI and TUI use the same state transitions while the
//! PipeWire and filter-chain implementations remain behind their legacy adapters.

pub mod application;
pub mod chain;
pub mod cli;
pub mod dsp;
pub mod preset;
pub mod pw;
pub mod render;
pub mod tui;
pub mod validation;

pub use validation::{validate_name, validate_preset};
