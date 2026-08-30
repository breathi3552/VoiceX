#!/usr/bin/env python3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def load(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def save(path: str, text: str) -> None:
    (ROOT / path).write_text(text.replace("\r\n", "\n"), encoding="utf-8", newline="\n")


def replace_once(path: str, old: str, new: str) -> None:
    text = load(path)
    if new in text:
        return
    if old not in text:
        raise RuntimeError(f"anchor missing in {path}: {old[:140]!r}")
    save(path, text.replace(old, new, 1))


def patch_state() -> None:
    path = "src-tauri/src/state.rs"
    text = load(path)

    field_old = "    pub(crate) final_injected: bool,\n    pub(crate) last_injected_text: String,\n"
    field_new = "    pub(crate) final_injected: bool,\n    pub(crate) ptt_release_committed: bool,\n    pub(crate) last_injected_text: String,\n"
    if field_new not in text:
        if field_old not in text:
            raise RuntimeError("state field anchor missing")
        text = text.replace(field_old, field_new, 1)

    init_old = "            final_injected: false,\n            last_injected_text: String::new(),\n"
    init_new = "            final_injected: false,\n            ptt_release_committed: false,\n            last_injected_text: String::new(),\n"
    if init_new not in text:
        if init_old not in text:
            raise RuntimeError("state init anchor missing")
        text = text.replace(init_old, init_new, 1)

    session_reset_old = "                self.final_injected = false;\n                self.has_final_result = false;\n                self.last_injected_text.clear();\n"
    session_reset_new = "                self.final_injected = false;\n                self.ptt_release_committed = false;\n                self.has_final_result = false;\n                self.last_injected_text.clear();\n"
    if session_reset_new not in text:
        if session_reset_old not in text:
            raise RuntimeError("new-session reset anchor missing")
        text = text.replace(session_reset_old, session_reset_new, 1)

    full_reset_old = "        self.final_injected = false;\n        self.has_final_result = false;\n        self.last_injected_text.clear();\n"
    full_reset_new = "        self.final_injected = false;\n        self.ptt_release_committed = false;\n        self.has_final_result = false;\n        self.last_injected_text.clear();\n"
    if full_reset_new not in text:
        if full_reset_old not in text:
            raise RuntimeError("full reset anchor missing")
        text = text.replace(full_reset_old, full_reset_new, 1)

    method = '''    /// Lock the text that a push-to-talk release explicitly commits.\n    ///\n    /// A real ASR final wins. If no final exists yet, the latest HUD/interim\n    /// transcript becomes the committed text. An empty snapshot means there is\n    /// nothing to inject, so the normal completion path remains in control.\n    pub(crate) fn commit_push_to_talk_release(&mut self) -> bool {\n        if self.session_state != HotkeySessionState::Finalizing\n            || self.recording_style != Some(RecordingStyle::PushToTalk)\n        {\n            return false;\n        }\n\n        let has_usable_final = self.has_final_result && !self.session_final_text.trim().is_empty();\n        let commit_text = if has_usable_final {\n            self.session_final_text.clone()\n        } else {\n            self.transcript_text.clone()\n        };\n\n        if commit_text.trim().is_empty() {\n            self.ptt_release_committed = false;\n            return false;\n        }\n\n        if !has_usable_final {\n            self.session_final_text = commit_text;\n            self.has_final_result = true;\n            self.final_version = self.final_version.saturating_add(1);\n        }\n\n        self.transcript_text = self.session_final_text.clone();\n        self.last_injected_text = self.session_final_text.clone();\n        self.ptt_release_committed = true;\n\n        // Release is the terminal recognition boundary for this PTT session.\n        // This deliberately bypasses stream-finished/final-timeout/refinement\n        // gates; the controller also cancels the live ASR transport.\n        self.asr_stream_finished = true;\n        self.asr_reconnect_in_progress = false;\n        self.asr_refinement_in_progress = false;\n        self.asr_refinement_done = true;\n        true\n    }\n\n'''
    anchor = "    fn should_upgrade_to_translate(&self, now: Instant) -> bool {\n"
    if method not in text:
        if anchor not in text:
            raise RuntimeError("PTT commit method anchor missing")
        text = text.replace(anchor, method + anchor, 1)

    save(path, text)


