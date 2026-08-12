//! macOS system voice backend (AVSpeechSynthesizer).
//!
//! Speaks directly through the OS; no audio bytes and no playback layer are
//! involved, which is why [`TtsBackend`] does not force backends to produce
//! audio.
//!
//! Completion is detected by polling `isSpeaking` behind a start latch and a
//! start timeout. That is a deliberate phase-0 stand-in: phase 3 replaces it
//! with `AVSpeechSynthesizerDelegate`, which can tell finished, stopped,
//! cancelled and failed apart. The known gap until then is an utterance that
//! begins and ends entirely between two polls — it is reported as a start
//! timeout rather than a completion.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_avf_audio::{
    AVSpeechBoundary, AVSpeechSynthesisVoice, AVSpeechSynthesizer, AVSpeechUtterance,
};
use objc2_foundation::NSString;
use tauri::AppHandle;

use super::{log_event, CancelToken, TtsBackend, TtsError, TtsRequest, TtsStatus, TtsVoice};

/// How long a main-thread hop may take before we treat the engine as wedged.
const MAIN_THREAD_TIMEOUT_MS: u64 = 2_000;
/// How long the engine has to report `isSpeaking` after we ask it to speak.
const START_TIMEOUT_MS: u64 = 2_000;
const POLL_INTERVAL_MS: u64 = 30;

thread_local! {
    /// Owned by the main thread. Every access goes through [`on_main`].
    static SYNTHESIZER: RefCell<Option<Retained<AVSpeechSynthesizer>>> =
        const { RefCell::new(None) };
}

/// Run `body` on the main thread and wait for its result.
///
/// Must not be called from the main thread itself.
fn on_main<T, F>(app: &AppHandle, body: F) -> Result<T, TtsError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = tx.send(body());
    })
    .map_err(|err| TtsError::Backend(format!("run_on_main_thread failed: {err}")))?;

    rx.recv_timeout(Duration::from_millis(MAIN_THREAD_TIMEOUT_MS))
        .map_err(|_| TtsError::Backend("the main thread did not respond".to_string()))
}

/// Main thread only: hand the shared synthesizer to `body`, creating it lazily.
fn with_synthesizer<T>(body: impl FnOnce(&AVSpeechSynthesizer) -> T) -> T {
    SYNTHESIZER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let synthesizer = slot.get_or_insert_with(|| unsafe { AVSpeechSynthesizer::new() });
        body(synthesizer)
    })
}

#[derive(Default)]
struct BackendState {
    /// Whether the engine currently has an utterance. Only mutated by whoever
    /// owns the session token, so a poller that lost a race to a newer request
    /// cannot clear a live utterance's state.
    speaking: AtomicBool,
}

pub struct MacSystemBackend {
    app: AppHandle,
    state: Arc<BackendState>,
}

impl MacSystemBackend {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            state: Arc::new(BackendState::default()),
        }
    }

    fn spawn_completion_poller(&self, token: CancelToken) {
        let app = self.app.clone();
        let state = self.state.clone();
        thread::Builder::new()
            .name("voicex-tts-poll".to_string())
            .spawn(move || {
                let started = Instant::now();
                let mut observed_speaking = false;

                // Terminal outcomes go through the token: it both releases the
                // session and tells us whether this utterance is still the
                // current one, in one atomic step.
                let settle = |event: &str| {
                    if token.finish() {
                        state.speaking.store(false, Ordering::SeqCst);
                        log_event(event, &[]);
                    }
                };

                loop {
                    if token.is_cancelled() {
                        return;
                    }

                    // A main-thread stall tells us nothing about the engine, so
                    // retry rather than mistaking it for a finished utterance.
                    let Ok(is_speaking) =
                        on_main(&app, || with_synthesizer(|s| unsafe { s.isSpeaking() }))
                    else {
                        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
                        continue;
                    };

                    if is_speaking {
                        if !observed_speaking {
                            observed_speaking = true;
                            log_event("speak_started", &[]);
                        }
                    } else if observed_speaking {
                        settle("speak_finished");
                        return;
                    } else if started.elapsed() >= Duration::from_millis(START_TIMEOUT_MS) {
                        if token.finish() {
                            state.speaking.store(false, Ordering::SeqCst);
                            log_event(
                                "speak_err",
                                &[(
                                    "error",
                                    TtsError::StartTimeout(START_TIMEOUT_MS).code().to_string(),
                                )],
                            );
                        }
                        return;
                    }

                    thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
                }
            })
            .expect("failed to spawn the TTS completion poller");
    }
}

impl TtsBackend for MacSystemBackend {
    fn name(&self) -> &'static str {
        "mac_system"
    }

    fn list_voices(&self) -> Result<Vec<TtsVoice>, TtsError> {
        on_main(&self.app, || unsafe {
            AVSpeechSynthesisVoice::speechVoices()
                .iter()
                .map(|voice| TtsVoice {
                    id: voice.identifier().to_string(),
                    name: voice.name().to_string(),
                    language: voice.language().to_string(),
                })
                .collect()
        })
    }

    fn start(&self, request: TtsRequest, token: CancelToken) -> Result<(), TtsError> {
        let TtsRequest {
            text,
            voice,
            rate,
            volume,
            pitch,
        } = request;

        // `run_on_main_thread` cannot be un-queued: if the wait below times out,
        // the closure still runs later. It therefore re-checks the token itself
        // rather than trusting that the caller is still interested — otherwise a
        // timed-out start could begin speaking after the user already cancelled.
        let speak_token = token.clone();
        let queued = on_main(&self.app, move || {
            if speak_token.is_cancelled() {
                return false;
            }

            with_synthesizer(|synthesizer| unsafe {
                // Replace whatever is queued; a new request cancels the old one.
                synthesizer.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate);

                let utterance =
                    AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(&text));
                if let Some(rate) = rate {
                    utterance.setRate(rate.clamp(0.0, 1.0));
                }
                if let Some(volume) = volume {
                    utterance.setVolume(volume.clamp(0.0, 1.0));
                }
                if let Some(pitch) = pitch {
                    utterance.setPitchMultiplier(pitch.clamp(0.5, 2.0));
                }
                if let Some(identifier) = voice.as_deref() {
                    match AVSpeechSynthesisVoice::voiceWithIdentifier(&NSString::from_str(
                        identifier,
                    )) {
                        Some(voice) => utterance.setVoice(Some(&voice)),
                        None => log::warn!("Unknown system voice identifier: {identifier}"),
                    }
                }

                synthesizer.speakUtterance(&utterance);
            });
            true
        });

        match queued {
            Ok(true) => {
                self.state.speaking.store(true, Ordering::SeqCst);
                self.spawn_completion_poller(token);
                Ok(())
            }
            Ok(false) => {
                // Superseded between the caller's check and the main thread.
                token.finish();
                log_event("speak_discarded", &[("reason", "superseded".to_string())]);
                Ok(())
            }
            Err(err) => {
                // Release the session, which also cancels the queued closure
                // above so it cannot start speaking behind our back.
                token.finish();
                Err(err)
            }
        }
    }

    fn stop(&self) -> Result<(), TtsError> {
        self.state.speaking.store(false, Ordering::SeqCst);

        on_main(&self.app, || {
            with_synthesizer(|synthesizer| unsafe {
                synthesizer.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate);
            });
        })
    }

    fn status(&self) -> TtsStatus {
        if self.state.speaking.load(Ordering::SeqCst) {
            TtsStatus::Speaking
        } else {
            TtsStatus::Idle
        }
    }
}
