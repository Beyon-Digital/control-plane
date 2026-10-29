#!/usr/bin/env python3
"""Generate a real H264 test fixture from synthetic pixels; never captures a screen."""
from pathlib import Path
import subprocess
import struct
import json

root=Path(__file__).resolve().parent
helper=root.parent / "helpers/macos/build/control-plane-desktop"
result=subprocess.run([str(helper),"--video-self-test"],capture_output=True,check=True,timeout=15)
packet=result.stdout
assert packet[:4]==b"CPV1"
h,p=struct.unpack(">II",packet[4:12])
assert len(packet)==12+h+p
header=json.loads(packet[12:12+h])
assert header["width"]==64 and header["height"]==64 and header["keyframe"]
path=root / "Tests/ControlPlaneKitTests/Fixtures/synthetic-h264.cpv"
path.parent.mkdir(parents=True,exist_ok=True)
path.write_bytes(packet)
print(f"Generated {len(packet)}-byte synthetic VideoToolbox fixture")