def patch_hotkey_handler() -> None:
    path = "src-tauri/src/session/handlers/hotkey.rs"
    text = load(path)

    old = '''    pub fn on_hotkey_released(&self, state: &mut AppState) {\n        self.cancel_hold_timer();\n\n        let was_recording = state.is_recording;\n        state.handle_hotkey_released();\n\n        let mut schedule_finalize = false;\n        if state.session_state == HotkeySessionState::Finalizing {\n            schedule_finalize = true;\n        }\n\n        if was_recording && !state.is_recording {\n            self.stop_audio_capture("hotkey_release");\n        }\n\n        if schedule_finalize {\n            self.emit_countdown(None);\n            self.schedule_finalize_cleanup();\n        }\n\n        self.emit_state_from(state);\n        if state.session_state == HotkeySessionState::Idle {\n            self.set_escape_swallowing(false);\n        }\n    }\n'''
    new = '''    pub fn on_hotkey_released(&self, state: &mut AppState) {\n        self.cancel_hold_timer();\n\n        let was_recording = state.is_recording;\n        let was_push_to_talk = state.session_state == HotkeySessionState::PushToTalk;\n        state.handle_hotkey_released();\n\n        // A PTT key-up is an explicit commit boundary. Freeze the best text we\n        // already have before stopping capture so late provider events cannot\n        // replace it. Hands-free never enters this branch.\n        let ptt_release_committed =\n            was_push_to_talk && state.commit_push_to_talk_release();\n        if ptt_release_committed {\n            self.emit_transcript(&state.session_final_text, true);\n            self.cancel_asr_final_timeout();\n            log::info!(\n                "PTT release commit locked (len={}, had_provider_final={})",\n                state.session_final_text.chars().count(),\n                state.final_version > 0\n            );\n        }\n\n        let schedule_finalize = state.session_state == HotkeySessionState::Finalizing;\n\n        if was_recording && !state.is_recording {\n            // Closing the local capture is still required so audio/history are\n            // finalized, but injection no longer waits for the remote stream.\n            self.stop_audio_capture("hotkey_release");\n        }\n\n        if ptt_release_committed {\n            // We already own the commit snapshot. Cancel the provider transport\n            // immediately; any already-queued ASR messages are ignored by the\n            // PTT commit guards in the ASR handler.\n            self.abort_asr_task();\n            self.stop_asr_audio_bridge();\n        }\n\n        if schedule_finalize {\n            self.emit_countdown(None);\n            self.schedule_finalize_cleanup();\n        }\n\n        self.emit_state_from(state);\n        if state.session_state == HotkeySessionState::Idle {\n            self.set_escape_swallowing(false);\n        }\n    }\n'''
    if new not in text:
        if old not in text:
            raise RuntimeError("hotkey release handler anchor missing")
        text = text.replace(old, new, 1)

    tests = r'''

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
'''
    if "mod ptt_fast_commit_tests" not in text:
        text = text.rstrip() + tests + "\n"

    save(path, text)


