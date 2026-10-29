#!/usr/bin/env python3
"""Observe an actual desktop on a dedicated runner; never inject input."""
import argparse
import asyncio
import base64
import json
import os
from pathlib import Path
import re
import shutil
import signal
import struct
import sys
import tempfile
import time

class GateFailure(Exception):
    pass

def require(condition, message):
    if not condition:
        raise GateFailure(message)

def integer(value, low, high):
    require(type(value) is int and low <= value <= high, "Invalid numeric metadata")
    return value

def dimensions(value, maximum=32768):
    return integer(value.get("width"), 1, maximum), integer(value.get("height"), 1, maximum)

def identifier(value):
    require(type(value) in (str, int), "Missing display identifier")
    value = str(value)
    require(0 < len(value) <= 256, "Invalid display identifier")
    return value

async def read_bounded(reader, limit):
    result = bytearray()
    while chunk := await reader.read(65536):
        result.extend(chunk)
        require(len(result) <= limit, "Subprocess output exceeded limit")
    return bytes(result)

async def stop(process):
    if process.returncode is None:
        if os.name == "nt":
            killer = await asyncio.create_subprocess_exec("taskkill.exe", "/PID", str(process.pid), "/T", "/F",
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
            try:
                await asyncio.wait_for(killer.wait(), 5)
            finally:
                if killer.returncode is None:
                    killer.kill()
                    await asyncio.wait_for(killer.wait(), 5)
                if process.returncode is None:
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass
        else:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    await asyncio.wait_for(process.wait(), 5)

async def spawn(command):
    return await asyncio.create_subprocess_exec(*command, stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=os.name != "nt")

async def run(command, data=b"", limit=34 * 1024 * 1024):
    process = await spawn(command)
    async def write():
        try:
            process.stdin.write(data)
            await process.stdin.drain()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            process.stdin.close()
    tasks = [asyncio.create_task(write()), asyncio.create_task(read_bounded(process.stdout, limit)),
        asyncio.create_task(read_bounded(process.stderr, 65536)), asyncio.create_task(process.wait())]
    try:
        async with asyncio.timeout(25):
            results = await asyncio.gather(*tasks)
        require(process.returncode == 0, "Subprocess returned failure")
        return results[1]
    finally:
        for task in tasks:
            task.cancel()
        await stop(process)
        await asyncio.gather(*tasks, return_exceptions=True)

def command_for(path):
    if path.suffix.lower() == ".ps1":
        return ["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-File", str(path)]
    return [sys.executable, str(path)] if path.suffix.lower() == ".py" else [str(path)]

async def request(command, operation, **arguments):
    raw = await run(command, json.dumps({"command": operation, **arguments}).encode())
    value = json.loads(raw)
    require(isinstance(value, dict) and not value.get("error"), "Helper rejected observation request")
    return value

async def decode(ffmpeg, data, codec, frames, expected):
    raw = await run([ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-max_alloc", "268435456",
        "-f", codec, "-i", "pipe:0", "-threads", "1", "-frames:v", str(frames), "-f", "framemd5", "pipe:1"], data, 65536)
    text = raw.decode("ascii")
    sizes = re.findall(r"^#dimensions\s+\d+:\s*(\d+)x(\d+)\s*$", text, re.M)
    rows = [line for line in text.splitlines() if line.strip() and not line.startswith("#")]
    require(len(sizes) == 1 and tuple(map(int, sizes[0])) == expected, "Decoded dimensions differ from metadata")
    require(len(rows) == frames and all(re.search(r",\s*[0-9a-fA-F]{32}\s*$", row) for row in rows),
        "FFmpeg did not decode the required frame count")

async def frame(command, ffmpeg, display):
    value = await request(command, "frame", display_id=display)
    require(identifier(value.get("display_id")) == display, "Frame belongs to another display")
    size = dimensions(value)
    geometry = integer(value.get("geometry_version"), 1, (1 << 64) - 1)
    require(value.get("mime_type") in ("image/jpeg", "image/jpg"), "Frame is not JPEG")
    encoded = value.get("image_base64")
    require(isinstance(encoded, str) and len(encoded) <= 32 * 1024 * 1024, "Invalid encoded JPEG size")
    data = base64.b64decode(encoded, validate=True)
    require(0 < len(data) <= 24 * 1024 * 1024, "Invalid JPEG size")
    await decode(ffmpeg, data, "mjpeg", 1, size)
    return size, geometry

def inspect_tree(tree):
    require(isinstance(tree, dict), "Accessibility tree is absent")
    pending, count, placeholders, secure_count = [(tree, 0)], 0, 0, 0
    while pending:
        node, depth = pending.pop()
        require(isinstance(node, dict) and depth <= 6, "Invalid accessibility tree structure")
        if node == {"truncated": True}:
            placeholders += 1
            require(placeholders <= 8000, "Too many truncated accessibility nodes")
            continue
        count += 1
        require(count <= 200, "Accessibility node limit exceeded")
        children = node.get("children", [])
        require(isinstance(children, list) and len(children) <= 40, "Accessibility child limit exceeded")
        require(all(not isinstance(v, str) or len(v) <= 512 for v in node.values()), "Accessibility string limit exceeded")
        role = str(node.get("role", "")).lower()
        if node.get("password") is True or node.get("secure") is True or "password" in role or "securetext" in role:
            secure_count += 1
            require(not children, "Secure control exposes descendants")
            for key in ("value", "text", "selected_text", "content", "document_text"):
                require(node.get(key) in (None, "", "[redacted]", "[secure]"), "Secure control exposes content")
        pending.extend((child, depth + 1) for child in children)
    require(count >= 2, "Accessibility tree has no descendants")
    return count, secure_count

def parameter_set(value, kind):
    require(isinstance(value, str) and len(value) <= 90000, "Missing H.264 parameter set")
    raw = base64.b64decode(value, validate=True)
    require(0 < len(raw) <= 65536 and raw[0] & 31 == kind and not raw[0] & 128, "Invalid H.264 parameter-set NAL")
    return b"\x00\x00\x00\x01" + raw

def annex_b(payload):
    result, offset, count, idr = bytearray(), 0, 0, False
    while offset < len(payload):
        require(offset + 4 <= len(payload), "Truncated AVCC length")
        size = struct.unpack_from(">I", payload, offset)[0]
        offset += 4
        require(size > 0 and offset + size <= len(payload), "Invalid AVCC size")
        nal = payload[offset:offset + size]
        offset += size
        count += 1
        kind = nal[0] & 31
        require(count <= 256 and not nal[0] & 128 and 1 <= kind <= 23 and kind not in (1, 2, 3, 4), "Invalid or non-IDR H.264 NAL")
        idr |= kind == 5
        result.extend(b"\x00\x00\x00\x01" + nal)
    require(idr, "Video packet has no IDR")
    return bytes(result)

async def video(command, ffmpeg, display, geometry, desktop):
    process = await spawn(command + ["--video", "--display-id", display])
    process.stdin.close()
    errors = asyncio.create_task(read_bounded(process.stderr, 65536))
    data, previous, size = bytearray(), -1, None
    try:
        async with asyncio.timeout(25):
            for _ in range(2):
                prefix = await process.stdout.readexactly(12)
                require(prefix[:4] == b"CPV1", "Invalid video magic")
                header_size, payload_size = struct.unpack(">II", prefix[4:])
                require(0 < header_size <= 65536 and 0 < payload_size <= 8 * 1024 * 1024, "Oversized video packet")
                header = json.loads(await process.stdout.readexactly(header_size))
                integer(header.get("version"), 1, 1)
                require(header.get("codec") == "h264" and header.get("keyframe") is True, "Invalid video metadata")
                require(identifier(header.get("display_id")) == display and integer(header.get("geometry_version"), 1, (1 << 64) - 1) == geometry, "Video display/geometry mismatch")
                current = dimensions(header, 4096)
                require(size is None or size == current, "Video dimensions changed")
                size = current
                require((integer(header.get("desktop_width"), 1, 32768), integer(header.get("desktop_height"), 1, 32768)) == desktop, "Video desktop dimensions differ")
                timestamp = integer(header.get("timestamp_us"), 0, (1 << 64) - 1)
                require(timestamp > previous, "Video timestamps are not monotonic")
                previous = timestamp
                data.extend(parameter_set(header.get("sps_base64"), 7))
                data.extend(parameter_set(header.get("pps_base64"), 8))
                data.extend(annex_b(await process.stdout.readexactly(payload_size)))
                if errors.done():
                    errors.result()
        await decode(ffmpeg, bytes(data), "h264", 2, size)
        return size
    finally:
        errors.cancel()
        await stop(process)
        await asyncio.gather(errors, return_exceptions=True)

async def acceptance(args, report):
    require(sys.platform == {"macos": "darwin", "windows": "win32", "gnome": "linux"}[args.platform], "Runner OS does not match selected platform")
    root = Path(__file__).resolve().parents[1]
    if args.platform == "gnome":
        require(os.environ.get("XDG_SESSION_TYPE") == "wayland" and "GNOME" in os.environ.get("XDG_CURRENT_DESKTOP", "").upper() and os.environ.get("DBUS_SESSION_BUS_ADDRESS"), "An unlocked GNOME Wayland user session is required")
        import hashlib
        installed = Path(os.environ.get("CONTROL_PLANE_WAYLAND_DAEMON_DIRECTORY", Path.home() / ".local/share/control-plane/linux"))
        for source in (root / "helpers/linux").glob("*.py"):
            deployed = installed / source.name
            require(deployed.is_file() and hashlib.sha256(deployed.read_bytes()).digest() == hashlib.sha256(source.read_bytes()).digest(), "Redeploy the current helper and authorize its GNOME portal session locally before acceptance")
    defaults = {"macos": root / "helpers/macos/build/control-plane-desktop", "windows": root / "helpers/windows/DesktopHelper.ps1", "gnome": root / "helpers/linux/wayland_helper.py"}
    helper = Path(os.environ.get("CONTROL_PLANE_DESKTOP_HELPER", defaults[args.platform]))
    video_path = Path(os.environ.get("CONTROL_PLANE_VIDEO_HELPER", root / "helpers/windows/VideoHelper.py" if args.platform == "windows" else helper))
    require(helper.is_file() and video_path.is_file(), "Desktop helper is missing")
    ffmpeg = os.environ.get("CONTROL_PLANE_FFMPEG") or shutil.which("ffmpeg")
    require(ffmpeg, "FFmpeg is required for image/video decoding")
    command = command_for(helper)
    report["gate"] = "status"
    status = await request(command, "status")
    require(status.get("capture_available") is True and status.get("available") is True, "Desktop is unavailable, locked, or missing permission")
    displays = status.get("displays")
    require(isinstance(displays, list) and args.minimum_displays <= len(displays) <= 16, "Required display count is unavailable")
    ids = [identifier(item.get("id")) for item in displays]
    require(len(set(ids)) == len(ids), "Duplicate display identifiers")
    primary = next((item for item in displays if item.get("primary") is True), displays[0])
    ordered = [primary] + [item for item in displays if item is not primary]
    report["checks"].append({"gate": "status", "display_count": len(displays)})
    for index, display in enumerate(ordered[:args.minimum_displays], 1):
        report["gate"] = "display_capture"
        selected = identifier(display["id"])
        selection = await request(command, "select_display", display_id=selected)
        require(identifier(selection.get("selected_display_id")) == selected, "Display selection mismatch")
        first = await frame(command, ffmpeg, selected)
        require(first == await frame(command, ffmpeg, selected), "Display geometry changed")
        report["checks"].append({"gate": "display_capture", "display_index": index, "desktop_dimensions": dimensions(display), "image_dimensions": first[0], "frames_decoded": 2})
    selected = identifier(primary["id"])
    await request(command, "select_display", display_id=selected)
    report["gate"] = "accessibility"
    tree = await request(command, "accessibility", display_id=selected)
    require(tree.get("coordinate_space") and identifier(tree.get("display_id")) == selected, "Accessibility display metadata is absent")
    count, secure = inspect_tree(tree.get("tree"))
    report["checks"].append({"gate": "accessibility", "nodes_checked": count, "secure_nodes_checked": secure})
    report["gate"] = "video"
    captured = await frame(command, ffmpeg, selected)
    size = await video(command_for(video_path), ffmpeg, selected, captured[1], dimensions(primary))
    report["checks"].append({"gate": "video", "frames_decoded": 2, "encoded_dimensions": size})

async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", required=True, choices=("macos", "windows", "gnome"))
    parser.add_argument("--minimum-displays", type=int, choices=range(1, 9), default=1)
    parser.add_argument("--report-path", required=True, type=Path)
    args = parser.parse_args()
    report = {"schema": 1, "platform": args.platform, "passed": False, "checks": [], "untested": [
        "Mouse and keyboard input", "Lock and permission revocation transitions", "Pixel freshness and visual correctness", "Known-sentinel password-redaction fixture"]}
    started = time.monotonic()
    try:
        async with asyncio.timeout(600):
            await acceptance(args, report)
        report["passed"] = True
    except Exception as error:
        report["failure"] = str(error) if isinstance(error, GateFailure) else type(error).__name__
    report["duration_seconds"] = round(time.monotonic() - started, 3)
    args.report_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=args.report_path.parent, delete=False) as output:
        temporary = Path(output.name)
        json.dump(report, output, indent=2)
        output.write("\n")
    try:
        os.replace(temporary, args.report_path)
    finally:
        temporary.unlink(missing_ok=True)
    print("Desktop observation acceptance:", "PASS" if report["passed"] else "FAIL")
    return 0 if report["passed"] else 1

if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
