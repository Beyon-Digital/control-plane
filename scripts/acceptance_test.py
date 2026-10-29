"""Checks that CI observation gates reject invalid evidence and wrong platforms."""
import argparse
import asyncio
import importlib.util
import json
from pathlib import Path
import struct
import sys
import unittest

def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

desktop, wsl = load("desktop-acceptance"), load("wsl-live-smoke")

class AcceptanceEvidence(unittest.TestCase):
    def test_existing_encoder_fixture_contains_real_idr(self):
        packet = (Path(__file__).resolve().parents[1] / "apple/Tests/ControlPlaneKitTests/Fixtures/synthetic-h264.cpv").read_bytes()
        self.assertEqual(packet[:4], b"CPV1")
        header, size = struct.unpack(">II", packet[4:12])
        metadata = json.loads(packet[12:12 + header])
        self.assertEqual(metadata["codec"], "h264")
        encoded = desktop.annex_b(packet[12 + header:])
        self.assertGreater(size, 0)
        self.assertTrue(encoded.startswith(b"\0\0\0\1"))

    def test_non_idr_truncated_and_forged_nals_fail(self):
        for payload in (b"\0\0\0", b"\0\0\0\5\x65", b"\0\0\0\1\x41", b"\0\0\0\1\xe5"):
            with self.assertRaises(desktop.GateFailure):
                desktop.annex_b(payload)

    def test_password_values_and_unbounded_trees_fail(self):
        for child in ({"password": True, "value": "sentinel"}, {"children": [{}] * 41}):
            with self.assertRaises(desktop.GateFailure):
                desktop.inspect_tree({"role": "window", "children": [child]})
        self.assertEqual(desktop.inspect_tree({"role": "window", "children": [{"password": True, "name": "Password", "value": "[redacted]"}]}), (2, 1))

    def test_wrong_os_cannot_count_as_platform_acceptance(self):
        selected = "windows" if sys.platform != "win32" else "gnome"
        with self.assertRaises(desktop.GateFailure):
            asyncio.run(desktop.acceptance(argparse.Namespace(platform=selected), {}))

    def test_wsl_utf16_names_spaces_and_exact_version(self):
        raw = "  NAME       STATE     VERSION\r\n* Ubuntu CI  Running   2\r\n  Ubuntu     Stopped   1\r\n".encode("utf-16")
        text = wsl.listing(raw)
        self.assertTrue(wsl.wsl2_installed(text, "Ubuntu CI"))
        self.assertFalse(wsl.wsl2_installed(text, "Ubuntu"))
        self.assertFalse(wsl.wsl2_installed(text, "Ubuntu C"))
        self.assertFalse(wsl.wsl2_installed(text + "Ubuntu CI Running 2\n", "Ubuntu CI"))

if __name__ == "__main__":
    unittest.main()