def patch_asr_handler() -> None:
    path = "src-tauri/src/session/handlers/asr.rs"
    text = load(path)

    event_old = '''    pub fn handle_asr_event_state(&self, state: &mut AppState, evt: AsrEvent) {\n        if !state.asr_received_event {\n'''
    event_new = '''    pub fn handle_asr_event_state(&self, state: &mut AppState, evt: AsrEvent) {\n        if state.ptt_release_committed {\n            log::debug!(\n                "Dropping ASR {} after PTT release commit",\n                if evt.is_final { "final" } else { "partial" }\n            );\n            return;\n        }\n        if !state.asr_received_event {\n'''
    if event_new not in text:
        if event_old not in text:
            raise RuntimeError("ASR event handler anchor missing")
        text = text.replace(event_old, event_new, 1)

    finished_old = '''    pub fn on_asr_stream_finished_state(&self, state: &mut AppState) {\n        // ASR stream completion is not the same as microphone capture stopping.\n'''
    finished_new = '''    pub fn on_asr_stream_finished_state(&self, state: &mut AppState) {\n        if state.ptt_release_committed {\n            log::debug!("Dropping ASR stream-finished after PTT release commit");\n            return;\n        }\n        // ASR stream completion is not the same as microphone capture stopping.\n'''
    if finished_new not in text:
        if finished_old not in text:
            raise RuntimeError("ASR stream-finished handler anchor missing")
        text = text.replace(finished_old, finished_new, 1)

    failed_old = '''    pub fn on_asr_stream_failed_state(&self, state: &mut AppState, failure: AsrFailure) {\n        log::warn!(\n'''
    failed_new = '''    pub fn on_asr_stream_failed_state(&self, state: &mut AppState, failure: AsrFailure) {\n        if state.ptt_release_committed {\n            log::debug!("Dropping ASR stream failure after PTT release commit");\n            return;\n        }\n        log::warn!(\n'''
    if failed_new not in text:
        if failed_old not in text:
            raise RuntimeError("ASR stream-failed handler anchor missing")
        text = text.replace(failed_old, failed_new, 1)

    tests = r'''

#[cfg(test)]
mod ptt_late_final_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn late_final_after_ptt_release_commit_is_ignored() {
        let controller = SessionController::default();
        let mut state = AppState::new();
        let now = Instant::now();
        state.handle_hotkey_pressed_at(now);
        state.on_hold_threshold_reached();
        state.transcript_text = "release snapshot".to_string();
        state.handle_hotkey_released_at(now + std::time::Duration::from_millis(1_200));
        assert!(state.commit_push_to_talk_release());
        let committed_version = state.final_version;

        controller.handle_asr_event_state(
            &mut state,
            AsrEvent {
                text: "late provider final".to_string(),
                is_final: true,
                prefetch: false,
                definite: true,
                confidence: None,
            },
        );

        assert_eq!(state.session_final_text, "release snapshot");
        assert_eq!(state.transcript_text, "release snapshot");
        assert_eq!(state.final_version, committed_version);
        assert!(state.ptt_release_committed);
    }
}
'''
    if "mod ptt_late_final_tests" not in text:
        text = text.rstrip() + tests + "\n"

    save(path, text)


def patch_audio_stopped() -> None:
    path = "src-tauri/src/session/mod.rs"
    text = load(path)

    old = '''        if state.terminal_error_message.is_some() {\n            log::info!("Audio capture stopped after ASR failure; skipping transcription pipeline");\n            self.schedule_error_cleanup();\n            return;\n        }\n\n        if let Some(ms) = duration_ms {\n'''
    new = '''        if state.terminal_error_message.is_some() {\n            log::info!("Audio capture stopped after ASR failure; skipping transcription pipeline");\n            self.schedule_error_cleanup();\n            return;\n        }\n\n        // PTT release already selected the final commit snapshot. AudioStopped\n        // is the local capture-close acknowledgement, not an ASR completion\n        // gate: inject now without waiting for WebSocket/stream-finished/final\n        // timeout or post-recording refinement. The audio metadata above is\n        // populated first so history persistence remains intact.\n        if state.ptt_release_committed {\n            log::info!(\n                "PTT release commit ready after audio stop; injecting immediately (len={})",\n                state.session_final_text.chars().count()\n            );\n            self.cancel_asr_final_timeout();\n            self.maybe_inject_final_state(state);\n            return;\n        }\n\n        if let Some(ms) = duration_ms {\n'''
    if new not in text:
        if old not in text:
            raise RuntimeError("AudioStopped PTT injection anchor missing")
        text = text.replace(old, new, 1)

    save(path, text)


def bump_version() -> None:
    replace_once("package.json", '  "version": "0.14.0",\n', '  "version": "0.14.1",\n')
    replace_once("src-tauri/tauri.conf.json", '  "version": "0.14.0",\n', '  "version": "0.14.1",\n')
    replace_once("src-tauri/Cargo.toml", 'version = "0.14.0"\n', 'version = "0.14.1"\n')


def main() -> None:
    patch_state()
    patch_hotkey_handler()
    patch_asr_handler()
    patch_audio_stopped()
    bump_version()
    print("PTT release-to-commit source migration applied")


if __name__ == "__main__":
    main()
