#!/usr/bin/env python3
"""Run iOS XCTest/UI tests on an owned simulator or an explicit paired phone."""
import argparse
from contextlib import nullcontext
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import tempfile
import uuid
from host_fixture import ROOT, native_host

def run(command, **kwargs):
    return subprocess.run(command, check=True, timeout=1800, **kwargs)

def inventory(command):
    return json.loads(subprocess.check_output(command, timeout=30))

def simulator():
    runtimes = inventory(["xcrun", "simctl", "list", "runtimes", "-j"])["runtimes"]
    available = [value for value in runtimes if value.get("isAvailable") and value["name"].startswith("iOS ")]
    if not available:
        raise RuntimeError("Install an iOS Simulator runtime in Xcode before testing")
    runtime = max(available, key=lambda item: tuple(int(v) for v in item["version"].split(".")))
    types = inventory(["xcrun", "simctl", "list", "devicetypes", "-j"])["devicetypes"]
    kind = next((value for value in types if value["name"] == "iPhone 16 Plus"), None)
    if kind is None:
        raise RuntimeError("The installed Xcode must provide the iPhone 16 Plus simulator type")
    return subprocess.check_output(["xcrun", "simctl", "create", "ControlPlane Acceptance "
        + str(uuid.uuid4())[:8], kind["identifier"], runtime["identifier"]], text=True, timeout=30).strip()

