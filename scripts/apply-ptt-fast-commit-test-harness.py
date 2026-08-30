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
        raise RuntimeError(f"anchor missing in {path}: {old[:180]!r}")
    save(path, text.replace(old, new, 1))


def write_pure_state_module() -> None:
    path = ROOT / "src-tauri/src/ptt_commit.rs"
    content = r'''//! Pure Push-To-Talk release commit policy.
//!
//! This module deliberately has no Tauri or platform dependencies so the
//! release-to-commit state machine can be executed as a standalone Rust test
//! binary on Windows. Production AppState calls the same functions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseMode {
    PushToTalk,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitDecision {
    pub(crate) text: String,
    pub(crate) promoted_interim: bool,
}

/// Select the text that an explicit hotkey release commits.
///
/// A provider final always wins. If no non-empty final exists yet, the latest
/// HUD/interim transcript is accepted. Other recording modes never use this
/// release-to-commit policy.
pub(crate) fn select_release_commit(
    mode: ReleaseMode,
    has_final: bool,
    final_text: &str,
    latest_text: &str,
) -> Option<CommitDecision> {
    if mode != ReleaseMode::PushToTalk {
        return None;
    }

    if has_final && !final_text.trim().is_empty() {
        return Some(CommitDecision {
            text: final_text.to_string(),
            promoted_interim: false,
        });
    }

    if latest_text.trim().is_empty() {
        return None;
    }

    Some(CommitDecision {
        text: latest_text.to_string(),
        promoted_interim: true,
    })
}

/// Once PTT release has locked a commit snapshot, provider events from the old
/// stream must not alter or re-inject that session.
pub(crate) fn should_ignore_asr_after_release(ptt_release_committed: bool) -> bool {
    ptt_release_committed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptt_release_immediately_commits_existing_final() {
        let decision = select_release_commit(
            ReleaseMode::PushToTalk,
            true,
            "provider final",
            "newer interim",
        )
        .expect("PTT release must produce a commit immediately");

        assert_eq!(decision.text, "provider final");
        assert!(!decision.promoted_interim);
    }

    #[test]
    fn ptt_release_falls_back_to_latest_interim() {
        let decision = select_release_commit(
            ReleaseMode::PushToTalk,
            false,
            "",
            "latest HUD interim",
        )
        .expect("interim must be commit-capable on PTT release");

        assert_eq!(decision.text, "latest HUD interim");
        assert!(decision.promoted_interim);
    }

    #[test]
    fn ptt_release_does_not_commit_empty_text() {
        assert_eq!(
            select_release_commit(ReleaseMode::PushToTalk, false, "", "   "),
            None
        );
    }

    #[test]
    fn late_final_is_rejected_after_ptt_release_commit() {
        assert!(should_ignore_asr_after_release(true));
        assert!(!should_ignore_asr_after_release(false));
    }

    #[test]
    fn hands_free_keeps_existing_completion_path() {
        assert_eq!(
            select_release_commit(
                ReleaseMode::Other,
                false,
                "",
                "hands-free interim must keep waiting",
            ),
            None
        );
    }
}
'''
    path.write_text(content, encoding="utf-8", newline="\n")


def patch_lib() -> None:
    replace_once(
        "src-tauri/src/lib.rs",
        "pub mod network_proxy;\n",
        "pub mod network_proxy;\npub mod ptt_commit;\n",
    )


def patch_state_to_use_policy() -> None:
    path = "src-tauri/src/state.rs"
    old = '''    pub(crate) fn commit_push_to_talk_release(&mut self) -> bool {\n        if self.session_state != HotkeySessionState::Finalizing\n            || self.recording_style != Some(RecordingStyle::PushToTalk)\n        {\n            return false;\n        }\n\n        let has_usable_final = self.has_final_result && !self.session_final_text.trim().is_empty();\n        let commit_text = if has_usable_final {\n            self.session_final_text.clone()\n        } else {\n            self.transcript_text.clone()\n        };\n\n        if commit_text.trim().is_empty() {\n            self.ptt_release_committed = false;\n            return false;\n        }\n\n        if !has_usable_final {\n            self.session_final_text = commit_text;\n            self.has_final_result = true;\n            self.final_version = self.final_version.saturating_add(1);\n        }\n\n        self.transcript_text = self.session_final_text.clone();\n        self.last_injected_text = self.session_final_text.clone();\n        self.ptt_release_committed = true;\n\n        // Release is the terminal recognition boundary for this PTT session.\n        // This deliberately bypasses stream-finished/final-timeout/refinement\n        // gates; the controller also cancels the live ASR transport.\n        self.asr_stream_finished = true;\n        self.asr_reconnect_in_progress = false;\n        self.asr_refinement_in_progress = false;\n        self.asr_refinement_done = true;\n        true\n    }\n'''
    new = '''    pub(crate) fn commit_push_to_talk_release(&mut self) -> bool {\n        let mode = if self.session_state == HotkeySessionState::Finalizing\n            && self.recording_style == Some(RecordingStyle::PushToTalk)\n        {\n            crate::ptt_commit::ReleaseMode::PushToTalk\n        } else {\n            crate::ptt_commit::ReleaseMode::Other\n        };\n\n        let Some(decision) = crate::ptt_commit::select_release_commit(\n            mode,\n            self.has_final_result,\n            &self.session_final_text,\n            &self.transcript_text,\n        ) else {\n            self.ptt_release_committed = false;\n            return false;\n        };\n\n        if decision.promoted_interim {\n            self.session_final_text = decision.text;\n            self.has_final_result = true;\n            self.final_version = self.final_version.saturating_add(1);\n        } else {\n            self.session_final_text = decision.text;\n        }\n\n        self.transcript_text = self.session_final_text.clone();\n        self.last_injected_text = self.session_final_text.clone();\n        self.ptt_release_committed = true;\n\n        // Release is the terminal recognition boundary for this PTT session.\n        // This deliberately bypasses stream-finished/final-timeout/refinement\n        // gates; the controller also cancels the live ASR transport.\n        self.asr_stream_finished = true;\n        self.asr_reconnect_in_progress = false;\n        self.asr_refinement_in_progress = false;\n        self.asr_refinement_done = true;\n        true\n    }\n'''
    replace_once(path, old, new)


def patch_asr_to_use_policy() -> None:
    path = "src-tauri/src/session/handlers/asr.rs"
    text = load(path)
    old = "if state.ptt_release_committed {"
    new = "if crate::ptt_commit::should_ignore_asr_after_release(state.ptt_release_committed) {"
    if new not in text:
        count = text.count(old)
        if count < 3:
            raise RuntimeError(f"expected at least 3 PTT ASR guards, found {count}")
        text = text.replace(old, new, 3)
        save(path, text)


def main() -> None:
    write_pure_state_module()
    patch_lib()
    patch_state_to_use_policy()
    patch_asr_to_use_policy()
    print("Standalone PTT state-machine test harness applied")


if __name__ == "__main__":
    main()
