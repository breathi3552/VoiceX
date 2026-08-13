//! Text-to-speech subsystem for the selected-text reading feature.
//!
//! [`TtsBackend`] is the platform- and vendor-neutral control surface. The
//! macOS system voice implements it directly (it speaks without ever producing
//! audio bytes); a cloud backend composes synthesis and playback behind the
//! same trait.
//!
//! On macOS the empty-id "system default" path goes through [`mac_say`] so it
//! can use the Spoken Content / Siri voice; a listed compact voice still goes
//! through [`mac_system`].

pub mod aliyun;
pub mod controller;
pub mod decode;
#[cfg(target_os = "macos")]
pub mod mac_say;
#[cfg(target_os = "macos")]
pub mod mac_system;
pub mod playback;
pub mod volcengine;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use controller::TtsController;

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
    /// Backend-specific voice identifier; `None` uses the engine default.
    pub voice: Option<String>,
    /// Normalized 0.0..=1.0 speaking rate; `None` uses the engine default.
    /// The user-facing scale is 0.5x–2x, converted in the settings UI.
    pub rate: Option<f32>,
    /// Normalized 0.0..=1.0 volume; `None` uses the engine default.
    pub volume: Option<f32>,
    /// Pitch multiplier in the engine's own 0.5..=2.0 scale; `None` uses the
    /// engine default. Unlike rate and volume this one is not normalized —
    /// 1.0 is the neutral value and both ends are meaningful, so squashing it
    /// into 0..1 would only hide where "unchanged" sits.
    pub pitch: Option<f32>,
}

impl TtsRequest {
    /// A request that leaves every voice parameter at the engine default.
    pub fn plain(text: String) -> Self {
        Self {
            text,
            voice: None,
            rate: None,
            volume: None,
            pitch: None,
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
    /// Stopped from the app's own UI — today, the settings-page preview.
    /// Distinct from `Hotkey` so the harness assertion on `reason=hotkey`
    /// keeps meaning "the global hotkey did it".
    Ui,
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
            StopReason::Ui => "ui",
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

    /// Recent output level in 0..=1, for the HUD waveform.
    ///
    /// `None` means this backend cannot know: the macOS system voice speaks
    /// straight through the OS and never hands over audio (plan §5.3). The HUD
    /// hides the waveform in that case rather than animating a made-up one.
    fn audio_level(&self) -> Option<f32> {
        None
    }

    /// Longest text this backend will accept, if it has a limit.
    ///
    /// `None` means unbounded, which is the case for the local voice — it costs
    /// nothing per character and can be stopped at any moment. Cloud providers
    /// are metered and reject oversized requests outright, so the caller
    /// truncates rather than spending a request that comes back as an error
    /// (plan §4.5).
    fn max_chars(&self) -> Option<usize> {
        None
    }
}

/// Cut `text` down to `limit` characters, preferring a sentence boundary.
///
/// Truncation is going to be noticed, so it should at least land somewhere a
/// human would pause. Falls back to the hard cut when the tail holds no
/// boundary — a wall of text with no punctuation is exactly the case where
/// hunting for one would throw away most of what fits.
pub fn truncate_for_backend(text: &str, limit: usize) -> String {
    /// How far back to look for a sentence end. Everything here counts
    /// characters, never bytes — a byte-based window silently shrinks to a
    /// third of its intended size on Chinese text.
    const SENTENCE_LOOKBACK: usize = 200;

    if text.chars().count() <= limit {
        return text.to_string();
    }

    let clipped: Vec<char> = text.chars().take(limit).collect();
    // A fixed window, not a proportion of the limit: at 5000 characters a
    // proportional one would happily discard hundreds just to land on a full
    // stop, which is a worse outcome than the truncation it is smoothing over.
    let start = clipped.len().saturating_sub(SENTENCE_LOOKBACK);
    let boundary = clipped[start..]
        .iter()
        .rposition(|ch| matches!(ch, '。' | '！' | '？' | '.' | '!' | '?' | '\n'))
        .map(|offset| start + offset + 1);

    match boundary {
        Some(end) => clipped[..end].iter().collect(),
        None => clipped.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_within_the_limit_is_left_exactly_as_it_is() {
        assert_eq!(truncate_for_backend("你好，世界。", 100), "你好，世界。");
        // Exactly at the limit is still within it.
        assert_eq!(truncate_for_backend("abcde", 5), "abcde");
    }

    #[test]
    fn truncation_prefers_a_sentence_boundary_near_the_cut() {
        // Cutting mid-sentence is audible; ending on a full stop is not.
        let text = "第一句话。第二句话。第三句话在这里被截断";
        let cut = truncate_for_backend(text, 12);
        assert_eq!(cut, "第一句话。第二句话。");
        assert!(cut.chars().count() <= 12);
    }

    #[test]
    fn a_wall_of_text_with_no_punctuation_is_cut_at_the_limit() {
        // Hunting further back for a boundary would throw away most of what
        // fits, which is worse than the truncation itself.
        let text = "あ".repeat(50);
        let cut = truncate_for_backend(&text, 20);
        assert_eq!(cut.chars().count(), 20);
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        // A multibyte cap measured in bytes would cut a third of the text and,
        // worse, could land inside a character.
        let text = "中".repeat(10);
        assert_eq!(truncate_for_backend(&text, 6).chars().count(), 6);
    }

    #[test]
    fn field_values_stay_single_token() {
        assert_eq!(sanitize_field("Google Chrome"), "Google_Chrome");
        assert_eq!(sanitize_field("a=b"), "a_b");
        assert_eq!(sanitize_field(""), "-");
    }
}
