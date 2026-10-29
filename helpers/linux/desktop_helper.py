#!/usr/bin/env python3
"""Optional GNOME/X11 development backend. Wayland requires a portal backend.

Dependencies: Python 3, xrandr, xdotool, ImageMagick import, gdbus.
Runs as the graphical user. Does not bypass the compositor or screen lock.
"""
import base64
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import argparse
from x11_displays import displays, selected_display, geometry_version
from wayland_accessibility import capture_tree


def run(args, binary=False):
    result = subprocess.run(args, check=True, capture_output=True, timeout=7)
    return result.stdout if binary else result.stdout.decode("utf-8", "strict")


def ready():
    if os.environ.get("XDG_SESSION_TYPE") != "x11" or not os.environ.get("DISPLAY"):
        raise RuntimeError("Linux desktop requires an X11 user session; use the Wayland portal helper for Wayland sessions")
    for tool in ("xrandr", "xdotool", "import", "gdbus"):
        if not shutil.which(tool):
            raise RuntimeError("Missing desktop dependency: " + tool)
    # Fail closed if the lock service is absent or cannot confirm unlocked state.
    state = run(["gdbus", "call", "--session", "--dest", "org.gnome.ScreenSaver",
                 "--object-path", "/org/gnome/ScreenSaver", "--method", "org.gnome.ScreenSaver.GetActive"])
    if state.strip() != "(false,)":
        raise RuntimeError("GNOME screensaver is active or lock state cannot be established")
    screen = run(["xrandr", "--current"])
    match = re.search(r"current (\d+) x (\d+)", screen)
    if not match:
        raise RuntimeError("Cannot determine X11 desktop geometry")
    width, height = map(int, match.groups())
    geometry = int.from_bytes(hashlib.sha256(screen.encode()).digest()[:6], "big")
    return width, height, geometry


def execute(request):
    command = request.get("command")
    if command == "status":
        try:
            ready()
            selected=selected_display(request.get("display_id"))
            return {"available": True, "platform": "linux-x11", "reason": "GNOME X11 snapshot backend",
                    "capture_available": True, "input_available": True,
                    "accessibility": True, "displays": displays(), "selected_display_id": selected["id"]}
        except Exception as error:
            return {"available": False, "platform": "linux", "reason": str(error)}
    ready()
    selected=selected_display(request.get("display_id"));width,height=selected['width'],selected['height'];geometry=geometry_version(selected)
    if command=='select_display':return {'selected_display_id':selected['id']}
    if command=='accessibility':
        import asyncio
        return asyncio.run(capture_tree([selected['x'],selected['y'],width,height]))
    if command == "frame":
        if selected['x']<0 or selected['y']<0:
            raise RuntimeError('Negative X11 monitor origins require a compositor-supported capture backend; use Wayland portal')
        data = run(["import", "-window", "root", "-crop",f"{width}x{height}+{selected['x']}+{selected['y']}", "+repage", "-quality", "70", "jpeg:-"], binary=True)
        if len(data) > 24 * 1024 * 1024:
            raise RuntimeError("Screenshot exceeds size limit")
        return {"image_base64": base64.b64encode(data).decode(), "mime_type": "image/jpeg",
                "display_id": selected["id"], "width": width, "height": height, "geometry_version": geometry}
    if command != "action":
        raise RuntimeError("Unsupported desktop command")
    if request.get("geometry_version") != geometry:
        raise RuntimeError("Desktop geometry changed; capture a new frame")
    action = request.get("action", {})
    kind = action.get("type")
    if kind == "click":
        x, y = float(action["x"]), float(action["y"])
        if not math.isfinite(x) or not math.isfinite(y) or not (0 <= x < width and 0 <= y < height):
            raise RuntimeError("Click outside captured desktop")
        button = action.get("button", "left")
        if button not in ("left", "right"):
            raise RuntimeError("Unsupported button")
        run(["xdotool", "mousemove", "--sync", str(selected["x"]+int(x)), str(selected["y"]+int(y)), "click", "1" if button == "left" else "3"])
    elif kind == "text":
        text = action["text"]
        if not isinstance(text, str) or len(text.encode()) > 4096:
            raise RuntimeError("Text exceeds limit")
        run(["xdotool", "type", "--clearmodifiers", "--", text])
    elif kind == "key":
        keys = {"enter": "Return", "return": "Return", "tab": "Tab", "space": "space", "backspace": "BackSpace",
                "escape": "Escape", "left": "Left", "right": "Right", "up": "Up", "down": "Down", "delete": "Delete",
                "home": "Home", "end": "End", "pageup": "Prior", "pagedown": "Next"}
        keys.update({key: key for key in "acvxzs"})
        key = keys.get(action.get("key", "").lower())
        if not key:
            raise RuntimeError("Unsupported key")
        modifiers = {"control": "ctrl", "shift": "shift", "option": "alt", "command": "super"}
        requested = action.get("modifiers", [])
        if len(requested) > 4 or any(value not in modifiers for value in requested):
            raise RuntimeError("Unsupported key modifiers")
        run(["xdotool", "key", "--clearmodifiers", "+".join([modifiers[m] for m in requested] + [key])])
    elif kind == "scroll":
        dx, dy = action.get("dx", action.get("delta_x")), action.get("dy", action.get("delta_y"))
        if any(type(v) is not int or abs(v) > 1200 for v in (dx, dy)):
            raise RuntimeError("Invalid scroll distances")
        for value, negative, positive in ((dy, "4", "5"), (dx, "6", "7")):
            if value:
                run(["xdotool", "click", "--repeat", str(min(20, max(1, abs(value) // 60))), "--delay", "10", negative if value < 0 else positive])
    else:
        raise RuntimeError("Unsupported action")
    return {"success": True}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--stdio-session", action="store_true")
    parser.add_argument("--video", action="store_true")
    arguments = parser.parse_args()
    if arguments.video:
        print("GNOME X11 fallback does not provide H264; use Wayland helper or snapshots", file=sys.stderr)
        sys.exit(1)
    while True:
        try:
            raw = sys.stdin.buffer.readline(65537) if arguments.stdio_session else sys.stdin.buffer.read(65537)
            if not raw:
                break
            if len(raw) > 65536 or arguments.stdio_session and not raw.endswith(b"\n"):
                raise RuntimeError("Request exceeds limit or has invalid framing")
            print(json.dumps(execute(json.loads(raw))), flush=True)
        except Exception as error:
            print(json.dumps({"error": str(error)}), flush=True)
            if not arguments.stdio_session:
                sys.exit(1)
        if not arguments.stdio_session:
            break
