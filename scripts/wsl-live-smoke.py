#!/usr/bin/env python3
"""Actual Windows-to-WSL2 workspace routing and Linux process tests; no inference."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request
import uuid

STAGE = r'''
import base64,json,os,pathlib,pwd,shutil,subprocess,sys,tempfile
v=json.load(sys.stdin)
assert os.geteuid()!=0 and pwd.getpwuid(os.geteuid()).pw_name==v['user']
root=pathlib.Path(tempfile.mkdtemp(prefix='control-plane-ci-'+v['marker']+'-',dir='/tmp'))
try:
 root.chmod(0o700);(root/'.marker').write_text(v['marker'])
 assert set(v['files'])=={'wsl_bridge.py','wsl_transfer.py','wsl_bridge_test.py'}
 for name,data in v['files'].items():
  raw=base64.b64decode(data,validate=True);assert len(raw)<1024*1024
  fd=os.open(root/name,os.O_CREAT|os.O_EXCL|os.O_WRONLY|os.O_NOFOLLOW,0o600)
  with os.fdopen(fd,'wb')as f:f.write(raw)
 project=root/'project';project.mkdir(mode=0o700);(project/'identity.txt').write_text(v['marker'])
 subprocess.run(['git','-c','init.defaultBranch=main','init','--quiet',str(project)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15,env=dict(os.environ,GIT_CONFIG_GLOBAL='/dev/null',GIT_CONFIG_NOSYSTEM='1'))
 print(json.dumps({'root':str(root),'project':str(project),'bridge':str(root/'wsl_bridge.py')}))
except BaseException:shutil.rmtree(root);raise
'''
PROCESS_TESTS = r'''
import importlib.util,io,json,pathlib,sys,unittest
root=pathlib.Path(json.load(sys.stdin)['root'])
spec=importlib.util.spec_from_file_location('ci_bridge',root/'wsl_bridge_test.py')
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
suite=unittest.defaultTestLoader.loadTestsFromModule(module)
result=unittest.TextTestRunner(stream=io.StringIO()).run(suite)
passed=result.wasSuccessful()and result.testsRun==3 and not result.skipped
print(json.dumps({'passed':passed,'tests':result.testsRun}));sys.exit(0 if passed else 1)
'''
CLEANUP = r'''
import json,os,pathlib,pwd,shutil,stat,sys
v=json.load(sys.stdin);assert os.geteuid()!=0 and pwd.getpwuid(os.geteuid()).pw_name==v['user']
prefix='control-plane-ci-'+v['marker']+'-'
for root in pathlib.Path('/tmp').iterdir():
 if not root.name.startswith(prefix):continue
 meta=root.lstat();assert stat.S_ISDIR(meta.st_mode)and not stat.S_ISLNK(meta.st_mode)
 assert meta.st_uid==os.geteuid()and not stat.S_IMODE(meta.st_mode)&0o077
 marker=root/'.marker';m=marker.lstat();assert stat.S_ISREG(m.st_mode)and m.st_uid==os.geteuid()
 assert marker.read_text()==v['marker'];shutil.rmtree(root)
print(json.dumps({'cleaned':True}))
'''

def require(condition):
    if not condition:
        raise RuntimeError("Acceptance gate failed")

def command(argv, data=None, env=None, timeout=40):
    result = subprocess.run(argv, input=None if data is None else json.dumps(data).encode(),
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, timeout=timeout, check=True)
    require(len(result.stdout) <= 2 * 1024 * 1024)
    return result.stdout

def listing(raw):
    return raw.decode("utf-16" if raw.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-16-le" if b"\x00" in raw else "utf-8-sig").replace("\x00", "")

def wsl2_installed(text, distribution):
    matches = []
    for row in text.splitlines():
        row = row.strip().lstrip("*").lstrip()
        if row[:len(distribution)].casefold() != distribution.casefold():
            continue
        tail = row[len(distribution):]
        if tail[:1].isspace():
            version = re.search(r"\s+(\d+)\s*$", tail)
            if version:
                matches.append(version[1])
    return matches == ["2"]

def free_ports():
    sockets = [socket.socket(), socket.socket()]
    try:
        for item in sockets:
            item.bind(("127.0.0.1", 0))
        return tuple(item.getsockname()[1] for item in sockets)
    finally:
        for item in sockets:
            item.close()

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise RuntimeError("Redirect refused")

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--distribution", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--report-path", required=True, type=Path)
    args = parser.parse_args()
    names = ("windows_wsl2", "linux_stage", "native_pairing", "target_discovery", "session_target",
        "workspace_read", "workspace_write", "bridge_process_tests", "native_health_after_tests", "cleanup")
    gates = dict.fromkeys(names, False)
    marker, worker, directory, staged = str(uuid.uuid4()), None, None, False
    executable = shutil.which("wsl.exe")
    def wsl(script, data, timeout=60):
        return json.loads(command([executable, "--distribution", args.distribution, "--user", args.user,
            "--exec", "python3", "-c", script], data, timeout=timeout))
    try:
        require(os.name == "nt" and executable and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_. -]{0,127}", args.distribution))
        require(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]{0,63}", args.user) and args.user != "root")
        require(wsl2_installed(listing(command([executable, "--list", "--verbose"])), args.distribution))
        gates["windows_wsl2"] = True
        root = Path(__file__).resolve().parents[1]
        files = {name: base64.b64encode((root / "scripts" / name).read_bytes()).decode()
            for name in ("wsl_bridge.py", "wsl_transfer.py", "wsl_bridge_test.py")}
        staged = True
        stage = wsl(STAGE, {"files": files, "marker": marker, "user": args.user})
        require(stage["root"].startswith("/tmp/control-plane-ci-" + marker + "-")
            and "/" not in stage["root"][5:] and stage["project"] == stage["root"] + "/project"
            and stage["bridge"] == stage["root"] + "/wsl_bridge.py")
        gates["linux_stage"] = True
        directory = tempfile.TemporaryDirectory(prefix="control-plane-wsl-ci-")
        native = Path(directory.name)
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith("CONTROL_PLANE_")}
        env["CONTROL_PLANE_WSL_CONFIG"] = json.dumps({args.distribution: {"user": args.user, "roots": [stage["project"]], "bridge": stage["bridge"]}})
        cli = [str(root / "target/debug/control-plane-host.exe"), "--data", str(native / "data")]
        secret = command(cli + ["enroll"], env=env).decode().strip()
        http, grpc = free_ports()
        worker = subprocess.Popen(cli + ["serve", "--listen", f"127.0.0.1:{http}", "--grpc-listen", f"127.0.0.1:{grpc}", "--workspace-root", str(native)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        def request(path, body=None, token=""):
            req = urllib.request.Request(f"http://127.0.0.1:{http}" + path, data=None if body is None else json.dumps(body).encode(),
                headers={"Content-Type": "application/json", "X-Device-Token": token})
            with opener.open(req, timeout=135) as response:
                data = response.read(2 * 1024 * 1024 + 1)
            require(len(data) <= 2 * 1024 * 1024)
            return json.loads(data)
        for _ in range(100):
            require(worker.poll() is None)
            try:
                request("/healthz")
                break
            except OSError:
                time.sleep(.1)
        token = request("/v1/pair", {"secret": secret, "device_name": "WSL CI"})["token"]
        gates["native_pairing"] = True
        target = {"kind": "wsl", "distribution": args.distribution}
        require(any(item.get("execution_target") == target for item in request("/v1/execution-targets", token=token)))
        gates["target_discovery"] = True
        session = request("/v1/sessions", {"provider": "codex", "workspace": stage["project"], "title": "WSL CI", "trusted": False,
            "execution_target": target, "request_id": str(uuid.uuid4())}, token)
        require(session["execution_target"] == target and session["trusted"] is False and session["state"] != "running")
        gates["session_target"] = True
        def read(path):
            return request("/v1/workspace/read?" + urllib.parse.urlencode({"session_id": session["id"], "path": path}), token=token)
        require(read("identity.txt")["content"] == marker)
        gates["workspace_read"] = True
        request("/v1/workspace/write", {"session_id": session["id"], "path": "created.txt", "content": marker}, token)
        require(read("created.txt")["content"] == marker)
        gates["workspace_write"] = True
        require(wsl(PROCESS_TESTS, {"root": stage["root"]}, 110)["passed"] is True)
        gates["bridge_process_tests"] = True
        require(worker.poll() is None and request("/healthz")["status"] == "ok")
        gates["native_health_after_tests"] = True
    except Exception:
        pass  # The sanitized gate report is the failure evidence; never log credentials or process output.
    finally:
        cleaned = True
        if worker is not None and worker.poll() is None:
            try:
                worker.terminate()
                worker.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    worker.kill()
                    worker.wait(timeout=5)
                except (OSError, subprocess.TimeoutExpired):
                    cleaned = False
            except OSError:
                cleaned = False
        if staged:
            try:
                require(wsl(CLEANUP, {"marker": marker, "user": args.user})["cleaned"] is True)
            except Exception:
                cleaned = False
        if directory:
            try:
                directory.cleanup()
            except OSError:
                cleaned = False
        gates["cleanup"] = cleaned
    passed = all(gates.values())
    report = {"schema": 1, "passed": passed, "gates": gates, "untested": ["Provider inference and tool approval",
        "Interactive ConPTY and terminal UI", "Desktop input and lock transitions", "WSL OS-user isolation"]}
    args.report_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", dir=args.report_path.parent, delete=False, encoding="utf-8") as output:
        temporary = Path(output.name)
        json.dump(report, output, indent=2)
    try:
        os.replace(temporary, args.report_path)
    finally:
        temporary.unlink(missing_ok=True)
    print("Windows/WSL2 acceptance:", "PASS" if passed else "FAIL")
    return 0 if passed else 1

if __name__ == "__main__":
    raise SystemExit(main())
