//! Text injection module

mod clipboard;

pub use clipboard::{InjectorError, TextInjectionMode, TextInjector};

use std::sync::Mutex;

/// App handle used to dispatch AppKit work that must run on the main thread
/// (the pasteboard focus round-trip's activation dance). Registered once
/// during app setup; macOS only.
#[cfg(target_os = "macos")]
pub(crate) static MAIN_THREAD_APP: std::sync::OnceLock<tauri::AppHandle> =
    std::sync::OnceLock::new();

/// Global mutex to prevent concurrent text injections.
/// Two simultaneous clipboard pastes or typing sequences would corrupt output.
static INJECTION_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the global injection lock, inject text, then release.
/// This guarantees at most one injection runs at a time.
///
/// `skip_clipboard_restore` should be true when the target's per-app override
/// has `skip_clipboard_restore` set (see
/// `foreground_app::match_text_injection_override`), i.e. it bridges the
/// clipboard over a variable-latency channel (e.g. a remote-desktop client)
/// where a timed restore is not safe to perform.
///
/// `focus_roundtrip_pid` optionally identifies the paste target's process.
/// When set, the pasteboard path first bounces focus through VoiceX so the
/// target's clipboard bridge announces the freshly written payload before
/// the paste is synthesized — remote-desktop clients only re-read the Mac
/// clipboard when they become active again (macOS only; ignored elsewhere).
pub fn inject_serialized(
    mode: TextInjectionMode,
    text: &str,
    skip_clipboard_restore: bool,
    focus_roundtrip_pid: Option<u32>,
) -> Result<(), InjectorError> {
    let _guard = INJECTION_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    TextInjector::with_mode(mode, skip_clipboard_restore)
        .with_focus_roundtrip_target(focus_roundtrip_pid)
        .inject(text)
}
