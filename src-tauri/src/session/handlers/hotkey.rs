use tauri::Emitter;

use crate::state::{AppState, HotkeySessionState, RecordingStyle};

use super::super::SessionController;

impl SessionController {
    pub fn on_hotkey_pressed(&self, state: &mut AppState) {
        self.cancel_auto_hide();

        let mut start_hold_timer = None;
        let mut start_recording = false;
        let prev_state = state.session_state;
        state.handle_hotkey_pressed();
        if state.session_state == HotkeySessionState::Pending
            && prev_state == HotkeySessionState::Idle
        {
            start_hold_timer = Some(state.hold_threshold_ms);
            start_recording = true;
        }

        if start_recording {
            match self.capture_foreground_app() {
                Ok(app_info) => {
                    log::info!(
                        "Hotkey target app captured: display_name={:?}, process_name={:?}, bundle_id={:?}, pid={}, is_self={}",
                        app_info.display_name,
                        app_info.process_name,
                        app_info.bundle_id,
                        app_info.process_id,
                        app_info.is_self
                    );
                    if !app_info.is_self {
                        if let Some(recent_app) = app_info.to_recent_target_app() {
                            if let Err(err) =
                                crate::storage::remember_recent_target_app(&recent_app)
                            {
                                log::warn!("Failed to remember target app: {}", err);
                            } else if let Some(app_handle) = self.app_handle() {
                                if let Err(err) = app_handle.emit(
                                    "input:recent-target-apps-updated",
                                    serde_json::json!({
                                        "appName": recent_app.app_name,
                                        "platform": recent_app.platform,
                                        "matchKind": recent_app.match_kind,
                                        "matchValue": recent_app.match_value,
                                    }),
                                ) {
                                    log::warn!(
                                        "Failed to emit recent target apps update event: {}",
                                        err
                                    );
                                }
                            }
                        }
                    }
                    state.session_target_app = Some(app_info);
                }
                Err(err) => {
                    log::warn!("Failed to capture hotkey target app: {}", err);
                    state.session_target_app = None;
                }
            }

            let (asr_model_name, llm_model_name) =
                crate::services::history_service::HistoryService::capture_model_snapshot();
            state.session_asr_model_name = asr_model_name;
            state.session_llm_model_name = llm_model_name;

            // Default HUD to hands-free icon immediately; will switch to push-to-talk if hold threshold is reached.
            state.recording_style = Some(RecordingStyle::HandsFree);
            self.start_audio_capture();
            self.start_recording_timeout(self.effective_max_recording_minutes(state));
            if let Some(hud) = self.hud_service() {
                hud.reset_display();
            } else {
                self.emit_transcript("", false);
            }
        }

        self.show_hud();
        if let Some(threshold) = start_hold_timer {
            self.start_hold_timer(threshold);
        }
        self.emit_state_from(state);
    }

    pub fn on_hotkey_released(&self, state: &mut AppState) {
        self.cancel_hold_timer();

        let was_recording = state.is_recording;
        let was_push_to_talk = state.session_state == HotkeySessionState::PushToTalk;
        state.handle_hotkey_released();

        // A PTT key-up is an explicit commit boundary. Freeze the best text we
        // already have before stopping capture so late provider events cannot
        // replace it. Hands-free never enters this branch.
        let ptt_release_committed =
            was_push_to_talk && state.commit_push_to_talk_release();
        if ptt_release_committed {
            self.emit_transcript(&state.session_final_text, true);
            self.cancel_asr_final_timeout();
            log::info!(
                "PTT release commit locked (len={}, had_provider_final={})",
                state.session_final_text.chars().count(),
                state.final_version > 0
            );
        }

        let schedule_finalize = state.session_state == HotkeySessionState::Finalizing;

        if was_recording && !state.is_recording {
            // Closing the local capture is still required so audio/history are
            // finalized, but injection no longer waits for the remote stream.
            self.stop_audio_capture("hotkey_release");
        }

        if ptt_release_committed {
            // We already own the commit snapshot. Cancel the provider transport
            // immediately; any already-queued ASR messages are ignored by the
            // PTT commit guards in the ASR handler.
            self.abort_asr_task();
            self.stop_asr_audio_bridge();
        }

        if schedule_finalize {
            self.emit_countdown(None);
            self.schedule_finalize_cleanup();
        }

        self.emit_state_from(state);
        if state.session_state == HotkeySessionState::Idle {
            self.set_escape_swallowing(false);
        }
    }

    pub fn on_hold_threshold_reached_state(&self, state: &mut AppState) {
        state.on_hold_threshold_reached();
        self.emit_state_from(state);
    }
}

#[cfg(test)]
mod ptt_fast_commit_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn enter_ptt(state: &mut AppState, now: Instant) {
        state.handle_hotkey_pressed_at(now);
        state.on_hold_threshold_reached();
        assert_eq!(state.session_state, HotkeySessionState::PushToTalk);
    }

    #[test]
    fn ptt_release_commits_existing_final_immediately() {
        let controller = SessionController::default();
        let mut state = AppState::new();
        let now = Instant::now();
        enter_ptt(&mut state, now);
        state.session_final_text = "provider final".to_string();
        state.transcript_text = "newer interim must not win".to_string();
        state.has_final_result = true;
        state.final_version = 1;

        controller.on_hotkey_released(&mut state);

        assert_eq!(state.session_state, HotkeySessionState::Finalizing);
        assert!(state.ptt_release_committed);
        assert!(state.asr_stream_finished);
        assert_eq!(state.session_final_text, "provider final");
        assert_eq!(state.transcript_text, "provider final");
    }

    #[test]
    fn ptt_release_uses_interim_as_commit_fallback() {
        let controller = SessionController::default();
        let mut state = AppState::new();
        let now = Instant::now();
        enter_ptt(&mut state, now);
        state.transcript_text = "latest HUD interim".to_string();

        controller.on_hotkey_released(&mut state);

        assert!(state.ptt_release_committed);
        assert!(state.has_final_result);
        assert_eq!(state.session_final_text, "latest HUD interim");
        assert_eq!(state.final_version, 1);
    }

    #[test]
    fn ptt_release_with_no_text_does_not_commit_empty_content() {
        let controller = SessionController::default();
        let mut state = AppState::new();
        let now = Instant::now();
        enter_ptt(&mut state, now);
        state.transcript_text = "   ".to_string();

        controller.on_hotkey_released(&mut state);

        assert_eq!(state.session_state, HotkeySessionState::Finalizing);
        assert!(!state.ptt_release_committed);
        assert!(!state.has_final_result);
        assert!(state.session_final_text.is_empty());
    }

    #[test]
    fn hands_free_stop_does_not_enable_ptt_release_commit() {
        let controller = SessionController::default();
        let mut state = AppState::new();
        state.translation_enabled = false;
        let now = Instant::now();
        state.handle_hotkey_pressed_at(now);
        state.handle_hotkey_released_at(now + Duration::from_millis(80));
        assert_eq!(state.session_state, HotkeySessionState::HandsFree);
        state.transcript_text = "hands free text".to_string();
        state.handle_hotkey_pressed_at(now + Duration::from_millis(600));

        controller.on_hotkey_released(&mut state);

        assert_eq!(state.session_state, HotkeySessionState::Finalizing);
        assert!(!state.ptt_release_committed);
        assert!(!state.asr_stream_finished);
    }
}

