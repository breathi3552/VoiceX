//! macOS selection reader: layered Accessibility read with a Copy fallback.

mod ax;
mod clipboard;

use std::time::Instant;

use crate::foreground_app;
use crate::selection::{
    normalize_text, SelectionError, SelectionOutcome, SelectionRequest, SelectionSource,
    Sensitivity,
};

use ax::{AttributeRead, FocusedElement};

pub fn read_selection(request: &SelectionRequest) -> Result<SelectionOutcome, SelectionError> {
    let started = Instant::now();

    // Freeze the foreground application before touching anything else, so every
    // later decision refers to the app that was frontmost when the hotkey fired.
    let app_info = foreground_app::detect_foreground_app(&request.app).map_err(|err| {
        log::debug!("Foreground app detection failed: {err}");
        SelectionError::NoForegroundApp
    })?;
    let app_bundle_id = app_info.bundle_id.clone();
    let app_name = app_info.display_name.clone();
    let app_pid = app_info.process_id;

    let finish = |text: String,
                  source: SelectionSource,
                  sensitivity: Sensitivity,
                  clipboard_restored: Option<bool>| {
        Ok(SelectionOutcome {
            text,
            source,
            sensitivity,
            app_bundle_id: app_bundle_id.clone(),
            app_name: app_name.clone(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            clipboard_restored,
        })
    };

    if app_info.is_self {
        return Err(SelectionError::FocusIsSelf);
    }

    if !crate::hotkey::HotkeyPermissionStatus::detect().accessibility {
        return Err(SelectionError::PermissionDenied);
    }

    // Secure keyboard entry is process-global: whatever has focus is a secret,
    // and synthetic key events would be blocked anyway.
    if ax::secure_event_input_enabled() {
        return Err(SelectionError::SecureInput);
    }

    let focused = match FocusedElement::read() {
        Ok(focused) => focused,
        Err(_api_disabled) => return Err(SelectionError::PermissionDenied),
    };

    // Sensitivity defaults to Unknown: without a focused element we know nothing
    // about the control the text came from.
    let mut sensitivity = Sensitivity::Unknown;
    // Distinguishes "the control says nothing is selected" from "the control
    // does not answer", which decides how a silent copy is reported.
    let mut ax_reported_empty = false;

    if let Some(element) = focused.as_ref() {
        if element.is_secure() {
            return Err(SelectionError::SecureInput);
        }
        sensitivity = element.sensitivity();

        match element.selected_text() {
            AttributeRead::Text(raw) => {
                let text = normalize_text(&raw);
                if !text.is_empty() {
                    return finish(text, SelectionSource::Ax, sensitivity, None);
                }
                ax_reported_empty = true;
            }
            AttributeRead::Empty => ax_reported_empty = true,
            AttributeRead::ApiDisabled => return Err(SelectionError::PermissionDenied),
            AttributeRead::Unsupported => {
                log::debug!(
                    "AXSelectedText unsupported for role {:?}; falling back",
                    element.role()
                );
            }
        }
    }

    if !request.allow_clipboard_fallback {
        return Err(if ax_reported_empty {
            SelectionError::NoSelection
        } else {
            SelectionError::UnsupportedControl
        });
    }

    let copied = match clipboard::read_via_copy(&request.app, app_pid) {
        Ok(copied) => copied,
        // A copy that changes nothing is indistinguishable from an empty
        // selection at the pasteboard level; trust the Accessibility layer when
        // it already told us the selection was empty.
        Err(SelectionError::CopyTimeout) if ax_reported_empty => {
            return Err(SelectionError::NoSelection)
        }
        Err(err) => return Err(err),
    };

    if !copied.restored {
        // The user's clipboard now holds the copied selection. Loud on purpose:
        // reported up through the outcome so it lands in the structured log
        // rather than only in a debug line nobody reads.
        log::warn!("Clipboard was not restored after the copy fallback");
    }

    let text = normalize_text(&copied.text);
    if text.is_empty() {
        return Err(SelectionError::NoSelection);
    }

    finish(
        text,
        SelectionSource::ClipboardCopy,
        sensitivity,
        Some(copied.restored),
    )
}
