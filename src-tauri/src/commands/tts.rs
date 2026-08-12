//! Selected-text reading commands.
//!
//! Every command here is `async` on purpose: Tauri runs synchronous commands
//! on the main thread, and both the voice list and the preview hop to the main
//! thread themselves — doing that from the main thread would deadlock.

use serde::Serialize;
use tauri::State;

use crate::hotkey::{HotkeyConfiguration, HotkeyManager, ReadSelectionStatus};
use crate::tts::TtsController;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsVoiceOption {
    pub id: String,
    pub name: String,
    pub language: String,
}

/// Voices a backend offers, for the voice picker.
///
/// `provider` is explicit because the settings page asks right after the user
/// picks one, before the debounced save has written it — resolving it from the
/// store would answer about the previous provider and list its voices instead.
#[tauri::command]
pub async fn list_tts_voices(
    tts: State<'_, TtsController>,
    provider: Option<String>,
) -> Result<Vec<TtsVoiceOption>, String> {
    let controller = tts.inner().clone();
    tauri::async_runtime::spawn_blocking(move || controller.list_voices(provider.as_deref()))
        .await
        .map_err(|err| format!("Failed to list voices: {err}"))?
        .map(|voices| {
            voices
                .into_iter()
                .map(|voice| TtsVoiceOption {
                    id: voice.id,
                    name: voice.name,
                    language: voice.language,
                })
                .collect()
        })
        .map_err(|err| err.to_string())
}

/// Speak `text` with the saved voice parameters, bypassing the selection
/// reader. The caller supplies the sample so it can be in the UI's language.
#[tauri::command]
pub async fn preview_tts(tts: State<'_, TtsController>, text: String) -> Result<(), String> {
    let controller = tts.inner().clone();
    tauri::async_runtime::spawn_blocking(move || controller.speak_preview(text))
        .await
        .map_err(|err| format!("Failed to start the preview: {err}"))?
        .map_err(|err| err.to_string())
}

/// Stop whatever is being spoken. Used by the settings page to cancel a
/// preview; the read hotkey has its own path into the same session.
#[tauri::command]
pub async fn stop_tts(tts: State<'_, TtsController>) -> Result<(), String> {
    let controller = tts.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        controller.stop(crate::tts::StopReason::Ui);
    })
    .await
    .map_err(|err| format!("Failed to stop speech: {err}"))
}

/// Apply the reading hotkey. `enabled: false` unbinds it entirely, so the key
/// goes back to the foreground application instead of being swallowed.
///
/// Returns the resulting state, including whether the binding was refused for
/// colliding with the dictation key — the caller has no other way to find out.
#[tauri::command]
pub async fn apply_read_selection_hotkey(
    manager: State<'_, HotkeyManager>,
    config: Option<String>,
    enabled: bool,
) -> Result<ReadSelectionStatus, String> {
    let binding = if enabled {
        Some(
            config
                .and_then(|value| HotkeyConfiguration::from_storage(&value))
                .unwrap_or_else(HotkeyConfiguration::default_read_selection),
        )
    } else {
        None
    };

    manager.set_read_selection_config(binding);
    Ok(manager.read_selection_status())
}

/// Current state of the reading binding. The settings page asks on mount
/// because the dictation key may have changed since it last applied one.
#[tauri::command]
pub async fn read_selection_hotkey_status(
    manager: State<'_, HotkeyManager>,
) -> Result<ReadSelectionStatus, String> {
    Ok(manager.read_selection_status())
}
