#!/usr/bin/env python3
"""Persistent GNOME Wayland portal service and bounded stdio/socket RPC proxy.

Start `--daemon` in a graphical user session; explicitly run `--authorize` locally.
The host calls this file with --stdio-session. --video proxies binary CPV1 frames.
"""
import argparse
import asyncio
import base64
import json
import os
from pathlib import Path
import signal
import socket
import struct
import sys
import time

from wayland_displays import select_display
from wayland_accessibility import capture_tree
from wayland_media import GstMedia, packet
from wayland_portal import DBusPortal
from wayland_session import PortalSession, RestoreToken

MAX_REQUEST = 65536
MAX_RESPONSE = 34 * 1024 * 1024


def runtime_path():
    directory = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "control-plane"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = directory.lstat()
    if directory.is_symlink() or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError("Desktop IPC directory must be owned by this user with mode 0700")
    return directory / "wayland.sock"


def read_line(stream, limit=MAX_RESPONSE):
    line = stream.readline(limit + 1)
    if not line or len(line) > limit or not line.endswith(b"\n"):
        raise RuntimeError("Invalid or oversized desktop IPC frame")
    return line


def connect_socket():
    path = runtime_path()
    info = path.lstat()
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError("Desktop socket must be owner-only")
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(100)
    connection.connect(str(path))
    if hasattr(socket, "SO_PEERCRED"):
        _, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid != os.getuid():
            connection.close()
            raise RuntimeError("Desktop service peer identity mismatch")
    return connection


def forward(request):
    encoded = json.dumps(request, separators=(",", ":"), allow_nan=False).encode() + b"\n"
    if len(encoded) > MAX_REQUEST:
        raise RuntimeError("Desktop request exceeds size limit")
    with connect_socket() as connection:
        connection.sendall(encoded)
        with connection.makefile("rb") as stream:
            return json.loads(read_line(stream))


async def video(session, writer):
    last = None
    # Video is read-only: it never grants a desktop control lease or renews one.
    while not writer.is_closing():
        await session.ready()
        if not session.media.video_available:
            raise RuntimeError("H264 encoder unavailable")
        await session.frame(include_image=False)  # No JPEG base64 allocation for video metadata.
        payload, width, height, timestamp, sps, pps = await session.media.video_frame()
        if session.clock() - timestamp > 2:
            raise RuntimeError("H264 capture stalled")
        if timestamp == last:
            await asyncio.sleep(0.01)
            continue
        last = timestamp
        if not (0 < width <= 4096 and 0 < height <= 4096):
            raise RuntimeError("H264 encoded dimensions exceed supported bounds")
        header = {"version": 1, "codec": "h264", "width": width, "height": height,
                  "desktop_width": session.frame_size[0], "desktop_height": session.frame_size[1],
                  "geometry_version": session.geometry, "timestamp_us": int(timestamp * 1000000),
                  "display_id":str(session.stream["node"]), "keyframe": True, "sps_base64": base64.b64encode(sps).decode(), "pps_base64": base64.b64encode(pps).decode()}
        writer.write(packet(header, payload))
        await asyncio.wait_for(writer.drain(), 3)
        await asyncio.sleep(1 / 30)


async def dispatch(session, request):
    command = request.get("command")
    if command == "authorize":
        return await session.authorize()
    if command == "status":
        return await session.status()
    if command == "select_display":
        return await select_display(session,request.get("display_id"))
    if command == "frame":
        if request.get("display_id") is not None:
            await select_display(session,request["display_id"])
        return await session.frame()
    if command == "action":
        return await session.action(request)
    if command == "stop":
        await session.invalidate("Desktop stopped; authorize locally to resume")
        return {"success": True}
    if command == "accessibility":
        await session.ready()
        epoch=session.epoch
        if request.get("display_id") is not None and str(request["display_id"])!=str(session.stream["node"]):
            raise RuntimeError("Selected display changed")
        position=session.stream["properties"].get("position")
        bounds=None if position is None else [position[0],position[1],session.stream["width"],session.stream["height"]]
        result=await capture_tree(bounds)
        await session.ready()
        if session.epoch!=epoch:raise RuntimeError("Desktop changed during inspection")
        result["display_id"]=str(session.stream["node"])
        result["capture_size"]=session.capture_size
        return result
    raise RuntimeError("Unsupported desktop command")


async def cancellable_request(session, request, reader):
    operation = asyncio.create_task(dispatch(session, request))
    disconnected = asyncio.create_task(reader.read(1))
    try:
        done, _ = await asyncio.wait((operation, disconnected), return_when=asyncio.FIRST_COMPLETED)
        if disconnected in done and operation not in done:
            operation.cancel()
            try:
                await operation
            except asyncio.CancelledError:
                pass
            await session.invalidate("Desktop client disconnected during operation; authorize locally to resume")
            raise RuntimeError("Desktop request cancelled")
        return await operation
    finally:
        disconnected.cancel()
        if not operation.done():
            operation.cancel()


