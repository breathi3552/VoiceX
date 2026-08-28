#![allow(clippy::upper_case_acronyms, improper_ctypes_definitions)]
use crate::rdev::{Button, Event, EventType};
use cocoa::base::id;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, EventField};
use std::convert::TryInto;
use std::os::raw::c_void;
use std::time::SystemTime;

use crate::macos::keycodes::key_from_code;

pub type CFMachPortRef = *const c_void;
pub type CFIndex = u64;
pub type CFAllocatorRef = id;
pub type CFRunLoopSourceRef = id;
pub type CFRunLoopRef = id;
pub type CFRunLoopMode = id;
pub type CGEventTapProxy = id;
pub type CGEventRef = CGEvent;

// https://developer.apple.com/documentation/coregraphics/cgeventtapplacement?language=objc
pub type CGEventTapPlacement = u32;
#[allow(non_upper_case_globals)]
pub const kCGHeadInsertEventTap: u32 = 0;

// https://developer.apple.com/documentation/coregraphics/cgeventtapoptions?language=objc
#[allow(non_upper_case_globals)]
#[repr(u32)]
pub enum CGEventTapOption {
    #[cfg(feature = "unstable_grab")]
    Default = 0,
    ListenOnly = 1,
}

/// Per-keycode flag masks for classifying a FlagsChanged event as a press or a
/// release: (this key's device-specific bit, both sides' device bits for the
/// modifier, the modifier's device-independent bit).
///
/// The upstream heuristic compared the whole flags value against a global
/// "last flags" and called any numeric decrease a release. One missed event
/// (tap disabled, secure input) left that global stale, after which a real
/// press could read as a release and every consumer tracking modifier state
/// desynced. Deciding from the bit that belongs to the key in the event is
/// stateless, so a missed event can never poison the next one.
fn flags_changed_masks(code: u16) -> Option<(u64, u64, u64)> {
    // NX_DEVICE*KEYMASK / NX_*MASK constants from IOKit's IOLLEvent.h.
    match code {
        56 => Some((0x0000_0002, 0x0000_0006, 0x0002_0000)), // left shift
        60 => Some((0x0000_0004, 0x0000_0006, 0x0002_0000)), // right shift
        59 => Some((0x0000_0001, 0x0000_2001, 0x0004_0000)), // left control
        62 => Some((0x0000_2000, 0x0000_2001, 0x0004_0000)), // right control
        58 => Some((0x0000_0020, 0x0000_0060, 0x0008_0000)), // left option
        61 => Some((0x0000_0040, 0x0000_0060, 0x0008_0000)), // right option
        55 => Some((0x0000_0008, 0x0000_0018, 0x0010_0000)), // left command
        54 => Some((0x0000_0010, 0x0000_0018, 0x0010_0000)), // right command
        63 => Some((0x0080_0000, 0x0080_0000, 0x0080_0000)), // fn
        57 => Some((0x0001_0000, 0x0001_0000, 0x0001_0000)), // caps lock
        _ => None,
    }
}

/// Whether this FlagsChanged event is the key going down.
///
/// Hardware events carry the device-specific bits, which stay correct even
/// with both sides of a modifier held; synthetic events often carry only the
/// device-independent bit, so that is the fallback when no device bit of the
/// pair is present.
fn flags_changed_is_press(code: u16, bits: u64) -> Option<bool> {
    let (own_device_bit, pair_device_bits, generic_bit) = flags_changed_masks(code)?;
    if bits & pair_device_bits != 0 {
        Some(bits & own_device_bit != 0)
    } else {
        Some(bits & generic_bit != 0)
    }
}

#[cfg(test)]
mod flags_changed_tests {
    use super::flags_changed_is_press;

    const SHIFT: u64 = 0x0002_0000;
    const L_SHIFT_DEV: u64 = 0x0000_0002;
    const R_SHIFT_DEV: u64 = 0x0000_0004;
    const CONTROL: u64 = 0x0004_0000;
    const L_CTRL_DEV: u64 = 0x0000_0001;

    #[test]
    fn hardware_press_and_release_classify_by_the_device_bit() {
        assert_eq!(
            flags_changed_is_press(59, CONTROL | L_CTRL_DEV),
            Some(true)
        );
        assert_eq!(flags_changed_is_press(59, 0), Some(false));
    }

