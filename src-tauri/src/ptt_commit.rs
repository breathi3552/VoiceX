//! Pure Push-To-Talk release commit policy.
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
