# Desktop video

The native clients offer **Live video** alongside manual screenshot capture. The live path streams H.264 access units over the authenticated `GET /v1/desktop/video` WebSocket endpoint. Broker account base paths are preserved by the client, so `/accounts/<id>/v1/desktop/video` uses the selected OS account's worker.

## Capture and decoding

The signed macOS helper runs `--video`, starts a persistent ScreenCaptureKit stream for the selected display in the logged-in graphical session, and uses VideoToolbox for H.264 encoding. It requests 30 fps, caps encoded output at 1920×1080, preserves aspect ratio, limits capture queue depth to three, and allows one outstanding encode. Screen Recording consent, an unlocked desktop, and a valid display are required. Changes to display geometry close the stream; reconnect builds a new encoder configuration. Idle capture callbacks confirm unchanged image content and may re-encode the last frame. Revoked recording access and capture errors end the stream.

Each frame is an IDR access unit with its SPS and PPS, allowing the host and clients to drop obsolete frames safely. This favors predictable recovery over inter-frame compression efficiency. The bitrate property is a target, not a bandwidth guarantee. The host allows two video viewers per worker. Its newest-only watch channel retains one pending frame, and a WebSocket send deadline disconnects persistently slow clients. The Apple client also uses a newest-only queue before its serial VideoToolbox decoder and retains one displayed image.

The native client scales pointer coordinates from encoded image space into the desktop's input coordinate space. Successfully forwarded frames update host geometry and freshness checks; a stale or changed geometry is still rejected by the input service and platform helper. Input ownership uses the same short-lived lease as snapshots and agent tools. The live panel renews a human control lease every five seconds while active; an explicit takeover can supersede an agent controller.

Linux Wayland video uses the persistent portal helper when its GStreamer H.264 plugins are available. It shares the authorized PipeWire session with snapshot capture. Check the helper's `video_streaming` capability and Linux setup documentation. Clients show snapshot fallback when a backend cannot stream.

Windows video is opt-in: install Python 3.10+ and FFmpeg built with `gdigrab` and `libx264`, then set `CONTROL_PLANE_VIDEO_HELPER` to the absolute path of `helpers/windows/VideoHelper.py`. Keep `CONTROL_PLANE_DESKTOP_HELPER` pointed at `DesktopHelper.ps1` for snapshots and input. `CONTROL_PLANE_FFMPEG` can select an explicit FFmpeg binary; otherwise the helper checks its packaged runtime directory and PATH. The helper validates the required input device and encoder before capture and reports capability through `--status`. It selects the requested stable monitor DeviceName, captures at that monitor's desktop offset, scales to at most 1920×1080, and converts AnnexB output to CPV1 without third-party Python libraries. The helper rejects locked/secure desktops and geometry changes. FFmpeg runs in a kill-on-close Windows Job Object, so forcefully ending the helper also terminates its capture subprocess. Capture uses FFmpeg's GDI backend; this implementation does not claim DXGI hardware capture performance. Windows runtime acceptance remains required.

Display changes clear the input lease and previous frame geometry and end the active producer. Input coordinates are local to the selected display; platform helpers translate them into desktop coordinates. A missing selected display fails without switching to another monitor. Wayland selections must belong to the existing local portal grant. The X11 fallback rejects negative-origin capture layouts.

Accessibility is available separately through macOS Accessibility, Windows UI Automation, or Linux AT-SPI when locally permitted. Trees are bounded and secure/password controls are redacted. Successful video capture does not establish accessibility or input permission. Real Windows and GNOME multi-monitor capture, accessibility, video, and input acceptance remain unrun.

## CPV1 framing

Every WebSocket binary message contains exactly one complete packet. Helper stdout concatenates these packets without delimiters:

| Bytes | Meaning |
| --- | --- |
| 0–3 | ASCII `CPV1` |
| 4–7 | JSON header byte length, unsigned big endian |
| 8–11 | H.264 payload byte length, unsigned big endian |
| following header length | UTF-8 JSON header |
| remaining payload length | AVCC H.264 access unit, four-byte big-endian NAL lengths |

The JSON header contains `version:1`, `codec:"h264"`, encoded `width` and `height`, input `desktop_width` and `desktop_height`, selected `display_id`, `geometry_version`, monotonic `timestamp_us`, `keyframe:true`, `sps_base64`, and `pps_base64`. Parameter sets contain their NAL headers but no AVCC length prefix. Header size is bounded at 64 KiB, payload at 8 MiB, encoded dimensions at 4096, and desktop input dimensions at 32768. Future incompatible framing requires another magic/version. Unsupported, oversized, truncated, and non-IDR packets are rejected by the native decoder.

Server-side failures use a JSON text frame `{ "error": "…", "fallback": "snapshots" }` followed by connection close. The Apple client displays the reason and reconnects with exponential backoff; users can stop retrying and use snapshots explicitly. A capture helper that produces no complete frame for five seconds is disconnected. The host checks device/member revocation and the bound client certificate every 200 ms when the socket is writable, with a two-second send deadline bounding slow connections, and observes desktop emergency-stop cancellation immediately. Closing a viewer kills and reaps its capture subprocess. The persistent JSONL desktop command helper is separately killed by emergency stop, including when idle. Human input takeover cancels the previous input command/lease without stopping independent video viewers; emergency stop cancels both.

## Verification

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer sh helpers/macos/build.sh
python3 apple/generate_video_fixture.py
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift test --package-path apple
cargo test -p control-plane-host video::tests
cargo test -p control-plane-host desktop::tests
python3 -m unittest discover -s helpers/windows -p 'test_*.py'
```

The fixture generator uses synthetic pixels only, performs a real VideoToolbox encode, and writes a small CPV1 packet used by the Swift decoder test. No screen content is read. Tests cover packet limits/truncation, native H.264 decode, desktop cancellation/lease/geometry checks, and transport behavior. Builds verify both Mac and iOS Simulator targets. Real Screen Recording consent, network latency, hardware throughput, and Linux/Windows behavior still require target-machine acceptance tests; build success does not establish the latency target.
