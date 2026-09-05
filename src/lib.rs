//! Reusable `peq` library.
//!
//! The binary is intentionally a thin CLI adapter. Application operations live in
//! [`application`] so the CLI and TUI use the same state transitions while the
//! PipeWire and filter-chain implementations remain behind their legacy adapters.

pub mod application;
pub mod chain;
pub mod cli;
pub mod dsp;
pub mod engine;
pub mod preset;
pub mod pw;
pub mod render;
pub mod storage;
pub mod tui;
pub mod validation;

#[cfg(feature = "native-audio")]
pub mod native_audio;

pub use validation::{validate_name, validate_preset};
