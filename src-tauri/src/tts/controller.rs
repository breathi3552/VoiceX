//! Session control for selected-text reading.
//!
//! Phase 0 keeps this deliberately small, but the cancellation contract is
//! already the real one: a single [`SessionSlot`] owns "a read-and-speak is in
//! progress", and every stage carries the [`CancelToken`] it was started with.
//! Phase 3 grows this into the full `TtsSession` state machine
//! (`Idle -> ReadingSelection -> Synthesizing -> Playing -> Idle`) — the log
//! events emitted here are already the assertion surface it will keep.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use tauri::AppHandle;

use super::volcengine::{self, VolcengineBackend, VolcengineConfig};
use super::{log_event, SessionSlot, StopReason, TtsBackend, TtsError, TtsRequest, TtsVoice};
use crate::commands::settings::AppSettings;
use crate::selection::{self, SelectionError, SelectionOutcome, SelectionRequest};

/// Longest preview we will speak. The settings page sends a short fixed
/// sentence; the cap only stops a malformed call from starting a long read.
const PREVIEW_MAX_CHARS: usize = 500;

/// Settings value selecting the cloud backend.
const PROVIDER_VOLCENGINE: &str = "volcengine";

#[derive(Default)]
struct ControllerInner {
    app: Mutex<Option<AppHandle>>,
    /// The macOS system voice. Absent on other platforms.
    system: Mutex<Option<Arc<dyn TtsBackend>>>,
    /// Kept as its concrete type so credentials can be re-applied without
    /// rebuilding it — the settings page changes them while the app runs.
    volcengine: Mutex<Option<Arc<VolcengineBackend>>>,
    /// Whichever backend owns the current session. `stop` has to reach that
    /// one, not whichever provider the settings happen to name right now.
    active: Mutex<Option<Arc<dyn TtsBackend>>>,
    /// Owns the whole read-and-speak lifetime, from the moment a read starts
    /// until speech ends, fails, or is stopped. One flag, one owner — the
    /// previous split between a `reading` bool and the backend's own state had
    /// windows where neither said "busy" and a second hotkey press started a
    /// second read instead of stopping the first.
    session: SessionSlot,
    /// Set while dictation is recording. Reading is refused then: the speech
    /// would be picked up by the microphone and transcribed.
    recording: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Clone, Default)]
pub struct TtsController {
    inner: Arc<ControllerInner>,
}

impl TtsController {
    pub fn init_with_handle(&self, app: &AppHandle) {
        if let Ok(mut slot) = self.inner.app.lock() {
            *slot = Some(app.clone());
        }

        #[cfg(target_os = "macos")]
        {
            let backend: Arc<dyn TtsBackend> =
                Arc::new(super::mac_system::MacSystemBackend::new(app.clone()));
            if let Ok(mut slot) = self.inner.system.lock() {
                *slot = Some(backend);
            }
        }

        // Built unconditionally: it is network-only, so it works wherever the
        // app runs, including where there is no system voice at all.
        if let Ok(mut slot) = self.inner.volcengine.lock() {
            *slot = Some(Arc::new(VolcengineBackend::new(VolcengineConfig {
                api_key: String::new(),
                resource_id: volcengine::DEFAULT_RESOURCE_ID.to_string(),
            })));
        }
    }

    /// Share the dictation session's recording flag so reads can be refused
    /// while the microphone is live.
    pub fn attach_recording_flag(&self, recording: Arc<AtomicBool>) {
        if let Ok(mut slot) = self.inner.recording.lock() {
            *slot = Some(recording);
        }
    }

    /// Lock-free "is a read or speech in progress" view for the keyboard hook,
    /// which must not take locks inside the event tap callback.
    pub fn active_handle(&self) -> Arc<AtomicU64> {
        self.inner.session.active_handle()
    }

    fn app(&self) -> Option<AppHandle> {
        self.inner.app.lock().ok().and_then(|slot| slot.clone())
    }

