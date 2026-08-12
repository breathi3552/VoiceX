//! Copy compatibility mode: snapshot the pasteboard, synthesize Cmd-C, read the
//! result, restore.
//!
//! Deliberately **not** built on `injector/clipboard.rs`. That path writes then
//! pastes; this one reads what another app writes, so the ordering, the
//! `changeCount` bookkeeping and the failure modes are different assumptions
//! and are kept as separate code.
//!
//! Fail-closed contract (plan §3.4): if any declared pasteboard type cannot be
//! captured, the fallback is refused outright rather than clobbering the user's
//! clipboard and hoping the restore works.

use std::thread;
use std::time::{Duration, Instant};

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString};

use crate::selection::SelectionError;

/// Physical keycodes for a layout-independent Command+C.
const MACOS_COMMAND_KEYCODE: u16 = 55;
const MACOS_C_KEYCODE: u16 = 8;

/// How long the target application gets to answer the copy (plan §4.3).
const COPY_TIMEOUT_MS: u64 = 300;
const COPY_POLL_INTERVAL_MS: u64 = 10;

/// How long we wait for the user to let go of the hotkey before synthesizing a
/// copy. Posting Cmd-C while Option is physically down delivers Option+Cmd+C to
/// the target application, which is somebody else's shortcut.
const MODIFIER_RELEASE_TIMEOUT_MS: u64 = 800;
const MODIFIER_POLL_INTERVAL_MS: u64 = 10;

/// Total snapshot budget. Beyond this we refuse rather than hold (and rewrite)
/// very large clipboard payloads.
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

/// Types whose payload is not the data on the pasteboard — a promise to be
/// fulfilled later. Capturing the bytes does not capture the promise, so we
/// cannot honestly restore them.
const PROMISE_TYPE_PREFIXES: [&str; 2] = [
    "com.apple.pasteboard.promised",
    "Apple files promise pasteboard type",
];

#[allow(non_snake_case)]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state_id: u32) -> u64;
}

const K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION: u32 = 0;
/// Command | Alternate | Control | Shift
const MODIFIER_FLAG_MASK: u64 = 0x0010_0000 | 0x0008_0000 | 0x0004_0000 | 0x0002_0000;

fn modifiers_held() -> bool {
    let flags = unsafe { CGEventSourceFlagsState(K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION) };
    flags & MODIFIER_FLAG_MASK != 0
}

fn wait_for_modifier_release() -> Result<(), SelectionError> {
    let deadline = Instant::now() + Duration::from_millis(MODIFIER_RELEASE_TIMEOUT_MS);
    while modifiers_held() {
        if Instant::now() >= deadline {
            return Err(SelectionError::ModifiersHeld);
        }
        thread::sleep(Duration::from_millis(MODIFIER_POLL_INTERVAL_MS));
    }
    Ok(())
}

/// A captured pasteboard, item by item and type by type.
struct PasteboardSnapshot {
    items: Vec<Vec<(String, Vec<u8>)>>,
}

/// Reason a type could not be captured, for the structured error.
fn refuse(reason: impl Into<String>) -> SelectionError {
    SelectionError::ClipboardSnapshotRefused(reason.into())
}

fn is_promise_type(ty: &str) -> bool {
    PROMISE_TYPE_PREFIXES
        .iter()
        .any(|prefix| ty.starts_with(prefix))
}

impl PasteboardSnapshot {
    fn capture(pasteboard: &NSPasteboard) -> Result<Self, SelectionError> {
        let mut items = Vec::new();
        let mut total_bytes = 0usize;

        let Some(pasteboard_items) = pasteboard.pasteboardItems() else {
            // No items at all: nothing to restore, and nothing that can fail.
            return Ok(Self { items });
        };

        for item in pasteboard_items.iter() {
            let mut captured = Vec::new();
            for ty in item.types().iter() {
                let type_name = ty.to_string();
                if is_promise_type(&type_name) {
                    return Err(refuse(format!("promised type {type_name}")));
                }

                let Some(data) = item.dataForType(&ty) else {
                    // Lazily-provided data whose owner refused or went away.
                    return Err(refuse(format!("unreadable type {type_name}")));
                };

                total_bytes = total_bytes.saturating_add(data.len());
                if total_bytes > MAX_SNAPSHOT_BYTES {
                    return Err(refuse(format!(
                        "clipboard exceeds {MAX_SNAPSHOT_BYTES} bytes"
                    )));
                }

                captured.push((type_name, data.to_vec()));
            }
            items.push(captured);
        }

        Ok(Self { items })
    }

