"""PipeWire-backed bounded JPEG/H.264 appsinks and CPV1 packet framing."""
import asyncio
import base64
import json
import os
import struct
import threading
import time

MAX_PACKET = 24 * 1024 * 1024


def avc_parameter_sets(data):
    if len(data) < 7 or data[0] != 1:
        raise RuntimeError("Invalid AVC decoder configuration")
    if data[4] & 3 != 3:
        raise RuntimeError("Video transport requires four-byte AVCC NAL lengths")
    position = 6
    sps = []
    for _ in range(data[5] & 31):
        if position + 2 > len(data):
            raise RuntimeError("Truncated AVC SPS")
        size = int.from_bytes(data[position:position + 2], "big")
        position += 2
        if not size or position + size > len(data):
            raise RuntimeError("Invalid AVC SPS length")
        sps.append(data[position:position + size])
        position += size
    if position >= len(data):
        raise RuntimeError("Missing AVC PPS")
    count = data[position]
    position += 1
    pps = []
    for _ in range(count):
        if position + 2 > len(data):
            raise RuntimeError("Truncated AVC PPS")
        size = int.from_bytes(data[position:position + 2], "big")
        position += 2
        if not size or position + size > len(data):
            raise RuntimeError("Invalid AVC PPS length")
        pps.append(data[position:position + size])
        position += size
    if not sps or not pps:
        raise RuntimeError("AVC SPS/PPS required")
    return sps[0], pps[0]


def packet(header, payload):
    encoded = json.dumps(header, separators=(",", ":"), allow_nan=False).encode()
    if not 0 < len(encoded) <= 65536 or not 0 < len(payload) <= 8 * 1024 * 1024:
        raise RuntimeError("Video packet exceeds bounds")
    return b"CPV1" + struct.pack(">II", len(encoded), len(payload)) + encoded + payload


class GstMedia:
    def __init__(self):
        self.pipeline = None
        self.fd = None
        self.error = None
        self.jpeg = None
        self.h264 = None
        self.video_available = False
        self.mutex = threading.Lock()

    async def start(self, fd, node, serial=None):
        self.fd = fd
        import gi
        gi.require_version("Gst", "1.0")
        from gi.repository import Gst
        self.Gst = Gst
        Gst.init(None)
        self.error = None
        self.jpeg, self.h264 = None, None
        self.video_available = all(Gst.ElementFactory.find(name) for name in ("x264enc", "h264parse"))
        pipeline = "pipewiresrc name=capture do-timestamp=true ! videoconvert ! videorate drop-only=true ! video/x-raw,framerate=30/1 ! tee name=t "
        pipeline += "t. ! queue max-size-buffers=1 leaky=downstream ! videoconvert ! jpegenc quality=70 ! appsink name=jpeg emit-signals=true max-buffers=1 drop=true sync=false "
        if self.video_available:
            pipeline += "t. ! queue max-size-buffers=1 leaky=downstream ! videoconvert ! videoscale ! video/x-raw,format=I420,width=[2,4096],height=[2,4096] ! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=1 bitrate=4000 byte-stream=false ! h264parse config-interval=-1 ! video/x-h264,stream-format=avc,alignment=au ! appsink name=h264 emit-signals=true max-buffers=1 drop=true sync=false"
        self.pipeline = Gst.parse_launch(pipeline)
        source = self.pipeline.get_by_name("capture")
        source.set_property("fd", fd)
        if source.find_property("keepalive-time"):
            source.set_property("keepalive-time", 500)
        if source.find_property("on-disconnect"):
            source.set_property("on-disconnect", 2)  # Error, never replay after remote loss.
        if source.find_property("dont-reconnect"):
            source.set_property("dont-reconnect", True)
        if serial is not None and source.find_property("target-object"):
            source.set_property("target-object", str(serial))
        else:
            source.set_property("path", str(node))
        self.pipeline.get_by_name("jpeg").connect("new-sample", self.sample, "jpeg")
        if self.video_available:
            self.pipeline.get_by_name("h264").connect("new-sample", self.sample, "h264")
        result = self.pipeline.set_state(Gst.State.PLAYING)
        if result == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError("PipeWire capture pipeline failed to start")
        # First actual frame, rather than a successful state request, proves capture.
        deadline = time.monotonic() + 8
        while self.jpeg is None:
            self.check_bus()
            if self.error:
                raise RuntimeError(self.error)
            if time.monotonic() >= deadline:
                raise RuntimeError("PipeWire did not deliver a frame")
            await asyncio.sleep(0.025)

    def check_bus(self):
        if not self.pipeline:
            return
        message = self.pipeline.get_bus().pop_filtered(self.Gst.MessageType.ERROR | self.Gst.MessageType.EOS)
        if message:
            self.error = str(message.parse_error()[0]) if message.type == self.Gst.MessageType.ERROR else "PipeWire stream ended"

    def sample(self, sink, kind):
        try:
            sample = sink.emit("pull-sample")
            if sample is None:
                return self.Gst.FlowReturn.ERROR
            buffer = sample.get_buffer()
            if buffer.get_size() > MAX_PACKET:
                raise RuntimeError("Encoded frame exceeds size limit")
            success, mapped = buffer.map(self.Gst.MapFlags.READ)
            if not success:
                raise RuntimeError("Cannot map capture buffer")
            try:
                data = bytes(mapped.data)
            finally:
                buffer.unmap(mapped)
            structure = sample.get_caps().get_structure(0)
            width, height = int(structure.get_value("width")), int(structure.get_value("height"))
            timestamp = time.monotonic()
            if kind == "jpeg":
                value = (data, width, height, timestamp)
            else:
                configuration = structure.get_value("codec_data")
                if configuration is None:
                    raise RuntimeError("H264 caps missing codec_data")
                config = configuration.extract_dup(0, configuration.get_size())
                sps, pps = avc_parameter_sets(config)
                keyframe = not buffer.has_flags(self.Gst.BufferFlags.DELTA_UNIT)
                if not keyframe:
                    return self.Gst.FlowReturn.OK  # Latest-frame transport sends IDRs only.
                value = (data, width, height, timestamp, sps, pps)
            with self.mutex:
                if kind == "jpeg":
                    self.jpeg = value
                else:
                    self.h264 = value
            return self.Gst.FlowReturn.OK
        except Exception as error:
            self.error = str(error)
            return self.Gst.FlowReturn.ERROR

    async def snapshot(self):
        self.check_bus()
        with self.mutex:
            value = self.jpeg
        if self.error or value is None:
            raise RuntimeError(self.error or "No PipeWire frame available")
        return value

    async def video_frame(self):
        deadline = time.monotonic() + 2
        while True:
            self.check_bus()
            with self.mutex:
                value = self.h264
            if self.error or not self.video_available:
                raise RuntimeError(self.error or "H264 stream unavailable (install x264 and h264parse plugins)")
            if value is not None:
                return value
            if time.monotonic() >= deadline:
                raise RuntimeError("H264 encoder did not produce a frame")
            await asyncio.sleep(0.01)

    async def stop(self):
        if self.pipeline:
            self.pipeline.set_state(self.Gst.State.NULL)
            self.pipeline = None
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        with self.mutex:
            self.jpeg, self.h264 = None, None
        self.video_available = False
