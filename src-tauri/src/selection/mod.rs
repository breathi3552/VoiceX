//! Cross-application selected-text reading.
//!
//! This module is the platform-agnostic surface of the selection subsystem.
//! Platform types (AX element handles, `NSPasteboard`, …) never cross this
//! boundary, so a future `WindowsSelectionReader` only has to provide the same
//! [`read_selection`] entry point.
//!
//! Reads are serialized on a dedicated worker thread: the macOS Accessibility
//! API is documented as single-threaded, and serializing also keeps the
//! clipboard fallback from interleaving with itself.

use std::sync::{
    mpsc::{self, Sender, SyncSender},
    OnceLock,
};
use std::thread;

#[cfg(target_os = "macos")]
mod macos;

/// Which layer of the fallback chain produced the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// `kAXSelectedTextAttribute` read directly off the focused element.
    Ax,
    /// `kAXSelectedTextRangeAttribute` + parameterized attribute. Phase 1.
    #[allow(dead_code)]
    AxRange,
    /// Synthetic Cmd-C with clipboard snapshot/restore.
    ClipboardCopy,
}

impl SelectionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SelectionSource::Ax => "ax",
            SelectionSource::AxRange => "ax_range",
            SelectionSource::ClipboardCopy => "clipboard_copy",
        }
    }
}

/// A successful selection read.
#[derive(Debug, Clone)]
pub struct SelectionOutcome {
    pub text: String,
    pub source: SelectionSource,
    pub app_bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub elapsed_ms: u64,
    /// Only set for [`SelectionSource::ClipboardCopy`]: whether the user's
    /// clipboard was put back. `Some(false)` means their clipboard now holds
    /// the copied selection instead of what they had — surfaced in the
    /// structured log, and in the HUD once phase 3 adds status detail.
    pub clipboard_restored: Option<bool>,
}

/// Structured failure reasons. Every variant maps to a stable machine code via
/// [`SelectionError::code`], which the HUD and the automation harness key off.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SelectionError {
    #[error("No text is selected")]
    NoSelection,

    #[error("The focused control does not expose its selection")]
    UnsupportedControl,

    #[error("Accessibility permission is not granted")]
    PermissionDenied,

    #[error("Secure keyboard entry is active, so the copy fallback cannot run")]
    SecureInput,

    #[error("The application did not respond to the copy command in time")]
    CopyTimeout,

    #[error("Refusing the clipboard fallback: {0}")]
    ClipboardSnapshotRefused(String),

    #[error("Hotkey modifiers are still held; cannot synthesize a copy")]
    ModifiersHeld,

    #[error("The foreground application changed while reading the selection")]
    ForegroundChanged,

    #[error("VoiceX itself has focus")]
    FocusIsSelf,

    #[error("No foreground application")]
    NoForegroundApp,

    #[error("Selection reading is not supported on this platform")]
    PlatformUnsupported,

    #[error("Selection reading failed: {0}")]
    Internal(String),
}

impl SelectionError {
    pub fn code(&self) -> &'static str {
        match self {
            SelectionError::NoSelection => "no_selection",
            SelectionError::UnsupportedControl => "unsupported_control",
            SelectionError::PermissionDenied => "permission_denied",
            SelectionError::SecureInput => "secure_input",
            SelectionError::CopyTimeout => "copy_timeout",
            SelectionError::ClipboardSnapshotRefused(_) => "clipboard_snapshot_refused",
            SelectionError::ModifiersHeld => "modifiers_held",
            SelectionError::ForegroundChanged => "foreground_changed",
            SelectionError::FocusIsSelf => "focus_is_self",
            SelectionError::NoForegroundApp => "no_foreground_app",
            SelectionError::PlatformUnsupported => "platform_unsupported",
            SelectionError::Internal(_) => "internal",
        }
    }
}

#[derive(Clone)]
pub struct SelectionRequest {
    pub app: tauri::AppHandle,
    /// Compatibility mode: synthesize Cmd-C when the Accessibility path comes
    /// up empty. Subject to the fail-closed clipboard rules regardless.
    pub allow_clipboard_fallback: bool,
}

type Job = (
    SelectionRequest,
    SyncSender<Result<SelectionOutcome, SelectionError>>,
);

fn worker() -> &'static Sender<Job> {
    static WORKER: OnceLock<Sender<Job>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("voicex-selection".to_string())
            .spawn(move || {
                while let Ok((request, reply)) = rx.recv() {
                    let _ = reply.send(read_selection_on_worker(&request));
                }
            })
            .expect("failed to spawn the selection worker thread");
        tx
    })
}

fn read_selection_on_worker(
    request: &SelectionRequest,
) -> Result<SelectionOutcome, SelectionError> {
    #[cfg(target_os = "macos")]
    {
        macos::read_selection(request)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = request;
        Err(SelectionError::PlatformUnsupported)
    }
}

/// Read the current selection from the foreground application.
///
/// Blocking. Must not be called from the main thread: the macOS implementation
/// hops to the main thread for AppKit queries and would deadlock.
pub fn read_selection(request: SelectionRequest) -> Result<SelectionOutcome, SelectionError> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    worker()
        .send((request, reply_tx))
        .map_err(|_| SelectionError::Internal("selection worker is gone".to_string()))?;
    reply_rx
        .recv()
        .map_err(|_| SelectionError::Internal("selection worker dropped the request".to_string()))?
}

/// Minimal normalization: CRLF/CR to LF, then trim.
///
/// Table-text and whitespace normalization proper lands in phase 1 together
/// with the fixtures that pin down the expected output.
pub fn normalize_text(raw: &str) -> String {
    raw.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_line_endings_and_trims() {
        assert_eq!(normalize_text("  hello\r\nworld\r  "), "hello\nworld");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(SelectionError::NoSelection.code(), "no_selection");
        assert_eq!(SelectionError::SecureInput.code(), "secure_input");
        assert_eq!(
            SelectionError::ClipboardSnapshotRefused("x".to_string()).code(),
            "clipboard_snapshot_refused"
        );
    }
}