    /// Put the captured contents back.
    ///
    /// Every item is rebuilt *before* the pasteboard is cleared: `clearContents`
    /// is the destructive step, so anything that can fail must fail while the
    /// user's clipboard is still intact. Clearing first and then hitting a
    /// failure mid-rebuild would leave them with an empty or half-restored
    /// clipboard.
    fn restore(&self, pasteboard: &NSPasteboard) -> Result<(), String> {
        let mut rebuilt: Vec<Retained<NSPasteboardItem>> = Vec::with_capacity(self.items.len());
        for types in &self.items {
            let item = NSPasteboardItem::new();
            for (type_name, bytes) in types {
                let ns_type = NSString::from_str(type_name);
                let ns_data = NSData::with_bytes(bytes);
                if !item.setData_forType(&ns_data, &ns_type) {
                    return Err(format!("setData failed for {type_name}"));
                }
            }
            rebuilt.push(item);
        }

        pasteboard.clearContents();
        if rebuilt.is_empty() {
            return Ok(());
        }

        let writable: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = rebuilt
            .iter()
            .map(|item| ProtocolObject::from_ref(&**item))
            .collect();
        if !pasteboard.writeObjects(&NSArray::from_slice(&writable)) {
            return Err("writeObjects returned false".to_string());
        }
        Ok(())
    }
}

pub struct CopyReadOutcome {
    pub text: String,
    /// False when a concurrent clipboard change made restoring unsafe.
    pub restored: bool,
}

/// Synthesize Cmd-C against the foreground app and read back the plain text.
///
/// `expected` is the application the selection was read from; the copy is
/// posted to whatever is frontmost *now*, so the caller's snapshot is
/// re-verified immediately beforehand. Waiting for the modifiers to clear can
/// take most of a second, which is ample time to switch windows, and posting
/// Cmd-C into an app we never inspected could both read the wrong content and
/// hit a control we never checked for secure input.
///
/// Returns [`SelectionError::CopyTimeout`] when the pasteboard never changed —
/// which is also what an empty selection looks like from here; the caller
/// disambiguates using what the Accessibility layer already reported.
pub fn read_via_copy(
    app: &tauri::AppHandle,
    expected_pid: u32,
) -> Result<CopyReadOutcome, SelectionError> {
    wait_for_modifier_release()?;

    let current = crate::foreground_app::detect_foreground_app(app)
        .map_err(|_| SelectionError::NoForegroundApp)?;
    if current.process_id != expected_pid {
        return Err(SelectionError::ForegroundChanged);
    }

    let pasteboard = NSPasteboard::generalPasteboard();
    let snapshot = PasteboardSnapshot::capture(&pasteboard)?;
    let change_count_before = pasteboard.changeCount();

    post_copy_shortcut()?;

    let deadline = Instant::now() + Duration::from_millis(COPY_TIMEOUT_MS);
    loop {
        if pasteboard.changeCount() != change_count_before {
            break;
        }
        if Instant::now() >= deadline {
            // Nothing was written, so the user's clipboard is untouched.
            return Err(SelectionError::CopyTimeout);
        }
        thread::sleep(Duration::from_millis(COPY_POLL_INTERVAL_MS));
    }

    let change_count_after_copy = pasteboard.changeCount();
    let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) }
        .map(|value| value.to_string())
        .unwrap_or_default();

    // Only restore if nothing else has touched the pasteboard since our copy;
    // a clipboard manager or another app must not lose its write.
    let restored = if pasteboard.changeCount() == change_count_after_copy {
        match snapshot.restore(&pasteboard) {
            Ok(()) => true,
            Err(err) => {
                log::warn!("Failed to restore the clipboard after a copy fallback: {err}");
                false
            }
        }
    } else {
        log::info!("Skipping clipboard restore: another process wrote to it during the copy");
        false
    };

    Ok(CopyReadOutcome { text, restored })
}

/// Post Command+C as a raw CGEvent sourced from `HIDSystemState`, the same
/// source real hardware uses. Applications that gate on the event source (for
/// example remote-desktop clients) otherwise ignore the shortcut.
fn post_copy_shortcut() -> Result<(), SelectionError> {
    const NX_DEVICELCMDKEYMASK: CGEventFlags = CGEventFlags::from_bits_retain(0x0000_0008);

    let base_flags = {
        let mut flags = CGEventFlags::CGEventFlagNonCoalesced;
        flags.set(CGEventFlags::from_bits_retain(0x2000_0000), true);
        flags
    };
    let command_held_flags = base_flags | CGEventFlags::CGEventFlagCommand | NX_DEVICELCMDKEYMASK;

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| SelectionError::Internal("CGEventSource::new failed".to_string()))?;

    let post_key =
        |keycode: u16, keydown: bool, flags: CGEventFlags| -> Result<(), SelectionError> {
            let event =
                CGEvent::new_keyboard_event(source.clone(), keycode, keydown).map_err(|_| {
                    SelectionError::Internal("CGEvent::new_keyboard_event failed".to_string())
                })?;
            event.set_flags(flags);
            event.post(CGEventTapLocation::HID);
            Ok(())
        };

    post_key(MACOS_COMMAND_KEYCODE, true, command_held_flags)?;
    post_key(MACOS_C_KEYCODE, true, command_held_flags)?;
    post_key(MACOS_C_KEYCODE, false, command_held_flags)?;
    post_key(MACOS_COMMAND_KEYCODE, false, base_flags)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promise_types_are_recognised() {
        assert!(is_promise_type("com.apple.pasteboard.promised-file-url"));
        assert!(is_promise_type(
            "com.apple.pasteboard.promised-file-content-type"
        ));
        assert!(is_promise_type("Apple files promise pasteboard type"));
        assert!(!is_promise_type("public.utf8-plain-text"));
        assert!(!is_promise_type("public.png"));
    }
}
