//! Engine control contracts and an in-memory implementation for development.
//!
//! The trait in this module is deliberately independent of PipeWire.  A daemon can
//! put a socket client behind [`EngineClient`] later while the CLI, TUI and tests
//! continue to use the same revision and failure semantics.

use crate::preset::Preset;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The default deadline used by convenience callers of an engine client.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);

/// The state confirmed by an engine after applying a preset or changing bypass.
///
/// A snapshot owns its preset so a daemon can retain and restore it after a
/// client disconnects without borrowing a caller's draft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedSnapshot {
    pub preset: Preset,
    pub revision: u64,
    pub bypassed: bool,
}

/// Input and output levels reported by an engine.
///
/// The simulated engine has no audio stream, so it leaves these values at their
/// silent defaults.  The fields are kept in the shared status contract for the
/// native engine and future IPC clients.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EngineMetrics {
    pub input_rms: f64,
    pub output_rms: f64,
    pub input_peak: f64,
    pub output_peak: f64,
    pub input_clipped: bool,
    pub output_clipped: bool,
}

impl Default for EngineMetrics {
    fn default() -> Self {
        Self {
            input_rms: 0.0,
            output_rms: 0.0,
            input_peak: 0.0,
            output_peak: 0.0,
            input_clipped: false,
            output_clipped: false,
        }
    }
}

/// Current connection, graph and applied-preset state exposed to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub connected: bool,
    pub sample_rate: u32,
    pub revision: u64,
    pub output: Option<String>,
    pub metrics: EngineMetrics,
    pub snapshot: Option<AppliedSnapshot>,
}

/// Stable categories of failures returned by an engine client.
///
/// This type is serializable so the same error can cross the future JSON Lines
/// boundary without exposing implementation-specific errors from PipeWire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum EngineError {
    #[error("engine operation timed out")]
    Timeout,
    #[error("engine is disconnected")]
    Disconnected,
    #[error("stale engine revision: expected {expected}, current {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("engine failure: {0}")]
    Failure(String),
    #[error("no preset has been applied yet")]
    NoSnapshot,
    #[error("output name cannot be empty")]
    InvalidOutput,
}

/// A small owned event surface suitable for polling or forwarding over IPC.
///
/// Events intentionally contain owned snapshots.  Consumers can retain an event
/// after the engine advances to a later revision without borrowing engine state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineEvent {
    Snapshot(AppliedSnapshot),
    Disconnected,
}

/// Controls the confirmed engine state.
///
/// Every operation receives a deadline because a daemon must be able to return a
/// bounded response to a slow or unavailable backend.  Mutations return only
/// after the requested state has been confirmed.
pub trait EngineClient: Send {
    fn apply(
        &mut self,
        preset: Preset,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError>;

    fn status(&mut self, timeout: Duration) -> Result<EngineStatus, EngineError>;

    fn bypass(
        &mut self,
        bypassed: bool,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError>;

    fn select_output(
        &mut self,
        output: String,
        timeout: Duration,
    ) -> Result<EngineStatus, EngineError>;

    /// Poll events that occurred since the previous call.  IPC implementations
    /// may override this; clients that have no event queue return an empty list.
    fn events(&mut self) -> Vec<EngineEvent> {
        Vec::new()
    }
}

/// Deterministic behavior modes for [`MockEngine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockEngineMode {
    /// Complete the operation immediately.
    Success,
    /// Wait for this duration, timing out when it exceeds the request deadline.
    Delay(Duration),
    /// Reject operations as unavailable.
    Disconnected,
    /// Reject operations with the supplied stable failure message.
    Failure(String),
}

/// Configuration for a [`MockEngine`].
#[derive(Debug, Clone)]
pub struct MockEngineConfig {
    pub mode: MockEngineMode,
    pub sample_rate: u32,
    pub output: Option<String>,
}

impl Default for MockEngineConfig {
    fn default() -> Self {
        Self {
            mode: MockEngineMode::Success,
            sample_rate: 48_000,
            output: Some("mock-output".to_string()),
        }
    }
}

impl MockEngineConfig {
    pub fn success() -> Self {
        Self::default()
    }

