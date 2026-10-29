import asyncio
import base64
import io
import json
import os
from pathlib import Path
import struct
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from wayland_session import PortalSession, RestoreToken
from wayland_media import avc_parameter_sets, packet
from wayland_helper import read_line, cancellable_request
from wayland_portal import DBusPortal


class FakePortal:
    def __init__(self):
        self.calls = []
        self.is_unlocked = True
        self.denied = False
        self.on_invalidated = None
    async def unlocked(self): return self.is_unlocked
    async def capabilities(self): return {"devices": 3, "version": 2, "cursors": 2}
    async def create(self): self.calls.append(("create",)); return "/session/test"
    async def select_devices(self, handle, options): self.calls.append(("devices", options))
    async def select_sources(self, handle, options): self.calls.append(("sources", options))
    async def start(self, handle):
        if self.denied: raise RuntimeError("Local portal authorization cancelled")
        return {"devices": 3, "streams": [[17, {"logical_size": [1920, 1080], "pipewire-serial": 123}]], "restore_token": "rotated-token"}
    async def open_remote(self, handle): return os.open(os.devnull, os.O_RDONLY)
    async def close(self, handle): self.calls.append(("close", handle))
    async def notify(self, handle, method, *values): self.calls.append((method, *values))


class FakeMedia:
    def __init__(self): self.error = None; self.fd = None; self.video_available = True
    async def start(self, fd, node, serial): self.fd = fd; self.node = node; self.serial = serial
    async def stop(self):
        if self.fd is not None: os.close(self.fd); self.fd = None
    async def snapshot(self): return b"\xff\xd8jpeg", 3840, 2160, time.monotonic()


class Sessions(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory()
        os.chmod(self.directory.name, 0o700)
        self.tokens = RestoreToken(Path(self.directory.name) / "token")
        self.portal, self.media = FakePortal(), FakeMedia()
        self.session = PortalSession(self.portal, self.media, self.tokens)
    async def asyncTearDown(self):
        await self.session.invalidate()
        self.directory.cleanup()
    async def test_status_never_authorizes_or_reports_fake_availability(self):
        status = await self.session.status()
        self.assertFalse(status["available"])
        self.assertEqual(self.portal.calls, [])
        with self.assertRaisesRegex(RuntimeError, "authorization required"): await self.session.frame()
    async def test_combined_session_negotiation_and_rotated_token(self):
        self.tokens.save("old-token")
        status = await self.session.authorize()
        self.assertTrue(status["available"])
        devices = next(call[1] for call in self.portal.calls if call[0] == "devices")
        sources = next(call[1] for call in self.portal.calls if call[0] == "sources")
        self.assertEqual(devices, {"types": 3, "persist_mode": 2, "restore_token": "old-token"})
        self.assertNotIn("persist_mode", sources)
        self.assertNotIn("restore_token", sources)
        self.assertEqual(self.tokens.path.read_text(), "rotated-token")
        self.assertEqual(self.tokens.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.media.serial, 123)
    async def test_cancelled_consent_closes_session_without_capture(self):
        self.portal.denied = True
        with self.assertRaisesRegex(RuntimeError, "cancelled"): await self.session.authorize()
        self.assertIsNone(self.session.session)
        self.assertIsNone(self.media.fd)
        self.assertIn(("close", "/session/test"), self.portal.calls)
    async def test_cancelled_authorization_task_cleans_portal_session(self):
        entered = asyncio.Event()
        async def waiting(handle):
            entered.set()
            await asyncio.sleep(30)
        self.portal.start = waiting
        operation = asyncio.create_task(self.session.authorize())
        await entered.wait()
        operation.cancel()
        with self.assertRaises(asyncio.CancelledError): await operation
        self.assertIn(("close", "/session/test"), self.portal.calls)
        self.assertIsNone(self.media.fd)
    async def test_pointer_pixels_map_to_logical_portal_coordinates(self):
        await self.session.authorize()
        frame = await self.session.frame()
        await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "click", "x": 1920, "y": 1080}})
        self.assertIn(("NotifyPointerMotionAbsolute", 17, 960, 540), self.portal.calls)
        self.assertIn(("NotifyPointerButton", 272, 0), self.portal.calls)
    async def test_geometry_stale_and_boundary_input_rejected(self):
        await self.session.authorize()
        frame = await self.session.frame()
        with self.assertRaisesRegex(RuntimeError, "geometry changed"):
            await self.session.action({"geometry_version": 0, "action": {"type": "click", "x": 0, "y": 0}})
        with self.assertRaisesRegex(RuntimeError, "outside"):
            await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "click", "x": 3840, "y": 0}})
        self.session.last_frame -= 6
        with self.assertRaisesRegex(RuntimeError, "stale"):
            await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "key", "key": "enter"}})
    async def test_lock_revokes_and_clears_saved_permission(self):
        await self.session.authorize()
        frame = await self.session.frame()
        self.portal.is_unlocked = False
        with self.assertRaisesRegex(RuntimeError, "locked"):
            await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "key", "key": "enter"}})
        self.assertFalse((await self.session.status())["available"])
        self.assertFalse(self.tokens.path.exists())
        self.assertIsNone(self.media.fd)
    async def test_session_signal_closes_capture_and_invalidates_geometry(self):
        await self.session.authorize()
        await self.session.frame()
        await self.portal.on_invalidated("Permission revoked")
        self.assertIsNone(self.session.geometry)
        self.assertIsNone(self.media.fd)
        self.assertEqual((await self.session.status())["reason"], "Permission revoked")
    async def test_clean_shutdown_preserves_single_use_restore_token(self):
        await self.session.authorize()
        await self.session.invalidate("shutdown", forget=False)
        self.assertEqual(self.tokens.consume(), "rotated-token")
        self.assertIsNone(self.tokens.consume())
    async def test_unicode_text_and_modifier_release(self):
        await self.session.authorize()
        frame = await self.session.frame()
        await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "text", "text": "Aλ"}})
        self.assertIn(("NotifyKeyboardKeysym", 0x010003bb, 1), self.portal.calls)
        await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "key", "key": "c", "modifiers": ["control"]}})
        self.assertEqual(self.portal.calls[-1], ("NotifyKeyboardKeysym", 0xffe3, 0))
    async def test_disconnected_proxy_cancels_pending_authority(self):
        await self.session.authorize()
        frame = await self.session.frame()
        blocked = asyncio.Event()
        async def slow_notify(*args):
            blocked.set()
            await asyncio.sleep(30)
        self.portal.notify = slow_notify
        reader = asyncio.StreamReader()
        operation = asyncio.create_task(cancellable_request(self.session, {
            "command": "action", "geometry_version": frame["geometry_version"],
            "action": {"type": "click", "x": 1, "y": 1}}, reader))
        await blocked.wait()
        reader.feed_eof()
        with self.assertRaisesRegex(RuntimeError, "cancelled"):
            await asyncio.wait_for(operation, 1)
        self.assertIsNone(self.session.session)
        self.assertIsNone(self.media.fd)
    async def test_monitor_resize_invalidates_existing_coordinates(self):
        await self.session.authorize()
        frame = await self.session.frame()
        async def resized(): return b"\xff\xd8jpeg", 1920, 1080, time.monotonic()
        self.media.snapshot = resized
        with self.assertRaisesRegex(RuntimeError, "geometry changed"):
            await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "click", "x": 1, "y": 1}})
        self.assertIsNone(self.session.session)
    async def test_portal_rejected_input_revokes_authority(self):
        await self.session.authorize()
        frame = await self.session.frame()
        async def rejected(*args): raise RuntimeError("permission withdrawn")
        self.portal.notify = rejected
        with self.assertRaisesRegex(RuntimeError, "permission withdrawn"):
            await self.session.action({"geometry_version": frame["geometry_version"], "action": {"type": "key", "key": "enter"}})
        self.assertIsNone(self.session.session)


