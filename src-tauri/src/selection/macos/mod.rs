//! macOS selection reader: layered Accessibility read with a Copy fallback.

mod ax;
mod clipboard;

use std::time::Instant;

use crate::foreground_app;
use crate::selection::{
    normalize_text, SelectionError, SelectionOutcome, SelectionRequest, SelectionSource,
};
use crate::tts::log_event;

use ax::{AttributeRead, FocusedElement, SelectionAttributes};

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

    let finish = |text: String, source: SelectionSource, clipboard_restored: Option<bool>| {
        Ok(SelectionOutcome {
            text,
            source,
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

    let focused = match FocusedElement::read() {
        Ok(focused) => focused,
        Err(_api_disabled) => return Err(SelectionError::PermissionDenied),
    };

    // Distinguishes "the control says nothing is selected" from "the control
    // does not answer", which decides how a silent copy is reported.
    let mut ax_reported_empty = false;

    if let Some(element) = focused.as_ref() {
        let read = element.selected_text();

        // Fast path first: when the Accessibility layer hands over text there is
        // nothing left to diagnose, and the probe below costs another round trip.
        if let AttributeRead::Text(raw) = &read {
            let text = normalize_text(raw);
            if !text.is_empty() {
                log_ax_probe(element, &read, None);
                return finish(text, SelectionSource::Ax, None);
            }
        }

        // Everything from here on is a fallback, so spend the extra call: which
        // attributes the element advertises is what decides whether the phase-1
        // range layer can help this application at all (plan §5).
        log_ax_probe(element, &read, Some(element.selection_attributes()));

        match read {
            AttributeRead::Text(_) | AttributeRead::Empty(_) => ax_reported_empty = true,
            AttributeRead::ApiDisabled => return Err(SelectionError::PermissionDenied),
            AttributeRead::Unsupported(_) => {}
        }
    } else {
        log_event("selection_ax", &[("focused", "none".to_string())]);
    }

    if !request.allow_clipboard_fallback {
        return Err(if ax_reported_empty {
            SelectionError::NoSelection
        } else {
            SelectionError::UnsupportedControl
        });
    }

    // Only the copy path cares: secure keyboard entry makes the window server
    // drop synthetic key events, so the Cmd-C below would never arrive and the
    // read would fail as a copy timeout 300 ms later. Checking here turns that
    // into an immediate, explainable error. Not a privacy rule — the
    // Accessibility path above ran unconditionally (plan §3.4).
    if ax::secure_event_input_enabled() {
        return Err(SelectionError::SecureInput);
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

    finish(text, SelectionSource::ClipboardCopy, Some(copied.restored))
}

/// Record what the Accessibility layer said about the focused control.
///
/// Diagnostic only — no decision reads this. It exists because "AX did not
/// return text" hides several very different situations, and phase 1 needs to
/// know which one each application is in before deciding whether a range layer
/// is worth building (plan §5). `attributes` is `None` on the success path,
/// where the extra round trip would buy nothing.
///
/// Roles and attribute names are control metadata, never content, so this is
/// safe at the ordinary log level under §3.4.
fn log_ax_probe(
    element: &FocusedElement,
    read: &AttributeRead,
    attributes: Option<SelectionAttributes>,
) {
    let mut fields = vec![
        ("role", element.role().unwrap_or_default().to_string()),
        ("subrole", element.subrole().unwrap_or_default().to_string()),
        ("attr", read.kind().to_string()),
        ("status", read.status().to_string()),
    ];
    if let Some(attributes) = attributes {
        fields.push(("enumerated", attributes.enumerated.to_string()));
        fields.push(("has_sel_text", attributes.selected_text.to_string()));
        fields.push(("has_sel_range", attributes.selected_text_range.to_string()));
        fields.push((
            "has_marker_range",
            attributes.selected_text_marker_range.to_string(),
        ));
        fields.push(("has_value", attributes.value.to_string()));
    }
    log_event("selection_ax", &fields);
}