    #[test]
    fn releasing_one_of_two_held_shifts_is_a_release() {
        // The generic shift bit stays set because the other side is still
        // down; only the device bit tells the sides apart. The upstream
        // whole-value comparison got exactly this case wrong.
        let right_still_held = SHIFT | R_SHIFT_DEV;
        assert_eq!(flags_changed_is_press(56, right_still_held), Some(false));
        assert_eq!(flags_changed_is_press(60, right_still_held), Some(true));
    }

    #[test]
    fn synthetic_events_without_device_bits_fall_back_to_the_generic_bit() {
        assert_eq!(flags_changed_is_press(56, SHIFT), Some(true));
        assert_eq!(flags_changed_is_press(56, 0), Some(false));
    }

    #[test]
    fn classification_is_stateless() {
        // The same event classifies the same way regardless of what came
        // before — a missed event can no longer poison the next one.
        let press = CONTROL | L_CTRL_DEV;
        assert_eq!(flags_changed_is_press(59, press), Some(true));
        assert_eq!(flags_changed_is_press(59, 0), Some(false));
        assert_eq!(flags_changed_is_press(59, press), Some(true));
    }

    #[test]
    fn unknown_keycodes_are_dropped_rather_than_guessed() {
        assert_eq!(flags_changed_is_press(200, SHIFT), None);
    }
}

// https://developer.apple.com/documentation/coregraphics/cgeventmask?language=objc
pub type CGEventMask = u64;
#[allow(non_upper_case_globals)]
pub const kCGEventMaskForAllEvents: u64 = (1 << CGEventType::LeftMouseDown as u64)
    + (1 << CGEventType::LeftMouseUp as u64)
    + (1 << CGEventType::RightMouseDown as u64)
    + (1 << CGEventType::RightMouseUp as u64)
    + (1 << CGEventType::MouseMoved as u64)
    + (1 << CGEventType::LeftMouseDragged as u64)
    + (1 << CGEventType::RightMouseDragged as u64)
    + (1 << CGEventType::KeyDown as u64)
    + (1 << CGEventType::KeyUp as u64)
    + (1 << CGEventType::FlagsChanged as u64)
    + (1 << CGEventType::ScrollWheel as u64);

#[cfg(target_os = "macos")]
#[link(name = "Cocoa", kind = "framework")]
extern "C" {
    #[allow(improper_ctypes)]
    pub fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOption,
        eventsOfInterest: CGEventMask,
        callback: QCallback,
        user_info: id,
    ) -> CFMachPortRef;
    pub fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        tap: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub fn CFRunLoopRun();

    pub static kCFRunLoopCommonModes: CFRunLoopMode;

}
#[allow(improper_ctypes_definitions)]
pub type QCallback = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    _type: CGEventType,
    cg_event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

pub unsafe fn convert(
    _type: CGEventType,
    cg_event: &CGEvent,
) -> Option<Event> {
    let option_type = match _type {
        CGEventType::LeftMouseDown => Some(EventType::ButtonPress(Button::Left)),
        CGEventType::LeftMouseUp => Some(EventType::ButtonRelease(Button::Left)),
        CGEventType::RightMouseDown => Some(EventType::ButtonPress(Button::Right)),
        CGEventType::RightMouseUp => Some(EventType::ButtonRelease(Button::Right)),
        CGEventType::MouseMoved => {
            let point = cg_event.location();
            Some(EventType::MouseMove {
                x: point.x,
                y: point.y,
            })
        }
        CGEventType::KeyDown => {
            let code = cg_event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            Some(EventType::KeyPress(key_from_code(code.try_into().ok()?)))
        }
        CGEventType::KeyUp => {
            let code = cg_event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            Some(EventType::KeyRelease(key_from_code(code.try_into().ok()?)))
        }
        CGEventType::FlagsChanged => {
            let code = cg_event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            let code = code.try_into().ok()?;
            let flags = cg_event.get_flags();
            match flags_changed_is_press(code, flags.bits())? {
                true => Some(EventType::KeyPress(key_from_code(code))),
                false => Some(EventType::KeyRelease(key_from_code(code))),
            }
        }
        CGEventType::ScrollWheel => {
            let delta_y =
                cg_event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
            let delta_x =
                cg_event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
            Some(EventType::Wheel { delta_x, delta_y })
        }
        _ => None,
    };
    if let Some(event_type) = option_type {
        // Avoid calling create_string_for_key which asserts main-thread only on macOS
        return Some(Event {
            event_type,
            time: SystemTime::now(),
            name: None,
        });
    }
    None
}
