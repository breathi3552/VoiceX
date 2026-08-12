#!/usr/bin/env python3
"""Post a real left click (and optionally a drag) as CGEvents.

AppleScript's ``System Events`` ``click at`` performs an accessibility click on
whatever element is under the point, which does not move keyboard focus — so a
following Cmd-A still goes to wherever focus already was (Safari's address bar,
typically). Posting genuine mouse events at the HID tap does move focus, the
same way a user's click does.

Coordinates are global display points, origin at the top-left of the main
display — the same space AppleScript's window ``bounds`` uses.

Usage:
    cgevent_click.py --at 960,200
    cgevent_click.py --at 100,200 --to 400,260     # click-drag (select text)
"""

import argparse
import ctypes
import ctypes.util
import sys
import time

K_CG_HID_EVENT_TAP = 0
K_CG_EVENT_SOURCE_STATE_HID_SYSTEM = 1

K_CG_EVENT_MOUSE_MOVED = 5
K_CG_EVENT_LEFT_MOUSE_DOWN = 1
K_CG_EVENT_LEFT_MOUSE_UP = 2
K_CG_EVENT_LEFT_MOUSE_DRAGGED = 6
K_CG_MOUSE_BUTTON_LEFT = 0


class CGPoint(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


def load_core_graphics():
    path = ctypes.util.find_library("CoreGraphics")
    if not path:
        sys.exit("CoreGraphics framework not found")
    cg = ctypes.CDLL(path)

    cg.CGEventSourceCreate.restype = ctypes.c_void_p
    cg.CGEventSourceCreate.argtypes = [ctypes.c_int32]
    cg.CGEventCreateMouseEvent.restype = ctypes.c_void_p
    cg.CGEventCreateMouseEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint32,
        CGPoint,
        ctypes.c_uint32,
    ]
    cg.CGEventPost.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
    cg.CFRelease.argtypes = [ctypes.c_void_p]
    return cg


def parse_point(value):
    try:
        x, y = value.split(",")
        return CGPoint(float(x), float(y))
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected 'x,y', got {value!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--at", type=parse_point, required=True, help="x,y")
    parser.add_argument("--to", type=parse_point, help="x,y — drag target")
    parser.add_argument("--gap-ms", type=int, default=40)
    parser.add_argument(
        "--steps", type=int, default=10, help="intermediate points in a drag"
    )
    args = parser.parse_args()

    cg = load_core_graphics()
    source = cg.CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM)
    if not source:
        sys.exit("CGEventSourceCreate failed")

    gap = args.gap_ms / 1000.0

    def post(event_type, point):
        event = cg.CGEventCreateMouseEvent(
            source, event_type, point, K_CG_MOUSE_BUTTON_LEFT
        )
        if not event:
            sys.exit("CGEventCreateMouseEvent failed")
        cg.CGEventPost(K_CG_HID_EVENT_TAP, event)
        cg.CFRelease(event)
        time.sleep(gap)

    post(K_CG_EVENT_MOUSE_MOVED, args.at)
    post(K_CG_EVENT_LEFT_MOUSE_DOWN, args.at)

    if args.to:
        # Applications track selection through the intermediate drag points, so
        # a single jump to the end point selects nothing in most text views.
        for step in range(1, args.steps + 1):
            ratio = step / args.steps
            point = CGPoint(
                args.at.x + (args.to.x - args.at.x) * ratio,
                args.at.y + (args.to.y - args.at.y) * ratio,
            )
            post(K_CG_EVENT_LEFT_MOUSE_DRAGGED, point)
        post(K_CG_EVENT_LEFT_MOUSE_UP, args.to)
    else:
        post(K_CG_EVENT_LEFT_MOUSE_UP, args.at)

    cg.CFRelease(source)


if __name__ == "__main__":
    main()