def inject_fixture(source, values, require_unit=True):
    document = plistlib.loads(source.read_bytes())
    targets = [target for config in document.get("TestConfigurations", [])
        for target in config.get("TestTargets", [])]
    if not targets:
        targets = [value for value in document.values() if isinstance(value, dict) and "TestBundlePath" in value]
    unit = [target for target in targets if target.get("BlueprintName") == "ControlPlaneTests"]
    if require_unit and len(unit) != 1:
        raise RuntimeError("Generated test run must contain exactly one ControlPlaneTests target")
    if values and unit:
        unit[0].setdefault("EnvironmentVariables", {}).update(values)
    if values:
        for target in targets:
            if target.get("BlueprintName") == "ControlPlaneUITests" and values["CONTROL_PLANE_TEST_URL"].startswith("http://127.0.0.1:"):
                target.setdefault("EnvironmentVariables", {}).update({
                    "CONTROL_PLANE_UI_HOST_URL": values["CONTROL_PLANE_TEST_URL"],
                    "CONTROL_PLANE_UI_HOST_TOKEN": values["CONTROL_PLANE_TEST_TOKEN"],
                    "CONTROL_PLANE_UI_SESSION_ID": values["CONTROL_PLANE_TEST_SESSION_ID"]})
    output = source.with_name(source.stem + ".fixture.xctestrun")
    output.write_bytes(plistlib.dumps(document))
    os.chmod(output, 0o600)
    return output

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group()
    target.add_argument("--device", help="Exact physical-device UDID; never chooses a different phone")
    target.add_argument("--simulator", help="Explicit simulator UDID; otherwise create and clean up a dedicated simulator")
    parser.add_argument("--team", default=os.environ.get("IOS_TEAM_ID", ""))
    parser.add_argument("--result-path", type=Path, default=ROOT / "build/ios-acceptance/result.xcresult")
    parser.add_argument("--derived-data", type=Path)
    parser.add_argument("--only-ui", action="store_true", help="Run only UI acceptance after an existing logic-test pass")
    parser.add_argument("--host-ip", default=os.environ.get("IOS_TEST_HOST_IP"), help="Private Mac IPv4 reachable from the phone; enables real mTLS host acceptance")
    parser.add_argument("--preview-host", action="store_true", help="Supply a disposable host during UI-only simulator review")
    args = parser.parse_args()
    result = args.result_path.resolve()
    if result.exists():
        parser.error("Result path already exists; choose a fresh path to preserve earlier evidence")
    owned = None
    try:
        if args.device:
            if not args.team:
                parser.error("Physical testing needs --team or IOS_TEAM_ID for normal Apple development signing")
            with tempfile.TemporaryDirectory(prefix="control-plane-device-list-") as directory:
                path = Path(directory) / "devices.json"
                run(["xcrun", "devicectl", "list", "devices", "--timeout", "15", "--json-output", str(path)], stdout=subprocess.DEVNULL)
                devices = json.loads(path.read_text())["result"]["devices"]
            device = next((item for item in devices if item.get("hardwareProperties", {}).get("udid") == args.device), None)
            if device is None or device.get("connectionProperties", {}).get("tunnelState") == "unavailable":
                raise RuntimeError("The selected paired iPhone is unavailable; connect and unlock it in Xcode")
            destination = "platform=iOS,id=" + args.device
        else:
            identifier = args.simulator or simulator()
            if not args.simulator:
                owned = identifier
                run(["xcrun", "simctl", "boot", identifier], stdout=subprocess.DEVNULL)
            run(["xcrun", "simctl", "bootstatus", identifier, "-b"])
            destination = "platform=iOS Simulator,id=" + identifier
        run(["python3", str(ROOT / "apple/generate_project.py")])
        derived = (args.derived_data or ROOT / "apple/.build" / ("device-acceptance" if args.device else "simulator-acceptance")).resolve()
        command = ["xcodebuild", "-project", str(ROOT / "apple/ControlPlane.xcodeproj"),
            "-scheme", "ControlPlane-iOSTests", "-configuration", "Debug", "-skipPackagePluginValidation",
            "-destination", destination, "-destination-timeout", "60", "-derivedDataPath", str(derived),
            "CONTROL_PLANE_BUNDLE_IDENTIFIER=dev.controlplane.acceptance", "ONLY_ACTIVE_ARCH=YES"]
        if args.device:
            command += ["-allowProvisioningUpdates", "-allowProvisioningDeviceRegistration", "DEVELOPMENT_TEAM=" + args.team]
        else:
            command += ["CODE_SIGNING_ALLOWED=YES", "CODE_SIGN_IDENTITY=-"]
        run(command + (["-only-testing:ControlPlaneUITests"] if args.only_ui else []) + ["build-for-testing"])
        platform = "iphoneos" if args.device else "iphonesimulator"
        files = [value for value in (derived / "Build/Products").glob("*.xctestrun")
            if platform in value.name and ".fixture." not in value.name]
        if not files:
            raise RuntimeError("Xcode did not produce an iOS test-run manifest")
        source = max(files, key=lambda value: value.stat().st_mtime)
        result.parent.mkdir(parents=True, exist_ok=True)
        with (nullcontext({}) if args.only_ui and not args.preview_host or args.device and not args.host_ip else native_host(args.host_ip, preview=args.preview_host)) as fixture:
            manifest = inject_fixture(source, fixture, require_unit=not args.only_ui)
            try:
                run(["xcodebuild", "test-without-building", "-xctestrun", str(manifest),
                    "-destination", destination, "-destination-timeout", "60", "-parallel-testing-enabled", "NO",
                    "-collect-test-diagnostics", "never", "-test-timeouts-enabled", "YES",
                    "-default-test-execution-time-allowance", "120", "-maximum-test-execution-time-allowance", "180",
                    "-resultBundlePath", str(result)] + (["-only-testing:ControlPlaneUITests"] if args.only_ui else []))
            finally:
                manifest.unlink(missing_ok=True)
        print("PASS: iOS tests completed; result bundle:", result)
    finally:
        if owned:
            subprocess.run(["xcrun", "simctl", "shutdown", owned], timeout=30, check=False)
            subprocess.run(["xcrun", "simctl", "delete", owned], timeout=30, check=False)

if __name__ == "__main__":
    def cancelled(*_):
        raise SystemExit(143)
    signal.signal(signal.SIGTERM, cancelled)
    main()
