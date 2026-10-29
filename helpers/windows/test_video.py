import base64
import importlib.util
import json
from pathlib import Path
import struct
import unittest

HERE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location("video",HERE/"VideoHelper.py")
video=importlib.util.module_from_spec(spec);spec.loader.exec_module(video)

class VideoFramingTests(unittest.TestCase):
    def fixture(self):
        raw=(HERE.parent.parent/"apple/Tests/ControlPlaneKitTests/Fixtures/synthetic-h264.cpv").read_bytes()
        h,p=struct.unpack(">II",raw[4:12]);header=json.loads(raw[12:12+h]);payload=raw[12+h:]
        units=[b"\x09\xf0",base64.b64decode(header["sps_base64"]),base64.b64decode(header["pps_base64"])]
        while payload:
            n=struct.unpack(">I",payload[:4])[0];units.append(payload[4:4+n]);payload=payload[4+n:]
        return header,b"".join(b"\0\0\0\1"+unit for unit in units)
    def test_real_videotoolbox_fixture_converts_with_bytewise_chunking(self):
        header,annexb=self.fixture();parser=video.AnnexBParser(64,64,64,64,1);packets=[]
        for byte in annexb+annexb:packets+=parser.feed(bytes([byte]))
        packets+=parser.feed(b"",final=True)
        self.assertEqual(len(packets),2)
        for packet in packets:
            self.assertEqual(packet[:4],b"CPV1");h,p=struct.unpack(">II",packet[4:12]);self.assertEqual(len(packet),12+h+p)
            parsed=json.loads(packet[12:12+h]);self.assertEqual(parsed["sps_base64"],header["sps_base64"]);self.assertTrue(parsed["keyframe"])
    def test_invalid_and_oversized_nal_are_rejected(self):
        parser=video.AnnexBParser(64,64,64,64,1)
        with self.assertRaises(ValueError):parser.feed(b"garbage",final=True)
        parser=video.AnnexBParser(64,64,64,64,1)
        with self.assertRaises(ValueError):parser.feed(b"\0\0\1\x65"+b"x"*(video.MAX_NAL+65536))
    def test_non_idr_and_missing_parameter_sets_are_not_emitted(self):
        parser=video.AnnexBParser(64,64,64,64,1)
        self.assertEqual(parser.feed(b"\0\0\1\x41\x80",final=True),[])
        with self.assertRaises(ValueError):video.AnnexBParser(64,64,64,64,1).feed(b"\0\0\1\x65\x80",final=True)

if __name__=="__main__":unittest.main()