    /// Resolve the backend the settings ask for, applying its credentials.
    ///
    /// Falls back to the system voice when the cloud provider is selected but
    /// unavailable, rather than refusing to speak — but says so in the log, so
    /// "why does it sound like the local voice" has an answer.
    fn backend_for(&self, settings: Option<&AppSettings>) -> Option<Arc<dyn TtsBackend>> {
        let provider = settings.map(|s| s.tts_provider_type.as_str()).unwrap_or("");

        if provider == PROVIDER_VOLCENGINE {
            let cloud = self
                .inner
                .volcengine
                .lock()
                .ok()
                .and_then(|slot| slot.clone());
            if let (Some(cloud), Some(settings)) = (cloud, settings) {
                cloud.apply_config(VolcengineConfig {
                    api_key: settings.volc_tts_api_key.clone(),
                    resource_id: settings.volc_tts_resource_id.clone(),
                });
                return Some(cloud as Arc<dyn TtsBackend>);
            }
            log_event("backend_fallback", &[("provider", provider.to_string())]);
        }

        self.inner.system.lock().ok().and_then(|slot| slot.clone())
    }

    /// The backend that owns the session right now, for stopping it.
    fn active_backend(&self) -> Option<Arc<dyn TtsBackend>> {
        self.inner.active.lock().ok().and_then(|slot| slot.clone())
    }

    fn set_active_backend(&self, backend: Option<Arc<dyn TtsBackend>>) {
        if let Ok(mut slot) = self.inner.active.lock() {
            *slot = backend;
        }
    }

