#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo"
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
: "${PACKAGE_MAINTAINER:?Real release maintainer required}"
: "${CONTROL_PLANE_UPDATE_PUBLIC_KEY:?Pinned publisher key required}"
: "${CONTROL_PLANE_UPDATE_FEED:?HTTPS feed required}"
: "${CONTROL_PLANE_UPDATE_SIGNING_KEY:?Existing signing key path required}"
: "${CONTROL_PLANE_ARTIFACT_BASE_URL:?HTTPS artifact base URL required}"
: "${BUILD_NUMBER:?Monotonic release sequence required}"
: "${NODE_RUNTIME_PATH:?Approved Node runtime path required}"
: "${NODE_SHA256:?Approved Node SHA-256 required}"
version=$(python3 -c 'import tomllib;print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')
case "$BUILD_NUMBER" in ''|*[!0-9]*) exit 1;; esac
python3 - "$PACKAGE_MAINTAINER" <<'PY'
import sys
if '\n'in sys.argv[1]or'\r'in sys.argv[1]:raise SystemExit('Maintainer must be one line')
PY
cargo test --workspace --locked
python3 -m unittest discover -s helpers/linux/tests -v
cargo build --release --locked
root="$repo/build/release-linux/package"
test ! -e "$root"
mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/lib/control-plane/linux" "$root/usr/lib/systemd/user" "$root/usr/share/doc/control-plane-host" "$repo/dist"
for name in control-plane-host control-plane-broker control-plane-computer-tools control-plane-update;do install -m755 "target/release/$name" "$root/usr/bin/$name";done
find helpers/linux -maxdepth 1 -name '*.py' -exec install -m644 '{}' "$root/usr/lib/control-plane/linux/" ';'
python3 scripts/release/stage_runtime.py --platform linux --destination "$root/usr/lib/control-plane"
cat > "$root/DEBIAN/control" <<CONFIG
Package: control-plane-host
Version: $version
Section: devel
Priority: optional
Architecture: amd64
Maintainer: $PACKAGE_MAINTAINER
Depends: libc6 (>= 2.39), libstdc++6, libgcc-s1, python3 (>= 3.12), python3-gi, python3-dbus-next, gir1.2-atspi-2.0, gir1.2-gstreamer-1.0, gir1.2-gst-plugins-base-1.0, gstreamer1.0-pipewire, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-plugins-ugly, xdg-desktop-portal, xdg-desktop-portal-gnome, xdg-utils
Description: Private-network coding agent host and per-user desktop bridge
 Coding providers are authenticated separately by each OS user.
CONFIG
cat > "$root/usr/lib/systemd/user/control-plane-wayland.service" <<'CONFIG'
[Unit]
Description=Control Plane Wayland desktop portal
PartOf=graphical-session.target
After=graphical-session.target
[Service]
Type=simple
ExecStart=/usr/bin/python3 /usr/lib/control-plane/linux/wayland_helper.py --daemon
Restart=on-failure
RestartSec=3
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
[Install]
WantedBy=graphical-session.target
CONFIG
cat > "$root/usr/lib/systemd/user/control-plane-worker.service" <<'CONFIG'
[Unit]
Description=Control Plane OS-user worker
ConditionPathExists=%h/.config/control-plane/worker.env
[Service]
Type=simple
EnvironmentFile=%h/.config/control-plane/worker.env
Environment=CONTROL_PLANE_NODE_BIN=/usr/lib/control-plane/node
Environment=CONTROL_PLANE_CLAUDE_SIDECAR=/usr/lib/control-plane/claude/dist/index.mjs
ExecStartPre=/usr/bin/install -d -m0700 %h/.local/state/control-plane/ipc
ExecStart=/usr/bin/control-plane-host --data %h/.local/state/control-plane serve --ipc-socket %h/.local/state/control-plane/ipc/worker.sock --workspace-root ${CONTROL_PLANE_WORKSPACE_ROOT}
Restart=on-failure
RestartSec=3
UMask=0077
KillMode=control-group
[Install]
WantedBy=default.target
CONFIG
cat > "$root/DEBIAN/preinst" <<'SCRIPT'
#!/bin/sh
set -eu
if [ "${1:-}" = upgrade ]; then
 for executable in /proc/[0-9]*/exe; do
  target=$(readlink "$executable" 2>/dev/null || true)
  case "$target" in /usr/bin/control-plane-host|/usr/bin/control-plane-broker) echo 'Drain all installed Control Plane workers and the broker before upgrading.' >&2;exit 1;; esac
 done
fi
SCRIPT
chmod 755 "$root/DEBIAN/preinst"
cp docs/wayland.md docs/broker.md "$root/usr/share/doc/control-plane-host/"
package="$repo/dist/ControlPlane-$version-linux-x86_64.deb"
test ! -e "$package"
dpkg-deb --build --root-owner-group "$root" "$package"
dpkg-deb --info "$package"
python3 scripts/release_manifest.py --artifact "$package" --url "${CONTROL_PLANE_ARTIFACT_BASE_URL%/}/$(basename "$package")" --feed-url "$CONTROL_PLANE_UPDATE_FEED" --version "$version" --platform linux-x86_64 --sequence "$BUILD_NUMBER" --signing-key "$CONTROL_PLANE_UPDATE_SIGNING_KEY" --output "$repo/dist/stable-linux-x86_64-$BUILD_NUMBER.json"
echo 'Debian package and publisher-signed manifest prepared. No service enabled or publishing performed.'
