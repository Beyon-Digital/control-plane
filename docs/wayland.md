# GNOME Wayland desktop backend

`helpers/linux/wayland_helper.py` implements a persistent user-session XDG RemoteDesktop session combined with ScreenCast capture. It obtains the PipeWire remote descriptor through the portal, captures JPEG frames with GStreamer, injects pointer and keyboard events through RemoteDesktop Notify methods, and optionally produces H.264 CPV1 video.

This source implementation has **not been exercised against a real Linux compositor in this workspace**. The Python lifecycle, permission policy, input mapping, token storage, cancellation, and video framing tests run without Linux dependencies. Real GNOME/portal/PipeWire acceptance testing remains a release gate.

## Install on Ubuntu GNOME

Run from a local logged-in GNOME Wayland terminal:

```sh
sudo apt-get install python3-venv python3-gi gir1.2-atspi-2.0 gir1.2-gstreamer-1.0 gir1.2-gst-plugins-base-1.0 \
  gstreamer1.0-pipewire gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly xdg-desktop-portal xdg-desktop-portal-gnome
sh helpers/linux/install-wayland.sh
~/.local/share/control-plane/venv/bin/python ~/.local/share/control-plane/linux/wayland_helper.py --authorize
```

The installer creates a user-owned virtual environment with access to distro PyGObject, installs pinned `dbus-next`, and enables a graphical-session user service. It does not install system packages or run as root. The local `--authorize` command presents the portal's monitor/input consent dialog. Grant between one and eight physical monitors and both keyboard and pointer access. Remote display selection is limited to those locally granted streams; display IDs remain valid only within that portal session.

Configure the host environment before starting it:

```sh
export CONTROL_PLANE_DESKTOP_HELPER="$HOME/.local/share/control-plane/linux/wayland_helper.py"
```

The Python executable used by the host must have the helper dependencies available. The host's `python3` can execute the lightweight proxy without dbus-next/GI; only the systemd daemon uses the virtual environment and native bindings.

Inspect service health with `systemctl --user status control-plane-wayland` and `journalctl --user -u control-plane-wayland`. Start it only within a graphical GNOME login. SSH-only/headless sessions and non-GNOME lock services fail closed.

## Session and security behavior

- Status never opens a permission dialog or starts a portal session. Authorization is an explicit local CLI action.
- CreateSession, RemoteDesktop.SelectDevices, ScreenCast.SelectSources, and RemoteDesktop.Start run once per authorization. The portal session persists across host snapshot/input requests.
- Combined-session persistence is requested only through SelectDevices. A restore token is consumed once and atomically replaced by the next Start token when supported by RemoteDesktop interface version 2+. The token file is owner-only and is never logged or returned through RPC.
- Clean service shutdown preserves the current restore token. Permission revocation, lock, explicit stop, failed capture, or disconnected in-flight input cancels the session and clears saved authority. Run `--authorize` locally again after unlocking.
- Portal Session.Closed, portal owner changes, GNOME lock signals, and a periodic lock check invalidate capture/input. Input checks the live lock state and session generation as it proceeds.
- Unix IPC lives in `$XDG_RUNTIME_DIR/control-plane/wayland.sock`, in a mode-0700 directory, with a mode-0600 socket and Linux SO_PEERCRED checks. At most eight RPC clients and one video producer may attach. Requests are bounded to 64 KiB; frame responses to 34 MiB.
- Capture coordinates refer to the encoded JPEG pixels; the helper maps these to the selected monitor's logical portal coordinate space. Geometry and a five-second screenshot freshness window are checked before input.
- Changing displays validates the stream ID, replaces the PipeWire capture pipeline, advances the session epoch, and clears previous geometry. A missing or ungranted display fails without falling back to another monitor.
- The backend uses documented D-Bus Notify methods and never opens an EIS connection. It does not bypass compositor permissions or implement clipboard access.
- AT-SPI inspection requires desktop accessibility already enabled locally and an unlocked session. It reads a foreground window owned by the current OS user, bounded to 200 nodes, six levels, 40 children per node, and a six-second subprocess deadline. Password controls are redacted; text/value interfaces are not queried. The foreground window must intersect the selected display. Returned bounds include explicit global logical coordinates and capture dimensions. Unavailable permissions or a suitable window produce an error.

## Helper interfaces

- `--daemon`: persistent session-bus and PipeWire service.
- `--authorize`: one local consent request through that service.
- `--stdio-session`: newline-delimited JSON requests/responses; raw response objects match the existing host desktop helper protocol. Supported commands: `status`, `select_display`, `frame`, `action`, `stop`, and `accessibility`.
- No arguments: compatibility mode, one JSON request read to EOF and one response.
- `--video`: connect to the service and write binary CPV1 packets to stdout. Diagnostics go to stderr.
- `--status`: single JSON status response, including live video capability.

GStreamer retains only the latest JPEG and H.264 access units. H.264 is advertised only when `x264enc` and `h264parse` exist. Every transmitted access unit is an IDR frame, so obsolete frames can be dropped safely. The producer emits `CPV1`, two big-endian uint32 lengths, a JSON header, and AVCC H.264 payload. Headers contain selected display ID, encoded dimensions, input desktop dimensions, geometry version, monotonic timestamp in microseconds, and base64 SPS/PPS. Encoded video dimensions are capped at 4096 per axis; packet payloads at 8 MiB. There is no audio stream.

## Verification

```sh
python3 -m unittest discover -s helpers/linux/tests -v
```

The current 26 desktop contracts pass on macOS with mocked platform interfaces; the earlier 20-test checkpoint also passed in a Linux container. Target acceptance must additionally verify cancel/deny, multi-monitor grant/selection/removal, lock/unlock, revoke while typing, portal restart, monitor scaling/resolution changes, actual AT-SPI inspection, daemon restart and token rotation, missing encoder plugins, and H.264 decode on both Apple clients. Encoded capture-size changes close the session and require local reauthorization. Logical scaling changes without encoded-size changes depend on compositor stream/session updates and must be verified on target GNOME versions.

Implementation references: [XDG RemoteDesktop](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html), [XDG ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html), [PipeWire GStreamer source](https://github.com/PipeWire/pipewire/blob/master/src/gst/gstpipewiresrc.c), [GStreamer appsink](https://gstreamer.freedesktop.org/documentation/app/appsink.html).