    fn is_recording(&self) -> bool {
        self.inner
            .recording
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|flag| flag.load(Ordering::SeqCst)))
            .unwrap_or(false)
    }

    pub fn is_active(&self) -> bool {
        self.inner.session.is_active()
    }

    /// Voices the active backend can speak with, for the settings page.
    ///
    /// Blocking, and hops to the main thread — never call from there.
    pub fn list_voices(&self) -> Result<Vec<TtsVoice>, TtsError> {
        let settings = load_settings();
        self.backend_for(settings.as_ref())
            .ok_or(TtsError::Unsupported)
            .and_then(|backend| backend.list_voices())
    }

    /// Speak a fixed sample with the current voice settings.
    ///
    /// Deliberately skips the selection reader: the settings page needs to
    /// audition rate, pitch and voice without a foreground app or a selection.
    /// It shares the session slot with reading, so the read hotkey stops a
    /// preview and a second click supersedes the first.
    ///
    /// Allowed even when the master switch is off — auditioning a voice before
    /// turning the feature on is the point of the button.
    ///
    /// Blocking, and hops to the main thread — never call from there.
    pub fn speak_preview(&self, text: String) -> Result<(), TtsError> {
        if self.is_recording() {
            return Err(TtsError::Backend("dictation is recording".to_string()));
        }
        let settings = load_settings();
        let backend = self
            .backend_for(settings.as_ref())
            .ok_or(TtsError::Unsupported)?;

        let text: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
        if text.trim().is_empty() {
            return Err(TtsError::Backend("nothing to preview".to_string()));
        }

        let token = self.inner.session.claim();
        self.set_active_backend(Some(backend.clone()));
        log_event(
            "speak_start",
            &[
                ("backend", backend.name().to_string()),
                ("chars", text.chars().count().to_string()),
                ("origin", "preview".to_string()),
            ],
        );

        let request = match settings {
            Some(settings) => voice_request(&settings, text),
            None => TtsRequest::plain(text),
        };
        backend.start(request, token).inspect_err(|err| {
            log_event(
                "speak_err",
                &[
                    ("error", err.code().to_string()),
                    ("detail", err.to_string()),
                ],
            );
        })
    }

    /// The read-selection hotkey fired.
    ///
    /// Single-click semantics (plan §3.3): idle reads and speaks, anything else
    /// stops. Returns immediately — the read runs on a worker thread because
    /// the clipboard fallback can block for hundreds of milliseconds and this
    /// is called from the hotkey hook's worker.
    pub fn handle_read_selection_hotkey(&self) {
        let active = self.is_active();
        log_event(
            "hotkey_action",
            &[
                ("action", "read_selection".to_string()),
                ("state", if active { "active" } else { "idle" }.to_string()),
            ],
        );

        if active {
            self.stop(StopReason::Hotkey);
        } else {
            self.start();
        }
    }

    /// Stop reading because dictation is taking over. No-op when idle, so the
    /// dictation hotkey stays cheap.
    pub fn stop_for_dictation(&self) {
        if self.is_active() {
            self.stop(StopReason::Dictation);
        }
    }

    pub fn stop(&self, reason: StopReason) {
        // Release the session first: this cancels every outstanding token, so
        // an in-flight read discards its result and a queued main-thread speak
        // aborts instead of starting after the user asked to stop.
        self.inner.session.release();

        log_event("speak_stop", &[("reason", reason.as_str().to_string())]);

        let Some(backend) = self.active_backend() else {
            return;
        };
        match backend.stop() {
            // Distinct from `speak_stop`, which only records the request: this
            // is the engine confirming it actually stopped.
            Ok(()) => log_event("speak_stopped", &[("reason", reason.as_str().to_string())]),
            Err(err) => log_event(
                "speak_err",
                &[
                    ("error", err.code().to_string()),
                    ("detail", err.to_string()),
                ],
            ),
        }
    }

    fn start(&self) {
        if self.is_recording() {
            // Dictation wins: reading aloud during recording would feed the
            // speech straight back into the microphone.
            log_event("speak_err", &[("error", "recording_active".to_string())]);
            return;
        }

        let Some(app) = self.app() else {
            log_event("speak_err", &[("error", "not_initialized".to_string())]);
            return;
        };

        // Settings decide which backend speaks, so they have to be read before
        // the session is claimed. This runs on the hotkey hook's worker, and a
        // database read is cheap enough not to matter there.
        let settings = load_settings();
        let Some(backend) = self.backend_for(settings.as_ref()) else {
            log_event("speak_err", &[("error", "unsupported".to_string())]);
            return;
        };

        let token = self.inner.session.claim();
        self.set_active_backend(Some(backend.clone()));
        thread::Builder::new()
            .name("voicex-tts-read".to_string())
            .spawn(move || {
                log_event("selection_start", &[]);
                let result = selection::read_selection(SelectionRequest {
                    app,
                    // Compatibility mode, off by choice in the reading settings.
                    // The fail-closed clipboard rules apply either way.
                    allow_clipboard_fallback: settings
                        .as_ref()
                        .map(|s| s.tts_clipboard_fallback)
                        .unwrap_or(true),
                });

                // The session stays claimed across the handoff to the backend,
                // so there is no window where a hotkey press sees "idle" and
                // starts a second read instead of stopping this one.
                if token.is_cancelled() {
                    log_event(
                        "selection_discarded",
                        &[("reason", "superseded".to_string())],
                    );
                    return;
                }

                match result {
                    Ok(outcome) => {
                        log_selection_ok(&outcome);
                        log_event(
                            "speak_start",
                            &[
                                ("backend", backend.name().to_string()),
                                ("chars", outcome.text.chars().count().to_string()),
                            ],
                        );
                        let request = match settings {
                            Some(settings) => voice_request(&settings, outcome.text),
                            None => TtsRequest::plain(outcome.text),
                        };
                        if let Err(err) = backend.start(request, token) {
                            log_event(
                                "speak_err",
                                &[
                                    ("error", err.code().to_string()),
                                    ("detail", err.to_string()),
                                ],
                            );
                        }
                    }
                    Err(err) => {
                        log_selection_error(&err);
                        // Nothing will speak, so hand the session back — but
                        // only if a newer request has not already claimed it.
                        token.finish();
                    }
                }
            })
            .expect("failed to spawn the TTS read worker");
    }
}

/// Persisted settings, or `None` when the store cannot be read.
///
/// A failure here must not stop the user from being read to, so callers fall
/// back to engine defaults — loudly, because silently speaking in the wrong
/// voice is the kind of thing that gets reported as "the setting does nothing".
fn load_settings() -> Option<AppSettings> {
    match crate::storage::get_settings() {
        Ok(settings) => Some(settings),
        Err(err) => {
            log::warn!("Falling back to default voice parameters: {err}");
            log_event("settings_err", &[("error", err.to_string())]);
            None
        }
    }
}