    pub fn with_mode(mode: MockEngineMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }
}

impl From<MockEngineMode> for MockEngineConfig {
    fn from(mode: MockEngineMode) -> Self {
        Self::with_mode(mode)
    }
}

/// A no-audio engine used by tests and by clients while PipeWire is unavailable.
///
/// State changes happen only after [`MockEngineMode::Success`] or a delay that
/// fits within the request deadline.  Failure, disconnection and timeout paths
/// leave the revision, snapshot, output and event queue untouched.
#[derive(Debug, Clone)]
pub struct MockEngine {
    config: MockEngineConfig,
    revision: u64,
    snapshot: Option<AppliedSnapshot>,
    metrics: EngineMetrics,
    events: Vec<EngineEvent>,
}

impl Default for MockEngine {
    fn default() -> Self {
        Self::new(MockEngineConfig::default())
    }
}

impl MockEngine {
    pub fn new(config: impl Into<MockEngineConfig>) -> Self {
        let config = config.into();
        Self {
            config,
            revision: 0,
            snapshot: None,
            metrics: EngineMetrics::default(),
            events: Vec::new(),
        }
    }

    pub fn with_mode(mode: MockEngineMode) -> Self {
        Self::new(MockEngineConfig::with_mode(mode))
    }

    pub fn mode(&self) -> &MockEngineMode {
        &self.config.mode
    }

