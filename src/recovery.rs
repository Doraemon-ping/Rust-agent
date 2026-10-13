//! s15: model-call recovery.
//!
//! A long-running harness cannot treat every provider hiccup as fatal. This
//! module is the seam between the loop and the provider:
//!
//! - classify the failure (`429`, `529`, prompt-too-long, everything else);
//! - back off and retry the transient ones, with exponential delay plus jitter;
//! - switch to a fallback model once overloads keep coming;
//! - escalate the token budget when a response was truncated.
//!
//! The loop still owns the control flow -- `RecoveryConfig` and
//! [`RecoveryState`] are plain data, so the decision to retry, escalate, or
//! give up stays visible in one place instead of hiding behind a wrapper.

use std::time::Duration;

use crate::error::Error;

/// Sent to the model when a truncated turn is resumed.
pub const CONTINUATION_PROMPT: &str =
    "Continue from the previous response. Do not repeat completed work.";

/// How the harness recovers from a failed or truncated model call.
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    /// Attempts per model call, including the first one.
    pub max_retries: usize,
    /// First backoff step; doubles each attempt.
    pub base_delay_ms: u64,
    /// Upper bound on a single backoff sleep.
    pub max_delay_ms: u64,
    /// Consecutive `529`s before switching to [`RecoveryConfig::fallback_model`].
    pub max_consecutive_overloaded: usize,
    /// Model to switch to after repeated overloads. `None` disables switching.
    pub fallback_model: Option<String>,
    /// Token budget used once, when a response stops on `max_tokens`.
    pub escalated_max_tokens: u32,
    /// Times a truncated, tool-less turn is resumed with [`CONTINUATION_PROMPT`].
    pub max_continuation_retries: usize,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        RecoveryConfig {
            max_retries: 3,
            base_delay_ms: 500,
            max_delay_ms: 32_000,
            max_consecutive_overloaded: 2,
            fallback_model: None,
            escalated_max_tokens: 16_000,
            max_continuation_retries: 2,
        }
    }
}

impl RecoveryConfig {
    /// Use `model` after repeated overloads.
    pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
        self.fallback_model = Some(model.into());
        self
    }

    /// Change the attempt budget per model call.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Change the one-shot token escalation.
    pub fn with_escalated_max_tokens(mut self, max_tokens: u32) -> Self {
        self.escalated_max_tokens = max_tokens;
        self
    }

    /// Backoff before retry number `attempt` (0-based).
    ///
    /// Exponential, capped, and jittered by up to 25% so a fleet of clients
    /// does not stampede the provider in lockstep.
    pub fn delay(&self, attempt: usize) -> Duration {
        let shift = attempt.min(16) as u32;
        let exponential = self.base_delay_ms.saturating_mul(1u64 << shift);
        let capped = exponential.min(self.max_delay_ms) as f64;
        let jittered = capped * (1.0 + 0.25 * jitter_fraction());
        Duration::from_millis(jittered.round() as u64)
    }

    /// Record an overloaded response.
    ///
    /// Returns the fallback model when the overload count crossed the
    /// threshold and a switch actually happened.
    pub fn note_overloaded(&self, state: &mut RecoveryState) -> Option<String> {
        state.consecutive_overloaded += 1;
        if state.consecutive_overloaded < self.max_consecutive_overloaded {
            return None;
        }
        state.consecutive_overloaded = 0;
        let fallback = self.fallback_model.clone()?;
        if state.current_model.as_deref() == Some(fallback.as_str()) {
            return None;
        }
        state.current_model = Some(fallback.clone());
        Some(fallback)
    }
}

/// Per-turn recovery bookkeeping.
///
/// Everything here is scoped to one `run_turn`: escalation is a one-shot per
/// turn, and the continuation budget is per turn, exactly like the reference.
#[derive(Debug, Default, Clone)]
pub struct RecoveryState {
    /// The budget was already escalated once this turn.
    pub has_escalated: bool,
    /// Consecutive `529`s, reset on any success.
    pub consecutive_overloaded: usize,
    /// Continuations already requested this turn.
    pub continuation_count: usize,
    /// The model to call; `None` means the client's configured default.
    pub current_model: Option<String>,
}

impl RecoveryState {
    /// A fresh state for one turn.
    pub fn new() -> Self {
        Self::default()
    }
}

