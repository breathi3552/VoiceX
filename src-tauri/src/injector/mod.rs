//! Text injection module

mod clipboard;

pub use clipboard::{InjectorError, TextInjectionMode, TextInjector};

use std::sync::Mutex;

/// Global mutex to prevent concurrent text injections.
/// Two simultaneous clipboard pastes or typing sequences would corrupt output.
static INJECTION_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the global injection lock, inject text, then release.
/// This guarantees at most one injection runs at a time.
///
/// `skip_clipboard_restore` should be true when the target was resolved via a
/// per-app text-injection override (see `foreground_app::match_text_injection_override`),
/// since those targets are the ones known to bridge the clipboard over a
/// variable-latency channel (e.g. a remote-desktop client).
pub fn inject_serialized(
    mode: TextInjectionMode,
    text: &str,
    skip_clipboard_restore: bool,
) -> Result<(), InjectorError> {
    let _guard = INJECTION_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    TextInjector::with_mode(mode, skip_clipboard_restore).inject(text)
}