    /// Change behavior between test phases without resetting confirmed state.
    pub fn set_mode(&mut self, mode: MockEngineMode) {
        let was_connected = !matches!(self.config.mode, MockEngineMode::Disconnected);
        let is_connected = !matches!(mode, MockEngineMode::Disconnected);
        self.config.mode = mode;
        if was_connected && !is_connected {
            self.events.push(EngineEvent::Disconnected);
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn snapshot(&self) -> Option<&AppliedSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    /// Return status without running the configured operation mode.  This is
    /// useful for inspecting a disconnected mock in assertions and diagnostics.
    pub fn status_unchecked(&self) -> EngineStatus {
        self.status_value()
    }

    pub fn apply(
        &mut self,
        preset: Preset,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError> {
        <Self as EngineClient>::apply(self, preset, expected_revision, timeout)
    }

    pub fn status(&mut self, timeout: Duration) -> Result<EngineStatus, EngineError> {
        <Self as EngineClient>::status(self, timeout)
    }

    pub fn bypass(
        &mut self,
        bypassed: bool,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError> {
        <Self as EngineClient>::bypass(self, bypassed, expected_revision, timeout)
    }

    pub fn select_output(
        &mut self,
        output: String,
        timeout: Duration,
    ) -> Result<EngineStatus, EngineError> {
        <Self as EngineClient>::select_output(self, output, timeout)
    }

    fn status_value(&self) -> EngineStatus {
        EngineStatus {
            connected: !matches!(self.config.mode, MockEngineMode::Disconnected),
            sample_rate: self.config.sample_rate,
            revision: self.revision,
            output: self.config.output.clone(),
            metrics: self.metrics,
            snapshot: self.snapshot.clone(),
        }
    }

    /// Simulate backend work.  In the delay case, only the portion within the
    /// request deadline is slept; a test cannot accidentally block indefinitely.
    fn ready(&self, timeout: Duration) -> Result<(), EngineError> {
        match &self.config.mode {
            MockEngineMode::Success => Ok(()),
            MockEngineMode::Delay(delay) => {
                if *delay > timeout {
                    if !timeout.is_zero() {
                        std::thread::sleep(timeout);
                    }
                    Err(EngineError::Timeout)
                } else {
                    if !delay.is_zero() {
                        std::thread::sleep(*delay);
                    }
                    Ok(())
                }
            }
            MockEngineMode::Disconnected => Err(EngineError::Disconnected),
            MockEngineMode::Failure(message) => Err(EngineError::Failure(message.clone())),
        }
    }

    fn check_revision(&self, expected_revision: u64) -> Result<(), EngineError> {
        if expected_revision == self.revision {
            Ok(())
        } else {
            Err(EngineError::RevisionConflict {
                expected: expected_revision,
                actual: self.revision,
            })
        }
    }

    fn next_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }
}

impl EngineClient for MockEngine {
    fn apply(
        &mut self,
        preset: Preset,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError> {
        self.ready(timeout)?;
        self.check_revision(expected_revision)?;

        crate::validation::validate_preset(&preset, Some(self.config.sample_rate as f64))
            .map_err(|error| EngineError::Failure(error.to_string()))?;

        let snapshot = AppliedSnapshot {
            preset,
            revision: self.next_revision(),
            bypassed: false,
        };
        self.snapshot = Some(snapshot.clone());
        self.events.push(EngineEvent::Snapshot(snapshot.clone()));
        Ok(snapshot)
    }

    fn status(&mut self, timeout: Duration) -> Result<EngineStatus, EngineError> {
        // Connection loss is itself a useful status result: callers can render a
        // disconnected state and decide when to retry. Mutations still return the
        // stable `Disconnected` error above.
        if matches!(self.config.mode, MockEngineMode::Disconnected) {
            return Ok(self.status_value());
        }
        self.ready(timeout)?;
        Ok(self.status_value())
    }

    fn bypass(
        &mut self,
        bypassed: bool,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError> {
        self.ready(timeout)?;
        self.check_revision(expected_revision)?;
        let Some(current) = self.snapshot.as_ref() else {
            return Err(EngineError::NoSnapshot);
        };

        let snapshot = AppliedSnapshot {
            preset: current.preset.clone(),
            revision: self.next_revision(),
            bypassed,
        };
        self.snapshot = Some(snapshot.clone());
        self.events.push(EngineEvent::Snapshot(snapshot.clone()));
        Ok(snapshot)
    }

    fn select_output(
        &mut self,
        output: String,
        timeout: Duration,
    ) -> Result<EngineStatus, EngineError> {
        self.ready(timeout)?;
        if output.trim().is_empty() {
            return Err(EngineError::InvalidOutput);
        }
        self.config.output = Some(output);
        Ok(self.status_value())
    }

    fn events(&mut self) -> Vec<EngineEvent> {
        self.drain_events()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::silent_preset;

    const TIMEOUT: Duration = Duration::from_millis(20);

    #[test]
    fn success_advances_revision_and_emits_owned_snapshot() {
        let mut engine = MockEngine::default();
        let applied = engine.apply(silent_preset("flat"), 0, TIMEOUT).unwrap();

        assert_eq!(applied.revision, 1);
        assert!(!applied.bypassed);
        assert_eq!(applied.preset.name, "flat");
        assert_eq!(engine.status(TIMEOUT).unwrap().revision, 1);
        assert_eq!(engine.events(), vec![EngineEvent::Snapshot(applied)]);
    }

    #[test]
    fn stale_revision_is_rejected_without_mutating_state() {
        let mut engine = MockEngine::default();
        engine.apply(silent_preset("first"), 0, TIMEOUT).unwrap();
        let _ = engine.events();
        let before = engine.status_unchecked();
        let error = engine
            .apply(silent_preset("stale"), 0, TIMEOUT)
            .unwrap_err();

        assert_eq!(
            error,
            EngineError::RevisionConflict {
                expected: 0,
                actual: 1
            }
        );
        assert_eq!(engine.status_unchecked(), before);
        assert!(engine.events().is_empty());
    }

    #[test]
    fn delayed_timeout_does_not_mutate_snapshot_or_revision() {
        let mut engine = MockEngine::with_mode(MockEngineMode::Delay(Duration::from_millis(40)));
        let before = engine.status_unchecked();
        let error = engine.apply(silent_preset("late"), 0, TIMEOUT).unwrap_err();

        assert_eq!(error, EngineError::Timeout);
        assert_eq!(engine.status_unchecked(), before);
        assert!(engine.events().is_empty());
    }

    #[test]
    fn disconnection_and_failure_leave_confirmed_state_unchanged() {
        let mut engine = MockEngine::default();
        engine.apply(silent_preset("kept"), 0, TIMEOUT).unwrap();
        let before = engine.status_unchecked();
        engine.set_mode(MockEngineMode::Disconnected);
        assert_eq!(
            engine.bypass(true, 1, TIMEOUT).unwrap_err(),
            EngineError::Disconnected
        );
        assert!(!engine.status(TIMEOUT).unwrap().connected);
        assert_eq!(engine.status_unchecked().snapshot, before.snapshot);

        engine.set_mode(MockEngineMode::Failure("backend broke".into()));
        assert_eq!(
            engine.apply(silent_preset("lost"), 1, TIMEOUT).unwrap_err(),
            EngineError::Failure("backend broke".into())
        );
        assert_eq!(engine.status_unchecked().snapshot, before.snapshot);
        assert_eq!(engine.status_unchecked().revision, before.revision);
    }

    #[test]
    fn bypass_and_output_selection_are_confirmed_operations() {
        let mut engine = MockEngine::default();
        let applied = engine.apply(silent_preset("music"), 0, TIMEOUT).unwrap();
        let bypassed = engine.bypass(true, applied.revision, TIMEOUT).unwrap();
        assert!(bypassed.bypassed);
        assert_eq!(bypassed.revision, 2);

        let status = engine.select_output("headphones".into(), TIMEOUT).unwrap();
        assert_eq!(status.output.as_deref(), Some("headphones"));
        assert_eq!(status.revision, 2);
    }
}