/// What kind of failure a model call hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// HTTP 429 or an equivalent rate-limit signal.
    RateLimited,
    /// HTTP 529 / overloaded, or an equivalent signal.
    Overloaded,
    /// The prompt no longer fits the window.
    PromptTooLong,
    /// Anything else: retrying would not help.
    Fatal,
}

/// Classify a model-call failure.
///
/// The status code is trusted first, then the message, because gateways differ
/// in how faithfully they relay the provider's error type.
pub fn classify(error: &Error) -> ErrorClass {
    if let Error::Api { status, .. } = error {
        match *status {
            429 => return ErrorClass::RateLimited,
            529 => return ErrorClass::Overloaded,
            _ => {}
        }
    }
    let message = error.to_string().to_lowercase();
    if message.contains("rate_limit") || message.contains("ratelimit") || message.contains("429") {
        return ErrorClass::RateLimited;
    }
    if message.contains("overloaded") || message.contains("529") {
        return ErrorClass::Overloaded;
    }
    if crate::compact::is_context_overflow(error) {
        return ErrorClass::PromptTooLong;
    }
    ErrorClass::Fatal
}

/// Whether a class is worth retrying.
pub fn is_retryable(class: ErrorClass) -> bool {
    matches!(class, ErrorClass::RateLimited | ErrorClass::Overloaded)
}

/// A cheap jitter source: xorshift64 seeded from the wall clock.
///
/// This keeps the crate dependency-free; a retry delay needs decorrelation,
/// not cryptographic randomness.
fn jitter_fraction() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static STATE: AtomicU64 = AtomicU64::new(0);

    let mut seed = STATE.load(Ordering::Relaxed);
    if seed == 0 {
        seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    let mut x = seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, message: &str) -> Error {
        Error::Api {
            status,
            message: message.to_string(),
        }
    }

    #[test]
    fn status_codes_win_over_the_message() {
        assert_eq!(classify(&api(429, "slow down")), ErrorClass::RateLimited);
        assert_eq!(classify(&api(529, "overloaded")), ErrorClass::Overloaded);
        assert_eq!(
            classify(&api(400, "{\"type\":\"prompt_too_long\"}")),
            ErrorClass::PromptTooLong
        );
        assert_eq!(classify(&api(500, "boom")), ErrorClass::Fatal);
        assert_eq!(classify(&Error::Config("nope".into())), ErrorClass::Fatal);
    }

    #[test]
    fn messages_are_classified_when_the_status_is_vague() {
        assert_eq!(
            classify(&api(400, "RateLimitError: too many requests")),
            ErrorClass::RateLimited
        );
        assert_eq!(
            classify(&api(503, "the model is overloaded right now")),
            ErrorClass::Overloaded
        );
        assert!(is_retryable(ErrorClass::RateLimited));
        assert!(is_retryable(ErrorClass::Overloaded));
        assert!(!is_retryable(ErrorClass::PromptTooLong));
        assert!(!is_retryable(ErrorClass::Fatal));
    }

    #[test]
    fn backoff_grows_caps_and_jitters() {
        let config = RecoveryConfig {
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            ..RecoveryConfig::default()
        };
        let first = config.delay(0).as_millis() as u64;
        let second = config.delay(1).as_millis() as u64;
        assert!((100..=125).contains(&first), "{first}");
        assert!((200..=250).contains(&second), "{second}");
        // Capped, plus at most 25% jitter.
        let capped = config.delay(9).as_millis() as u64;
        assert!((1_000..=1_250).contains(&capped), "{capped}");
    }

    #[test]
    fn overloads_switch_to_the_fallback_model() {
        let config = RecoveryConfig {
            max_consecutive_overloaded: 2,
            fallback_model: Some("backup-model".to_string()),
            ..RecoveryConfig::default()
        };
        let mut state = RecoveryState::new();
        assert_eq!(config.note_overloaded(&mut state), None);
        assert_eq!(
            config.note_overloaded(&mut state),
            Some("backup-model".to_string())
        );
        assert_eq!(state.current_model.as_deref(), Some("backup-model"));
        // Once switched, it stays switched and does not report again.
        assert_eq!(config.note_overloaded(&mut state), None);
        assert_eq!(config.note_overloaded(&mut state), None);
        assert_eq!(state.consecutive_overloaded, 0);
    }

    #[test]
    fn no_fallback_configured_means_no_switch() {
        let config = RecoveryConfig::default();
        let mut state = RecoveryState::new();
        for _ in 0..10 {
            assert_eq!(config.note_overloaded(&mut state), None);
        }
        assert!(state.current_model.is_none());
    }
}
