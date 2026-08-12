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

use super::{log_event, SessionSlot, StopReason, TtsBackend, TtsRequest};
use crate::selection::{self, SelectionError, SelectionOutcome, SelectionRequest};

#[derive(Default)]
struct ControllerInner {
    app: Mutex<Option<AppHandle>>,
    backend: Mutex<Option<Arc<dyn TtsBackend>>>,
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
            if let Ok(mut slot) = self.inner.backend.lock() {
                *slot = Some(backend);
            }
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

    fn backend(&self) -> Option<Arc<dyn TtsBackend>> {
        self.inner.backend.lock().ok().and_then(|slot| slot.clone())
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

        let Some(backend) = self.backend() else {
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
        let Some(backend) = self.backend() else {
            log_event("speak_err", &[("error", "unsupported".to_string())]);
            return;
        };

        let token = self.inner.session.claim();
        thread::Builder::new()
            .name("voicex-tts-read".to_string())
            .spawn(move || {
                log_event("selection_start", &[]);
                let result = selection::read_selection(SelectionRequest {
                    app,
                    // Phase 0 hardcodes compatibility mode on; phase 2 exposes
                    // the setting. The fail-closed clipboard rules still apply.
                    allow_clipboard_fallback: true,
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
                        let request = TtsRequest::plain(outcome.text, outcome.sensitivity);
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

fn log_selection_ok(outcome: &SelectionOutcome) {
    let mut fields = vec![
        ("source", outcome.source.as_str().to_string()),
        ("sensitivity", outcome.sensitivity.as_str().to_string()),
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
