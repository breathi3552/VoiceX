//! Text-to-speech subsystem for the selected-text reading feature.
//!
//! [`TtsBackend`] is the platform- and vendor-neutral control surface. The
//! macOS system voice implements it directly (it speaks without ever producing
//! audio bytes); a cloud backend composes synthesis and playback behind the
//! same trait.

pub mod controller;
#[cfg(target_os = "macos")]
pub mod mac_system;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use controller::TtsController;

use crate::selection::Sensitivity;

/// Log target for the subsystem's structured events.
///
/// The automation harness under `scripts/tts/` asserts on these lines, so the
/// `event=` vocabulary and field names are part of the test contract: change
/// them and the scripts together.
/// Kept under the crate's own module path so an ordinary `RUST_LOG=voicex_lib`
/// filter still shows them.
pub const LOG_TARGET: &str = "voicex_lib::tts";

/// Emit one structured event line: `event=<name> key=value ...`.
///
/// Values are sanitized to stay whitespace-free so the lines remain trivially
/// splittable. Never pass selected text through here — only lengths, sources
/// and error codes (plan §3.4: no full text in ordinary logs).
pub fn log_event(event: &str, fields: &[(&str, String)]) {
    let mut line = String::from("event=");
    line.push_str(event);
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&sanitize_field(value));
    }
    log::info!(target: LOG_TARGET, "{line}");
}

fn sanitize_field(value: &str) -> String {
    if value.is_empty() {
        return "-".to_string();
    }
    value
        .chars()
        .map(|c| {
            if c.is_whitespace() || c == '=' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct TtsRequest {
    pub text: String,
    /// How much is known about the control the text came from. Carried on the
    /// request because plan §3.4 makes it a backend-level rule: a cloud backend
    /// must refuse anything that is not [`Sensitivity::Safe`]. The local system
    /// voice is unaffected.
    pub sensitivity: Sensitivity,
    /// Backend-specific voice identifier; `None` uses the engine default.
    pub voice: Option<String>,
    /// Normalized 0.0..=1.0 speaking rate; `None` uses the engine default.
    /// The user-facing scale is defined in phase 3 along with the settings UI.
    pub rate: Option<f32>,
    /// Normalized 0.0..=1.0 volume; `None` uses the engine default.
    pub volume: Option<f32>,
}

impl TtsRequest {
    pub fn plain(text: String, sensitivity: Sensitivity) -> Self {
        Self {
            text,
            sensitivity,
            voice: None,
            rate: None,
            volume: None,
        }
    }
}

/// Ownership of the read-and-speak session.
///
/// One `AtomicU64` holds the generation that currently owns the session, or 0
/// when nothing does. Superseding is a plain store of a newer generation, which
/// instantly makes every older [`CancelToken`] report cancelled — including one
/// captured by a closure already queued on the main thread, which is the only
/// way to stop work that `run_on_main_thread` will run whether or not the
/// caller is still waiting for it.
#[derive(Clone, Default)]
pub struct SessionSlot {
    owner: Arc<AtomicU64>,
    counter: Arc<AtomicU64>,
}

impl SessionSlot {
    /// Take the session for a new request, cancelling any current owner.
    pub fn claim(&self) -> CancelToken {
        let generation = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.owner.store(generation, Ordering::SeqCst);
        CancelToken {
            owner: self.owner.clone(),
            generation,
        }
    }

    /// Cancel the current owner and leave the session idle.
    pub fn release(&self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
        self.owner.store(0, Ordering::SeqCst);
    }

    pub fn is_active(&self) -> bool {
        self.owner.load(Ordering::SeqCst) != 0
    }

    /// Lock-free view for the keyboard hook, which must not take locks inside
    /// the event tap callback.
    pub fn active_handle(&self) -> Arc<AtomicU64> {
        self.owner.clone()
    }
}

#[derive(Clone)]
pub struct CancelToken {
    owner: Arc<AtomicU64>,
    generation: u64,
}

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        self.owner.load(Ordering::SeqCst) != self.generation
    }

    /// Give the session back if this generation still owns it.
    ///
    /// Returns false when a newer request already took over — in which case the
    /// caller must not touch shared state, because it no longer describes this
    /// request.
    pub fn finish(&self) -> bool {
        self.owner
            .compare_exchange(self.generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsStatus {
    Idle,
    Speaking,
}

#[derive(Debug, Clone)]
pub struct TtsVoice {
    pub id: String,
    pub name: String,
    pub language: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TtsError {
    #[error("No text-to-speech backend is available on this platform")]
    Unsupported,

    #[error("The speech engine did not start within {0} ms")]
    StartTimeout(u64),

    #[error("Speech engine failure: {0}")]
    Backend(String),
}

impl TtsError {
    pub fn code(&self) -> &'static str {
        match self {
            TtsError::Unsupported => "unsupported",
            TtsError::StartTimeout(_) => "start_timeout",
            TtsError::Backend(_) => "backend",
        }
    }
}

/// Why speech ended, for the structured log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// User pressed the read hotkey while speech was active.
    Hotkey,
    /// User pressed Escape while speech was active.
    Escape,
    /// Dictation started and takes priority over reading.
    Dictation,
    /// A newer read request superseded this one.
    Superseded,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Hotkey => "hotkey",
            StopReason::Escape => "escape",
            StopReason::Dictation => "dictation",
            StopReason::Superseded => "superseded",
        }
    }
}

pub trait TtsBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn list_voices(&self) -> Result<Vec<TtsVoice>, TtsError>;
    /// Begin speaking.
    ///
    /// Returns once the engine has accepted the request; completion is reported
    /// asynchronously through the structured log. The backend owns `token` for
    /// the rest of the utterance: it must check [`CancelToken::is_cancelled`]
    /// before any irreversible step, and call [`CancelToken::finish`] on every
    /// terminal outcome — completion, failure, or refusal — so the session
    /// cannot be left stuck as active.
    fn start(&self, request: TtsRequest, token: CancelToken) -> Result<(), TtsError>;
    fn stop(&self) -> Result<(), TtsError>;
    fn status(&self) -> TtsStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_values_stay_single_token() {
        assert_eq!(sanitize_field("Google Chrome"), "Google_Chrome");
        assert_eq!(sanitize_field("a=b"), "a_b");
        assert_eq!(sanitize_field(""), "-");
    }
}