/// Build a request carrying the user's voice parameters.
///
/// Rate and volume are stored normalized (0..=1) and go straight through; each
/// backend maps them onto its own scale. Pitch is macOS-only — Volcengine has
/// no such parameter and ignores it (plan §5.4).
///
/// The voice id is **per provider**: a macOS voice identifier means nothing to
/// a cloud provider and vice versa, so they are stored separately and picked
/// here. An empty id means "backend default", which is not the same as a voice
/// literally named "".
fn voice_request(settings: &AppSettings, text: String) -> TtsRequest {
    let voice = if settings.tts_provider_type == PROVIDER_VOLCENGINE {
        settings.volc_tts_speaker.clone()
    } else {
        settings.tts_voice_id.clone()
    };

    TtsRequest {
        text,
        voice: Some(voice).filter(|id| !id.trim().is_empty()),
        rate: Some(settings.tts_rate),
        volume: Some(settings.tts_volume),
        pitch: Some(settings.tts_pitch),
    }
}

fn log_selection_ok(outcome: &SelectionOutcome) {
    let mut fields = vec![
        ("source", outcome.source.as_str().to_string()),
        ("chars", outcome.text.chars().count().to_string()),
        ("elapsed_ms", outcome.elapsed_ms.to_string()),
        (
            "app",
            outcome
                .app_bundle_id
                .clone()
                .or_else(|| outcome.app_name.clone())
                .unwrap_or_default(),
        ),
    ];
    if let Some(restored) = outcome.clipboard_restored {
        fields.push(("clipboard_restored", restored.to_string()));
    }
    log_event("selection_ok", &fields);
}

fn log_selection_error(err: &SelectionError) {
    let mut fields = vec![("error", err.code().to_string())];
    if let SelectionError::ClipboardSnapshotRefused(reason) = err {
        fields.push(("detail", reason.clone()));
    }
    log_event("selection_err", &fields);
}

#[cfg(test)]
mod tests {
    use super::super::SessionSlot;
    use super::voice_request;
    use crate::commands::settings::AppSettings;

    #[test]
    fn an_unset_voice_means_engine_default_not_a_voice_named_empty() {
        let mut settings = AppSettings::default();
        assert!(settings.tts_voice_id.is_empty());
        assert_eq!(voice_request(&settings, "hi".to_string()).voice, None);

        settings.tts_voice_id = "com.apple.voice.compact.zh-CN.Tingting".to_string();
        assert_eq!(
            voice_request(&settings, "hi".to_string()).voice.as_deref(),
            Some("com.apple.voice.compact.zh-CN.Tingting")
        );
    }

    #[test]
    fn voice_parameters_reach_the_request_on_their_stored_scales() {
        // Rate and volume stay normalized; pitch keeps the engine's own scale,
        // where 1.0 is neutral rather than the midpoint of the range.
        let settings = AppSettings::default();
        let request = voice_request(&settings, "hi".to_string());
        assert_eq!(request.rate, Some(0.5), "0.5 is the engine's 1x mark");
        assert_eq!(request.volume, Some(1.0));
        assert_eq!(request.pitch, Some(1.0));
    }

    #[test]
    fn a_new_claim_cancels_the_previous_one() {
        let slot = SessionSlot::default();
        let first = slot.claim();
        assert!(!first.is_cancelled());

        let second = slot.claim();
        assert!(first.is_cancelled(), "superseded request must see cancel");
        assert!(!second.is_cancelled());
    }

    #[test]
    fn release_cancels_and_leaves_the_slot_idle() {
        let slot = SessionSlot::default();
        let token = slot.claim();
        assert!(slot.is_active());

        slot.release();
        assert!(!slot.is_active());
        assert!(token.is_cancelled());
        assert!(!token.finish(), "a cancelled token must not release again");
    }

    #[test]
    fn only_the_owner_can_finish() {
        let slot = SessionSlot::default();
        let stale = slot.claim();
        let current = slot.claim();

        assert!(!stale.finish(), "stale token must not clear a live session");
        assert!(slot.is_active(), "the live session survives a stale finish");

        assert!(current.finish());
        assert!(!slot.is_active());
    }

    #[test]
    fn finishing_twice_releases_once() {
        let slot = SessionSlot::default();
        let token = slot.claim();
        assert!(token.finish());
        assert!(!token.finish());
    }

    #[test]
    fn a_finished_session_is_idle_so_the_hotkey_starts_a_new_read() {
        let slot = SessionSlot::default();
        let token = slot.claim();
        assert!(
            slot.is_active(),
            "hotkey during a read must stop, not start"
        );
        token.finish();
        assert!(!slot.is_active(), "hotkey after completion must start anew");
    }
}