async def daemon():
    if sys.platform != "linux" or os.environ.get("XDG_SESSION_TYPE") != "wayland":
        raise RuntimeError("Wayland helper requires a Linux Wayland graphical user session")
    os.umask(0o077)
    path = runtime_path()
    if path.exists():
        try:
            active = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            active.connect(str(path))
        except ConnectionRefusedError:
            path.unlink()
        else:
            raise RuntimeError("Another Wayland helper is already running")
        finally:
            active.close()
    portal = DBusPortal()
    await portal.connect()
    state = Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state"))) / "control-plane"
    session = PortalSession(portal, GstMedia(), RestoreToken(state / "wayland-restore-token"))
    clients = set()
    video_client = False

    async def handle(reader, writer):
        nonlocal video_client
        peer = writer.get_extra_info("socket")
        if hasattr(socket, "SO_PEERCRED"):
            _, uid, _ = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            if uid != os.getuid():
                writer.close()
                return
        if len(clients) >= 8:
            writer.close()
            return
        clients.add(writer)
        streaming = False
        try:
            line = await asyncio.wait_for(reader.readline(), 5)
            if not line.endswith(b"\n") or len(line) > MAX_REQUEST:
                raise RuntimeError("Invalid desktop request framing")
            request = json.loads(line)
            command = request.get("command")
            if command == "video":
                if video_client:
                    raise RuntimeError("Another native video producer is already attached")
                video_client = streaming = True
                if request.get("display_id") is not None:
                    await select_display(session,request["display_id"])
                await video(session, writer)
                return
            else:
                result = await cancellable_request(session, request, reader)
            response = json.dumps(result, separators=(",", ":"), allow_nan=False).encode() + b"\n"
            if len(response) > MAX_RESPONSE:
                raise RuntimeError("Desktop response exceeds size limit")
            writer.write(response)
            await asyncio.wait_for(writer.drain(), 5)
        except Exception as error:
            if not streaming:
                writer.write(json.dumps({"error": str(error)}).encode() + b"\n")
                try:
                    await asyncio.wait_for(writer.drain(), 2)
                except Exception:
                    pass
        finally:
            if streaming:
                video_client = False
            clients.discard(writer)
            writer.close()
            await writer.wait_closed()

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)

    async def monitor():
        while not stop.is_set():
            if session.session:
                try:
                    await session.ready()
                except Exception:
                    pass
            await asyncio.sleep(0.5)

    server = await asyncio.start_unix_server(handle, path=str(path), limit=MAX_REQUEST)
    os.chmod(path, 0o600)
    watcher = asyncio.create_task(monitor())
    try:
        async with server:
            await stop.wait()
    finally:
        watcher.cancel()
        for writer in list(clients):
            writer.close()
        await session.invalidate("Desktop helper stopped", forget=False)
        path.unlink(missing_ok=True)
        portal.bus.disconnect()


def proxy_video(display_id=None):
    with connect_socket() as connection:
        connection.sendall(json.dumps({"command":"video","display_id":display_id}).encode()+b"\n")
        while True:
            chunk = connection.recv(65536)
            if not chunk:
                return
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()


def proxy_stdio():
    while True:
        line = sys.stdin.buffer.readline(MAX_REQUEST + 1)
        if not line:
            break
        try:
            if len(line) > MAX_REQUEST or not line.endswith(b"\n"):
                raise RuntimeError("Invalid request framing")
            result = forward(json.loads(line))
        except Exception as error:
            result = {"error": str(error)}
        print(json.dumps(result, separators=(",", ":")), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--daemon", action="store_true")
    mode.add_argument("--authorize", action="store_true")
    mode.add_argument("--stdio-session", action="store_true")
    mode.add_argument("--video", action="store_true")
    mode.add_argument("--status", action="store_true")
    parser.add_argument("--display-id")
    args = parser.parse_args()
    try:
        if args.daemon:
            asyncio.run(daemon())
        elif args.video:
            proxy_video(args.display_id)
        elif args.stdio_session:
            proxy_stdio()
        elif args.authorize:
            print(json.dumps(forward({"command": "authorize"})))
        elif args.status:
            print(json.dumps(forward({"command": "status"})))
        else:
            raw = sys.stdin.buffer.read(MAX_REQUEST + 1)
            if len(raw) > MAX_REQUEST:
                raise RuntimeError("Desktop request exceeds size limit")
            print(json.dumps(forward(json.loads(raw))))
    except Exception as error:
        if args.video or args.daemon:
            print(str(error), file=sys.stderr)
        else:
            print(json.dumps({"error": str(error)}))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
