#!/usr/bin/env python3
"""Windows user-session gdigrab/libx264 video -> bounded CPV1 packets.

Requires Python 3.10+ and FFmpeg with gdigrab and libx264. Never runs a shell.
"""
import base64
import ctypes
import json
import os
import shutil
import struct
import subprocess
import sys
import time
from pathlib import Path
from displays import selected_display, geometry_version

MAX_NAL=8*1024*1024

class AnnexBParser:
    """Bounded streaming parser; AUD delimits independently coded access units."""
    def __init__(self,width,height,desktop_width,desktop_height,geometry):
        self.buffer=bytearray();self.units=[];self.total=0;self.sps=None;self.pps=None
        self.header=dict(version=1,codec="h264",width=width,height=height,desktop_width=desktop_width,desktop_height=desktop_height,geometry_version=geometry,keyframe=True)
    @staticmethod
    def start(data,offset=0):
        for i in range(offset,max(offset,len(data)-2)):
            if data[i:i+3]==b"\0\0\1":return i,3
            if data[i:i+4]==b"\0\0\0\1":return i,4
        return None
    def feed(self,data,final=False):
        self.buffer.extend(data)
        if len(self.buffer)>MAX_NAL+65536:raise ValueError("H264 NAL exceeds limit")
        packets=[]
        while True:
            first=self.start(self.buffer)
            if not first:
                if final and self.buffer:raise ValueError("Invalid AnnexB stream")
                if len(self.buffer)>4:raise ValueError("Missing AnnexB start code")
                break
            offset,prefix=first
            following=self.start(self.buffer,offset+prefix)
            if not following:
                if final:
                    unit=bytes(self.buffer[offset+prefix:]).rstrip(b"\0");self.buffer.clear()
                    if unit:packets+=self.nal(unit)
                break
            unit=bytes(self.buffer[offset+prefix:following[0]]).rstrip(b"\0")
            del self.buffer[:following[0]]
            if unit:packets+=self.nal(unit)
        if final:
            packet=self.flush()
            if packet:packets.append(packet)
        return packets
    def nal(self,unit):
        kind=unit[0]&31;packets=[]
        if kind==9:
            packet=self.flush()
            if packet:packets.append(packet)
        if kind==7:self.sps=unit
        if kind==8:self.pps=unit
        self.units.append(unit);self.total+=4+len(unit)
        if self.total>MAX_NAL:raise ValueError("H264 access unit exceeds limit")
        return packets
    def flush(self):
        units=self.units;self.units=[];self.total=0
        if not units or not any(unit[0]&31==5 for unit in units):return None
        if not self.sps or not self.pps:raise ValueError("IDR frame has no parameter sets")
        if any(unit[0]&31 in (1,2,3,4) for unit in units):raise ValueError("Only independently coded IDR frames are supported")
        header=dict(self.header,timestamp_us=time.monotonic_ns()//1000,sps_base64=base64.b64encode(self.sps).decode(),pps_base64=base64.b64encode(self.pps).decode())
        metadata=json.dumps(header,separators=(",",":")).encode()
        payload=b"".join(struct.pack(">I",len(unit))+unit for unit in units)
        if len(metadata)>65536 or len(payload)>MAX_NAL:raise ValueError("Video packet exceeds limit")
        return b"CPV1"+struct.pack(">II",len(metadata),len(payload))+metadata+payload

def interactive_geometry():
    if sys.platform!="win32":raise RuntimeError("Windows video requires an interactive Windows user session")
    from ctypes import wintypes
    user=ctypes.WinDLL("user32",use_last_error=True)
    user.OpenInputDesktop.restype=wintypes.HANDLE
    user.OpenInputDesktop.argtypes=[wintypes.DWORD,wintypes.BOOL,wintypes.DWORD]
    user.CloseDesktop.argtypes=[wintypes.HANDLE]
    user.GetUserObjectInformationW.argtypes=[wintypes.HANDLE,ctypes.c_int,ctypes.c_void_p,wintypes.DWORD,ctypes.POINTER(wintypes.DWORD)]
    user.SetThreadDpiAwarenessContext.restype=wintypes.HANDLE
    user.SetThreadDpiAwarenessContext.argtypes=[wintypes.HANDLE]
    user.SetThreadDpiAwarenessContext(ctypes.c_void_p(-4))
    desktop=user.OpenInputDesktop(0,False,1)
    if not desktop:raise RuntimeError("Desktop is locked or unavailable")
    try:
        name=ctypes.create_unicode_buffer(256);needed=wintypes.DWORD()
        if not user.GetUserObjectInformationW(desktop,2,name,ctypes.sizeof(name),ctypes.byref(needed)) or name.value!="Default":raise RuntimeError("Secure or locked desktop cannot be captured")
    finally:user.CloseDesktop(desktop)
    width,height=user.GetSystemMetrics(0),user.GetSystemMetrics(1)
    if not (0<width<=32768 and 0<height<=32768):raise RuntimeError("Invalid desktop geometry")
    return width,height

def ffmpeg_binary():
    binary=os.environ.get("CONTROL_PLANE_FFMPEG") or (str(Path(sys.executable).with_name("ffmpeg.exe")) if getattr(sys,"frozen",False) and Path(sys.executable).with_name("ffmpeg.exe").is_file() else shutil.which("ffmpeg"))
    if not binary:raise RuntimeError("Install FFmpeg with gdigrab and libx264, or set CONTROL_PLANE_FFMPEG")
    for flag,capability in [("-devices","gdigrab"),("-encoders","libx264")]:
        result=subprocess.run([binary,"-hide_banner",flag],capture_output=True,timeout=3,check=True)
        if capability not in (result.stdout+result.stderr).decode(errors="replace"):raise RuntimeError("FFmpeg lacks "+capability)
    return binary

def kill_job(child):
    """Kernel-enforced descendant cleanup even if the host kills this helper."""
    from ctypes import wintypes
    kernel=ctypes.WinDLL("kernel32",use_last_error=True)
    class Basic(ctypes.Structure):
        _fields_=[("ProcessTime",ctypes.c_int64),("JobTime",ctypes.c_int64),("Flags",wintypes.DWORD),("Minimum",ctypes.c_size_t),("Maximum",ctypes.c_size_t),("Active",wintypes.DWORD),("Affinity",ctypes.c_size_t),("Priority",wintypes.DWORD),("Scheduling",wintypes.DWORD)]
    class IO(ctypes.Structure):_fields_=[("values",ctypes.c_uint64*6)]
    class Extended(ctypes.Structure):_fields_=[("Basic",Basic),("IO",IO),("ProcessMemory",ctypes.c_size_t),("JobMemory",ctypes.c_size_t),("PeakProcess",ctypes.c_size_t),("PeakJob",ctypes.c_size_t)]
    kernel.CreateJobObjectW.restype=wintypes.HANDLE;kernel.CreateJobObjectW.argtypes=[ctypes.c_void_p,wintypes.LPCWSTR]
    kernel.SetInformationJobObject.argtypes=[wintypes.HANDLE,ctypes.c_int,ctypes.c_void_p,wintypes.DWORD]
    kernel.AssignProcessToJobObject.argtypes=[wintypes.HANDLE,wintypes.HANDLE]
    kernel.CloseHandle.argtypes=[wintypes.HANDLE]
    job=kernel.CreateJobObjectW(None,None)
    limits=Extended();limits.Basic.Flags=0x2000
    if not job or not kernel.SetInformationJobObject(job,9,ctypes.byref(limits),ctypes.sizeof(limits)) or not kernel.AssignProcessToJobObject(job,wintypes.HANDLE(child._handle)):
        if job:kernel.CloseHandle(job)
        child.terminate();child.wait(timeout=3);raise RuntimeError("Cannot contain FFmpeg in a kill-on-close Windows job")
    return kernel,job

def main():
    if "--status" in sys.argv:
        try:
            interactive_geometry();ffmpeg_binary();result=dict(available=True,video_streaming=True,reason="FFmpeg gdigrab/libx264 available")
        except Exception as error:result=dict(available=False,video_streaming=False,reason=str(error))
        print(json.dumps(result));return
    if "--video" not in sys.argv:raise RuntimeError("Use --status or --video")
    interactive_geometry();binary=ffmpeg_binary()
    display_id=None
    if '--display-id' in sys.argv:
        index=sys.argv.index('--display-id')
        if index+1>=len(sys.argv):raise RuntimeError('Display ID required')
        display_id=sys.argv[index+1]
    selected=selected_display(display_id);width,height=selected['width'],selected['height']
    scale=min(1,1920/width,1080/height);w=max(2,int(width*scale)//2*2);h=max(2,int(height*scale)//2*2)
    args=[binary,"-hide_banner","-loglevel","error","-f","gdigrab","-framerate","30","-draw_mouse","1","-offset_x",str(selected["x"]),"-offset_y",str(selected["y"]),"-video_size",f"{width}x{height}","-i","desktop","-vf",f"scale={w}:{h}","-c:v","libx264","-preset","ultrafast","-tune","zerolatency","-pix_fmt","yuv420p","-b:v","8M","-g","1","-keyint_min","1","-x264-params","repeat-headers=1:aud=1","-f","h264","pipe:1"]
    child=subprocess.Popen(args,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,bufsize=0)
    kernel,job=kill_job(child)
    parser=AnnexBParser(w,h,width,height,geometry_version(selected))
    parser.header["display_id"]=selected["id"]
    try:
        while True:
            chunk=child.stdout.read(65536)
            if not chunk:raise RuntimeError("FFmpeg capture ended")
            interactive_geometry()
            if selected_display(selected["id"])!=selected:raise RuntimeError("Desktop geometry changed; reconnect video")
            for packet in parser.feed(chunk):sys.stdout.buffer.write(packet);sys.stdout.buffer.flush()
    finally:
        kernel.CloseHandle(job)
        if child.poll() is None:child.terminate()
        child.wait(timeout=3)

if __name__=="__main__":
    try:main()
    except Exception as error:print(str(error),file=sys.stderr);sys.exit(1)
