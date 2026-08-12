#!/usr/bin/env python3
"""Post a key combination as a CGEvent at the HID tap location.

VoiceX's global hotkey hook (rdev) installs its event tap at
``kCGHIDEventTap``. That tap sits *before* the session tap in the event
pipeline, so events injected by AppleScript's ``System Events`` — which arrive
at the session level — are invisible to it. Driving the hotkey therefore
requires posting at the HID tap, which is what this script does.

Uses ctypes against CoreGraphics so the harness needs nothing but the system
Python. Requires Accessibility permission for the calling terminal; without it
CGEventPost silently does nothing, so callers must verify the effect rather
than trusting a zero exit code.

Usage:
    cgevent_key.py --key 15 --mods option,command
"""

import argparse
import ctypes
import ctypes.util
import sys
import time

# CGEventFlags
FLAGS = {
    "command": 0x00100000,
    "shift": 0x00020000,
    "option": 0x00080000,
    "control": 0x00040000,
}

# Virtual keycodes for the modifier keys themselves.
MODIFIER_KEYCODES = {
    "command": 55,
    "shift": 56,
    "option": 58,
    "control": 59,
}

K_CG_HID_EVENT_TAP = 0
K_CG_EVENT_SOURCE_STATE_HID_SYSTEM = 1


def load_core_graphics():
    path = ctypes.util.find_library("CoreGraphics")
    if not path:
        sys.exit("CoreGraphics framework not found")
    cg = ctypes.CDLL(path)

    cg.CGEventSourceCreate.restype = ctypes.c_void_p
    cg.CGEventSourceCreate.argtypes = [ctypes.c_int32]
    cg.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
    cg.CGEventCreateKeyboardEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint16,
        ctypes.c_bool,
    ]
    cg.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
    cg.CGEventPost.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
    cg.CFRelease.argtypes = [ctypes.c_void_p]
    return cg


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--key", type=int, required=True, help="virtual keycode")
    parser.add_argument(
        "--mods",
        default="",
        help="comma-separated: command,option,control,shift",
    )
    parser.add_argument(
        "--gap-ms", type=int, default=20, help="delay between posted events"
    )
    args = parser.parse_args()

    mods = [m.strip() for m in args.mods.split(",") if m.strip()]
    for mod in mods:
        if mod not in FLAGS:
            sys.exit(f"Unknown modifier: {mod}")

    cg = load_core_graphics()
    source = cg.CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM)
    if not source:
        sys.exit("CGEventSourceCreate failed")

    gap = args.gap_ms / 1000.0

    def post(keycode, keydown, flags):
        event = cg.CGEventCreateKeyboardEvent(source, keycode, keydown)
        if not event:
            sys.exit("CGEventCreateKeyboardEvent failed")
        cg.CGEventSetFlags(event, flags)
        cg.CGEventPost(K_CG_HID_EVENT_TAP, event)
        cg.CFRelease(event)
        time.sleep(gap)

    # Press modifiers in order, accumulating their flags, then the key, then
    # release everything in reverse — the same shape a real keystroke has.
    accumulated = 0
    for mod in mods:
        accumulated |= FLAGS[mod]
        post(MODIFIER_KEYCODES[mod], True, accumulated)

    post(args.key, True, accumulated)
    post(args.key, False, accumulated)

    for mod in reversed(mods):
        accumulated &= ~FLAGS[mod]
        post(MODIFIER_KEYCODES[mod], False, accumulated)

    cg.CFRelease(source)


if __name__ == "__main__":
    main()
