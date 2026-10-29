#!/usr/bin/env python3
"""Install an autostart worker for the invoking OS user, without passwords/root.

The machine broker is installed separately by an administrator. This command
never creates users, changes WSL, or grants screen/input permissions.
"""
import argparse
import json
import os
import pathlib
import plistlib
import re
import subprocess
import sys
import xml.etree.ElementTree as ET


def systemd_quote(value):
    return json.dumps(str(value), ensure_ascii=False).replace("%", "%%")


def systemd_unit(arguments, environment):
    env = "\n".join("Environment=" + systemd_quote(f"{key}={value}") for key, value in environment.items())
    return "\n".join(["[Unit]", "Description=Control Plane OS-user worker", "After=default.target", "", "[Service]", "Type=simple",
        "ExecStart=" + " ".join(systemd_quote(arg) for arg in arguments), env, "Restart=on-failure", "RestartSec=2", "TimeoutStopSec=5", "KillMode=control-group", "UMask=0077", "", "[Install]", "WantedBy=default.target", ""])


def task_xml(arguments, sid):
    namespace = "http://schemas.microsoft.com/windows/2004/02/mit/task"
    ET.register_namespace("", namespace)
    def element(parent, name, text=None, **attributes):
        node = ET.SubElement(parent, "{" + namespace + "}" + name, attributes)
        node.text = text
        return node
    root = ET.Element("{" + namespace + "}Task", {"version": "1.4"})
    trigger = element(element(root, "Triggers"), "LogonTrigger")
    element(trigger, "Enabled", "true")
    element(trigger, "UserId", sid)
    principal = element(element(root, "Principals"), "Principal", id="Worker")
    element(principal, "UserId", sid)
    element(principal, "LogonType", "InteractiveToken")
    element(principal, "RunLevel", "LeastPrivilege")
    settings = element(root, "Settings")
    for key, value in {"MultipleInstancesPolicy": "IgnoreNew", "DisallowStartIfOnBatteries": "false", "StopIfGoingOnBatteries": "false", "ExecutionTimeLimit": "PT0S", "StartWhenAvailable": "true"}.items():
        element(settings, key, value)
    action = element(element(root, "Actions", Context="Worker"), "Exec")
    element(action, "Command", arguments[0])
    element(action, "Arguments", subprocess.list2cmdline(arguments[1:]))
    return ET.tostring(root, encoding="unicode", xml_declaration=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--account", required=True)
    parser.add_argument("--name")
    parser.add_argument("--data", required=True)
    parser.add_argument("--workspace-root", action="append", required=True)
    parser.add_argument("--broker-url")
    parser.add_argument("--desktop-helper")
    parser.add_argument("--video-helper")
    parser.add_argument("--broker-uid", default="0")
    parser.add_argument("--broker-sid", default="S-1-5-18")
    parser.add_argument("--print-only", action="store_true", help="Print generated service configuration without installing it")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", args.account):
        parser.error("account must contain 1–64 letters, digits, hyphens or underscores")
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        parser.error("Run as the member's OS user, not root")
    binary = pathlib.Path(args.binary).resolve(strict=True)
    data = pathlib.Path(args.data).expanduser().resolve()
    roots = [str(pathlib.Path(root).expanduser().resolve(strict=True)) for root in args.workspace_root]
    command = [str(binary), "--data", str(data), "serve", "--name", args.name or args.account]
    for root in roots:
        command += ["--workspace-root", root]
    for flag, value in (("--computer-tools-url", args.broker_url), ("--desktop-helper", args.desktop_helper), ("--video-helper", args.video_helper)):
        if value:
            command += [flag, value]
    if sys.platform == "win32":
        pipe = r"\\.\pipe\control-plane-" + args.account
        transport = ["--ipc-pipe", pipe]
        command += transport + ["--broker-sid", args.broker_sid]
    else:
        transport = ["--ipc-socket", str(data / "ipc" / "worker.sock")]
        command += transport + ["--broker-uid", args.broker_uid]
    environment = {"PATH": os.pathsep.join([str(binary.parent), str(pathlib.Path.home() / ".local/bin"), "/opt/homebrew/bin", "/usr/local/bin", os.environ.get("PATH", "/usr/bin:/bin")])}
    for key, value in (("CONTROL_PLANE_URL", args.broker_url), ("CONTROL_PLANE_DESKTOP_HELPER", args.desktop_helper), ("CONTROL_PLANE_VIDEO_HELPER", args.video_helper)):
        if value:
            environment[key] = value
    if sys.platform == "darwin":
        name = "dev.controlplane.worker." + args.account
        path = pathlib.Path.home() / "Library/LaunchAgents" / (name + ".plist")
        content = plistlib.dumps({"Label": name, "ProgramArguments": command, "EnvironmentVariables": environment, "RunAtLoad": True, "KeepAlive": True, "WorkingDirectory": str(data), "StandardOutPath": str(data / "worker.log"), "StandardErrorPath": str(data / "worker-error.log")})
        apply = [["launchctl", "bootstrap", f"gui/{os.getuid()}", str(path)]]
    elif sys.platform.startswith("linux"):
        name = "control-plane-worker-" + args.account + ".service"
        path = pathlib.Path.home() / ".config/systemd/user" / name
        content = systemd_unit(command, environment).encode()
        apply = [["systemctl", "--user", "daemon-reload"], ["systemctl", "--user", "enable", "--now", name]]
    elif sys.platform == "win32":
        sid = subprocess.check_output(["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value"], text=True).strip()
        if not re.fullmatch(r"S-1-[0-9-]+", sid):
            parser.error("Could not determine the current Windows account SID")
        path = data / "worker-task.xml"
        # Do not put provider API keys in task arguments or XML. WSL/provider
        # credentials are read inside the selected user account at runtime.
        content = task_xml(command, sid).encode("utf-16")
        task_name = "ControlPlane-Worker-" + args.account
        apply = [["schtasks", "/Create", "/TN", task_name, "/XML", str(path), "/F"], ["schtasks", "/Run", "/TN", task_name]]
    else:
        parser.error("Unsupported worker installation OS")
    if args.print_only:
        print(content.decode("utf-16" if sys.platform == "win32" else "utf-8"))
        return
    if path.exists():
        parser.error(f"{path} already exists; review and update the existing service explicitly")
    data.mkdir(parents=True, exist_ok=True, mode=0o700)
    if sys.platform != "win32":
        data.chmod(0o700)
        (data / "ipc").mkdir(mode=0o700, exist_ok=True)
        (data / "ipc").chmod(0o700)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    if sys.platform != "win32":
        path.chmod(0o600)
    for action in apply:
        subprocess.run(action, check=True)
    info = [str(binary), "--data", str(data), "worker-info", "--account", args.account, "--name", args.name or args.account] + transport
    print("Worker registered for this OS identity. Give this JSON to the broker administrator:")
    subprocess.run(info, check=True)


if __name__ == "__main__":
    main()
