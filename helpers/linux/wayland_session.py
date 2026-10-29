"""Portal session policy, independent of D-Bus/GStreamer for deterministic tests."""
import asyncio
import base64
import hashlib
import json
import math
import os
from pathlib import Path
import time
import uuid

from wayland_displays import granted_streams, status_displays

MAX_IMAGE = 24 * 1024 * 1024


class RestoreToken:
    def __init__(self, path):
        self.path = Path(path)

    def consume(self):
        try:
            stat = self.path.lstat()
            if self.path.is_symlink() or stat.st_uid != os.getuid() or stat.st_mode & 0o077:
                raise RuntimeError("restore token permissions must be owner-only")
            data = self.path.read_text()
            self.clear()  # Portal restore tokens are single-use, including failed attempts.
            if not 0 < len(data) <= 4096:
                raise RuntimeError("invalid restore token")
            return data
        except FileNotFoundError:
            return None

    def save(self, value):
        if not isinstance(value, str) or not 0 < len(value) <= 4096:
            raise RuntimeError("invalid restore token")
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.path.parent.stat().st_uid != os.getuid() or self.path.parent.stat().st_mode & 0o077:
            raise RuntimeError("restore token directory must be owner-only")
        temporary = self.path.with_name(self.path.name + "." + uuid.uuid4().hex + ".tmp")
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "w") as output:
                output.write(value)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, self.path)
        finally:
            temporary.unlink(missing_ok=True)

    def clear(self):
        self.path.unlink(missing_ok=True)


