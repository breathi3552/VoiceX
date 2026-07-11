use tauri::async_runtime;

use crate::injector::{inject_serialized, TextInjectionMode};

/// Handles text injection off the main async tasks to avoid blocking.
#[derive(Clone, Default)]
pub struct TextInjectionService;

impl TextInjectionService {
    pub fn new() -> Self {
        Self
    }

    /// Fire-and-forget text injection using the configured mode.
    ///
    /// `skip_clipboard_restore` should be true when `mode` was resolved via a
    /// per-app text-injection override, since those targets are the ones
    /// known to bridge the clipboard over a variable-latency channel.
    pub fn inject_background(
        &self,
        mode: TextInjectionMode,
        text: String,
        skip_clipboard_restore: bool,
    ) {
        if text.is_empty() {
            return;
        }

        async_runtime::spawn_blocking(move || {
            if let Err(err) = inject_serialized(mode, &text, skip_clipboard_restore) {
                log::warn!("Text injection failed: {}", err);
            }
        });
    }

    /// Inject text in a blocking thread with a cancellation guard.
    /// Returns a JoinHandle so callers can await completion.
    ///
    /// `skip_clipboard_restore` should be true when `mode` was resolved via a
    /// per-app text-injection override, since those targets are the ones
    /// known to bridge the clipboard over a variable-latency channel.
    pub fn inject_background_guarded(
        &self,
        mode: TextInjectionMode,
        text: String,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        skip_clipboard_restore: bool,
    ) -> Option<tauri::async_runtime::JoinHandle<()>> {
        if text.is_empty() {
            return None;
        }

        Some(async_runtime::spawn_blocking(move || {
            if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                log::info!("Injection skipped: cancelled before start");
                return;
            }
            if let Err(err) = inject_serialized(mode, &text, skip_clipboard_restore) {
                log::warn!("Text injection failed: {}", err);
            }
        }))
    }
}