class Framing(unittest.TestCase):
    def test_cpv1_packet_lengths_and_payload(self):
        header = {"version": 1, "codec": "h264", "keyframe": True}
        payload = b"\x00\x00\x00\x02\x65\x01"
        encoded = packet(header, payload)
        self.assertEqual(encoded[:4], b"CPV1")
        json_size, payload_size = struct.unpack(">II", encoded[4:12])
        self.assertEqual(json.loads(encoded[12:12 + json_size]), header)
        self.assertEqual(payload_size, len(payload))
        self.assertEqual(encoded[12 + json_size:], payload)
    def test_oversized_payload_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "bounds"): packet({}, b"x" * (8 * 1024 * 1024 + 1))
    def test_avc_parameter_sets_and_truncation(self):
        config = b"\x01\x42\x00\x1e\xff\xe1\x00\x02\x67\x01\x01\x00\x02\x68\x02"
        self.assertEqual(avc_parameter_sets(config), (b"\x67\x01", b"\x68\x02"))
        for end in range(len(config)):
            with self.assertRaises(RuntimeError): avc_parameter_sets(config[:end])
    def test_socket_line_bounds_and_truncation(self):
        self.assertEqual(read_line(io.BytesIO(b'{}\n'), 3), b'{}\n')
        for data in (b'{}', b'xxxx\n', b''):
            with self.assertRaises(RuntimeError): read_line(io.BytesIO(data), 3)
    def test_token_file_rejects_exposed_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "token"
            path.write_text("secret")
            os.chmod(path, 0o644)
            with self.assertRaisesRegex(RuntimeError, "owner-only"): RestoreToken(path).consume()


class PortalWire(unittest.IsolatedAsyncioTestCase):
    async def test_dbus_input_method_signatures(self):
        portal = DBusPortal()
        portal.Variant = lambda signature, value: (signature, value)
        calls = []
        async def call(*args): calls.append(args)
        portal.call = call
        await portal.notify("/session/test", "NotifyPointerMotionAbsolute", 17, 10.0, 20.0)
        self.assertEqual(calls[-1][4:], ("oa{sv}udd", ["/session/test", {}, 17, 10.0, 20.0]))
        await portal.notify("/session/test", "NotifyKeyboardKeysym", 65, 1)
        self.assertEqual(calls[-1][4], "oa{sv}iu")
        await portal.notify("/session/test", "NotifyPointerAxis", 0.0, 100.0)
        self.assertEqual(calls[-1][5][1], {"finish": ("b", True)})
    async def test_variants_preserve_boolean_vs_unsigned_types(self):
        portal = DBusPortal()
        portal.Variant = lambda signature, value: (signature, value)
        self.assertEqual(portal.variants({"types": 3, "multiple": False, "restore_token": "t"}),
                         {"types": ("u", 3), "multiple": ("b", False), "restore_token": ("s", "t")})


if __name__ == "__main__": unittest.main()