class PortalSession:
    def __init__(self, portal, media, tokens, clock=time.monotonic):
        self.portal, self.media, self.tokens = portal, media, tokens
        self.clock = clock
        self.session = None
        self.stream = None
        self.streams = []
        self.reason = "Local desktop authorization required: run wayland_helper.py --authorize"
        self.geometry = None
        self.last_frame = None
        self.capture_size = None
        self.epoch = 0
        self.guard = asyncio.Lock()
        portal.on_invalidated = self.invalidate

    async def invalidate(self, reason="Desktop permission revoked or session closed", forget=True):
        # Mark unavailable before awaiting anything: no pending request can inject
        # into a session after it is invalidated, even while media is stopping.
        self.epoch += 1
        session, self.session = self.session, None
        self.stream = None
        self.streams = []
        self.geometry = None
        self.last_frame = None
        self.capture_size = None
        self.reason = reason
        if forget:
            self.tokens.clear()
        await self.media.stop()
        if session:
            await self.portal.close(session)

    async def ready(self):
        if not self.session:
            raise RuntimeError(self.reason)
        if hasattr(self.media, "check_bus"):
            self.media.check_bus()
        if not await self.portal.unlocked():
            await self.invalidate("Desktop locked or lock state unavailable; authorize locally after unlocking")
            raise RuntimeError(self.reason)
        if self.media.error:
            await self.invalidate("PipeWire capture stopped: " + self.media.error)
            raise RuntimeError(self.reason)

    async def authorize(self):
        async with self.guard:
            if self.session:
                await self.ready()
                return await self.status()
            if not await self.portal.unlocked():
                raise RuntimeError("Unlock the GNOME desktop before authorizing")
            handle = None
            fd = None
            start_epoch = self.epoch
            try:
                capabilities = await self.portal.capabilities()
                if capabilities["devices"] & 3 != 3:
                    raise RuntimeError("Portal does not support both keyboard and pointer")
                handle = await self.portal.create()
                options = {"types": 3}
                if capabilities["version"] >= 2:
                    options["persist_mode"] = 2
                    restore = self.tokens.consume()
                    if restore:
                        options["restore_token"] = restore
                await self.portal.select_devices(handle, options)
                # Persistence belongs to RemoteDesktop, never SelectSources in a combined session.
                await self.portal.select_sources(handle, {"types": 1, "multiple": True,
                                                         "cursor_mode": 2 if capabilities["cursors"] & 2 else 1})
                result = await self.portal.start(handle)
                if result.get("devices", 0) & 3 != 3:
                    raise RuntimeError("Local user did not grant keyboard and pointer access")
                streams = result.get("streams", [])
                self.streams=granted_streams(streams)
                first=self.streams[0];node=first['node'];properties=first['properties'];size=[first['width'],first['height']]
                fd = await self.portal.open_remote(handle)
                media_fd, fd = fd, None  # media owns the descriptor, including failure paths.
                await self.media.start(media_fd, node, properties.get("pipewire-serial"))
                initial = await self.media.snapshot()
                self.capture_size = (initial[1], initial[2])
                if self.epoch != start_epoch:
                    raise RuntimeError("Desktop authorization invalidated while starting")
                self.session = handle
                self.stream = {"node": node, "width": size[0], "height": size[1], "properties": properties}
                self.reason = "Ready"
                if result.get("restore_token"):
                    self.tokens.save(result["restore_token"])
                return await self.status()
            except (Exception, asyncio.CancelledError):
                if fd is not None:
                    os.close(fd)
                await self.media.stop()
                if handle:
                    await self.portal.close(handle)
                self.session = None
                self.stream = None
                self.streams = []
                raise

    async def status(self):
        if self.session:
            try:
                await self.ready()
            except RuntimeError:
                pass
        available = self.session is not None
        return {"available": available, "platform": "linux-wayland", "reason": self.reason,
                "capture_available": available, "input_available": available,
                "video_streaming": bool(available and self.media.video_available), "accessibility": True,
                "displays": status_displays(self) if available else [],
                "selected_display_id": str(self.stream["node"]) if self.stream else None}


    async def frame(self, include_image=True):
        async with self.guard:
            await self.ready()
            epoch = self.epoch
            jpeg, width, height, timestamp = await self.media.snapshot()
            await self.ready()
            if self.epoch != epoch:
                raise RuntimeError("Desktop session changed while capturing")
            if not 0 < len(jpeg) <= MAX_IMAGE or not jpeg.startswith(b"\xff\xd8"):
                raise RuntimeError("Invalid or oversized JPEG frame")
            if not 0 < width <= 16384 or not 0 < height <= 16384:
                raise RuntimeError("Invalid captured geometry")
            if self.capture_size != (width, height):
                await self.invalidate("Monitor geometry changed; authorize locally to resume")
                raise RuntimeError("Desktop geometry changed; authorize locally to resume")
            if self.clock() - timestamp > 2:
                await self.invalidate("PipeWire capture stalled; authorize locally to resume")
                raise RuntimeError("PipeWire screenshot is stale")
            # Include encoded and logical dimensions: monitor scaling changes must
            # invalidate pointer coordinates even when the portal node is unchanged.
            dimensions = [epoch, self.stream["node"], self.stream["width"], self.stream["height"], width, height]
            self.geometry = int.from_bytes(hashlib.sha256(json.dumps(dimensions).encode()).digest()[:6], "big")
            self.last_frame = self.clock()
            self.frame_size = (width, height)
            result = {"display_id":str(self.stream["node"]),"mime_type": "image/jpeg", "width": width, "height": height, "geometry_version": self.geometry}
            if include_image:
                result["image_base64"] = base64.b64encode(jpeg).decode()
            return result

    async def action(self, request):
        async with self.guard:
            if request.get("display_id") is not None and (self.stream is None or str(request["display_id"])!=str(self.stream["node"])):
                raise RuntimeError("Selected display changed; capture a new frame")
            await self.ready()
            if self.geometry is None or request.get("geometry_version") != self.geometry:
                raise RuntimeError("Desktop geometry changed; capture a new frame")
            if self.last_frame is None or self.clock() - self.last_frame > 5:
                raise RuntimeError("Screenshot stale; capture a new frame")
            latest = await self.media.snapshot()
            if self.capture_size != (latest[1], latest[2]):
                await self.invalidate("Monitor geometry changed; authorize locally to resume")
                raise RuntimeError("Desktop geometry changed; authorize locally to resume")
            if self.clock() - latest[3] > 2:
                await self.invalidate("PipeWire capture stalled; authorize locally to resume")
                raise RuntimeError("PipeWire screenshot is stale")
            action = request.get("action", {})
            kind = action.get("type")
            epoch = self.epoch
            async def send(method, *values):
                await self.ready()
                if self.epoch != epoch:
                    raise RuntimeError("Desktop session changed")
                try:
                    await self.portal.notify(self.session, method, *values)
                except Exception:
                    await self.invalidate("Portal rejected input; authorize locally to resume")
                    raise
            if kind == "click":
                x, y = action.get("x"), action.get("y")
                if any(type(v) not in (int, float) or not math.isfinite(v) for v in (x, y)):
                    raise RuntimeError("Click coordinates must be finite numbers")
                if not (0 <= x < self.frame_size[0] and 0 <= y < self.frame_size[1]):
                    raise RuntimeError("Click outside captured display")
                button = {"left": 272, "right": 273}.get(action.get("button", "left"))
                if button is None:
                    raise RuntimeError("Unsupported pointer button")
                await send("NotifyPointerMotionAbsolute", self.stream["node"],
                           x * self.stream["width"] / self.frame_size[0], y * self.stream["height"] / self.frame_size[1])
                await send("NotifyPointerButton", button, 1)
                await send("NotifyPointerButton", button, 0)
            elif kind == "scroll":
                dx, dy = action.get("dx", action.get("delta_x")), action.get("dy", action.get("delta_y"))
                if any(type(v) not in (int, float) or not math.isfinite(v) or abs(v) > 1200 for v in (dx, dy)):
                    raise RuntimeError("Scroll values must be bounded finite numbers")
                await send("NotifyPointerAxis", float(dx), float(dy))
            elif kind in ("key", "text"):
                if kind == "text":
                    text = action.get("text")
                    if not isinstance(text, str) or len(text.encode()) > 4096:
                        raise RuntimeError("Text input exceeds 4096 bytes")
                    controls = {"\n": 0xff0d, "\r": 0xff0d, "\t": 0xff09, "\b": 0xff08}
                    symbols = [controls.get(c, ord(c) if ord(c) < 256 else 0x01000000 | ord(c)) for c in text]
                    modifiers = []
                else:
                    keys = {"enter": 0xff0d, "return": 0xff0d, "tab": 0xff09, "space": 32,
                            "backspace": 0xff08, "escape": 0xff1b, "left": 0xff51, "up": 0xff52,
                            "right": 0xff53, "down": 0xff54, "delete": 0xffff, "home": 0xff50,
                            "end": 0xff57, "pageup": 0xff55, "pagedown": 0xff56}
                    keys.update({key: ord(key) for key in "acvxzs"})
                    key = keys.get(str(action.get("key", "")).lower())
                    requested = action.get("modifiers", [])
                    mapping = {"control": 0xffe3, "shift": 0xffe1, "option": 0xffe9, "command": 0xffeb}
                    if key is None or not isinstance(requested, list) or len(requested) > 4 or any(v not in mapping for v in requested):
                        raise RuntimeError("Unsupported key or modifiers")
                    symbols, modifiers = [key], [mapping[v] for v in requested]
                pressed = []
                try:
                    for symbol in modifiers:
                        await send("NotifyKeyboardKeysym", symbol, 1)
                        pressed.append(symbol)
                    for symbol in symbols:
                        await send("NotifyKeyboardKeysym", symbol, 1)
                        pressed.append(symbol)
                        await send("NotifyKeyboardKeysym", symbol, 0)
                        pressed.pop()
                finally:
                    for symbol in reversed(pressed):
                        # Only release while the same session is valid; a revoked
                        # portal resets input itself and must not receive new events.
                        if self.session and self.epoch == epoch:
                            await self.portal.notify(self.session, "NotifyKeyboardKeysym", symbol, 0)
            else:
                raise RuntimeError("Unsupported desktop action")
            return {"success": True}
