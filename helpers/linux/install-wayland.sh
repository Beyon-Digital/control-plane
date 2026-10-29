#!/bin/sh
set -eu
case "$(uname -s)" in Linux) ;; *) echo "Linux user-session installer only" >&2; exit 1;; esac
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
install_dir="$HOME/.local/share/control-plane"
mkdir -p "$install_dir/linux" "$HOME/.config/systemd/user"
chmod 700 "$install_dir"
cp "$source_dir"/wayland_*.py "$source_dir/requirements.txt" "$install_dir/linux/"
python3 -m venv --system-site-packages "$install_dir/venv"
"$install_dir/venv/bin/python" -m pip install --requirement "$install_dir/linux/requirements.txt"
cp "$source_dir/control-plane-wayland.service" "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
# Import graphical variables from the local session; do not start this service
# from an SSH session with a different DBus or Wayland environment.
systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP
systemctl --user enable --now control-plane-wayland.service
echo "Next authorize locally: $install_dir/venv/bin/python $install_dir/linux/wayland_helper.py --authorize"
